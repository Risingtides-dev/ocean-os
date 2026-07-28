import Combine
import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

/// Foreground-only native Realtime coordinator shared by the Watch and iPhone
/// shells. Every asynchronous path is generation-checked so a stopped session
/// cannot reacquire the microphone or publish stale UI.
@MainActor
public final class OceanBuddyRealtimeController: ObservableObject {
    @Published public private(set) var stage: BuddyRealtimeStage = .off
    @Published public private(set) var microphoneLevel: Float = 0
    @Published public private(set) var transcript: String = ""
    @Published public private(set) var card: BuddyRealtimeCard?
    @Published public private(set) var status: String = "Ready"
    @Published public private(set) var errorMessage: String?

    private let audio = BuddyRealtimeAudioPipeline()
    private var generation: UInt64 = 0
    private var socketSession: URLSession?
    private var socket: URLSessionWebSocketTask?
    private var connectTask: Task<Void, Never>?
    private var receiveTask: Task<Void, Never>?
    private var audioSendTask: Task<Void, Never>?
    private var audioContinuation: AsyncStream<Data>.Continuation?
    private var reducer = BuddyRealtimeEventReducer()
    private var toolBroker: BuddyRealtimeToolBroker?
    private var configuredSession = false
    private var audioStarted = false
    private var activeAssistantItemID: String?
    private var fulfilledCallIDs: Set<String> = []
    private var fulfilledCallOrder: [String] = []
    private var transcriptBuffer = BuddyRealtimeTranscriptBuffer()
    private var toolQuota = BuddyRealtimeToolQuotaState()

    public init() {}

    public func start(
        baseURL: URL,
        sessionID: String? = nil,
        model: String? = nil,
        endpointSecurityMode: BuddyEndpointSecurityMode = .release,
        allowInsecureLocalNetwork: Bool = false
    ) {
        guard stage == .off || stage == .failed else { return }
        guard BuddyDaemonEndpointPolicy.allows(
            baseURL,
            mode: endpointSecurityMode,
            allowInsecureLocalNetwork: allowInsecureLocalNetwork
        ) else {
            stage = .failed
            status = "Voice unavailable"
            errorMessage = "Use HTTPS (including Tailscale HTTPS) or loopback. Debug builds can explicitly allow local HTTP in Connection settings."
            return
        }
        let canonicalSessionID: String?
        do {
            canonicalSessionID = try BuddyPairingCode.validatedSessionID(sessionID)
        } catch {
            stage = .failed
            status = "Voice unavailable"
            errorMessage = "Ocean session IDs must be UUIDs. Pair again or clear the optional session."
            return
        }
        teardownTransport()
        generation &+= 1
        let attempt = generation
        stage = .connecting
        status = "Preparing microphone…"
        errorMessage = nil
        transcript = ""
        card = nil
        transcriptBuffer.reset()
        reducer.reset()
        configuredSession = false
        audioStarted = false
        activeAssistantItemID = nil
        fulfilledCallIDs.removeAll(keepingCapacity: true)
        fulfilledCallOrder.removeAll(keepingCapacity: true)
        toolQuota.reset()

        connectTask = Task { [weak self] in
            do {
                let microphoneAllowed = await BuddyRealtimeAudioPipeline.requestMicrophonePermission()
                guard let self, self.isCurrent(attempt) else { return }
                guard microphoneAllowed else {
                    throw BuddyRealtimeAudioError.microphonePermissionDenied
                }
                self.status = "Connecting to Ocean…"
                let secret = try await HTTPBuddyRealtimeSecretClient(
                    baseURL: baseURL,
                    endpointSecurityMode: endpointSecurityMode,
                    allowInsecureLocalNetwork: allowInsecureLocalNetwork
                ).mint(sessionID: canonicalSessionID, model: model)
                if secret.expires(within: 15) {
                    throw BuddyRealtimeSecretError.expiredCredential
                }
                guard self.isCurrent(attempt) else { return }
                try self.openSocket(
                    secret: secret,
                    baseURL: baseURL,
                    sessionID: canonicalSessionID,
                    generation: attempt
                )
            } catch {
                guard let self else { return }
                self.fail(error.localizedDescription, generation: attempt)
            }
        }
    }

    public func stop() {
        generation &+= 1
        teardownTransport()
        stage = .off
        status = "Ready"
        errorMessage = nil
        microphoneLevel = 0
    }

    private func openSocket(
        secret: BuddyRealtimeSecret,
        baseURL: URL,
        sessionID: String?,
        generation: UInt64
    ) throws {
        guard isCurrent(generation) else { return }
        var components = URLComponents(string: "wss://api.openai.com/v1/realtime")
        components?.queryItems = [URLQueryItem(name: "model", value: secret.model)]
        guard let url = components?.url else {
            throw URLError(.badURL)
        }

        var request = URLRequest(url: url)
        request.timeoutInterval = 30
        request.setValue("Bearer \(secret.clientSecret)", forHTTPHeaderField: "Authorization")
        request.setValue("application/json", forHTTPHeaderField: "Accept")

        let configuration = URLSessionConfiguration.ephemeral
        configuration.timeoutIntervalForRequest = 30
        configuration.timeoutIntervalForResource = 3_600
        let session = BuddySecureURLSession.make(configuration: configuration)
        let socket = session.webSocketTask(with: request)
        self.socketSession = session
        self.socket = socket
        toolBroker = BuddyRealtimeToolBroker(baseURL: baseURL, sessionID: sessionID)
        socket.resume()

        receiveTask = Task { [weak self, socket] in
            do {
                while !Task.isCancelled {
                    let message = try await socket.receive()
                    guard let self, self.isCurrent(generation) else { return }
                    switch message {
                    case let .string(text):
                        await self.handleServerText(text, generation: generation)
                    case let .data(data):
                        if let text = String(data: data, encoding: .utf8) {
                            await self.handleServerText(text, generation: generation)
                        }
                    @unknown default:
                        continue
                    }
                }
            } catch {
                guard let self else {
                    socket.cancel(with: .goingAway, reason: nil)
                    return
                }
                if self.isCurrent(generation) {
                    self.fail(
                        "Realtime connection ended: \(error.localizedDescription)",
                        generation: generation
                    )
                }
            }
        }
    }

    private func handleServerText(_ text: String, generation: UInt64) async {
        guard isCurrent(generation) else { return }
        let effects = reducer.reduce(text: text)
        var toolCalls: [BuddyRealtimeToolCall] = []
        for effect in effects {
            guard isCurrent(generation) else { return }
            switch effect {
            case .sessionCreated:
                if !configuredSession {
                    configuredSession = true
                    do {
                        try await sendSessionConfiguration(generation: generation)
                    } catch {
                        fail(error.localizedDescription, generation: generation)
                        return
                    }
                }

            case .sessionUpdated:
                if configuredSession, !audioStarted {
                    audioStarted = true
                    do {
                        try startAudio(generation: generation)
                    } catch {
                        fail(error.localizedDescription, generation: generation)
                        return
                    }
                }

            case let .audio(data):
                if !audio.enqueueOutput(data) {
                    fail("Realtime audio playback fell behind.", generation: generation)
                    return
                }

            case let .assistantText(replyID, text, replace):
                updateTranscript(replyID: replyID, text: text, replace: replace)

            case let .assistantAudioItem(itemID):
                activeAssistantItemID = itemID
                audio.beginAssistantResponse()

            case let .interrupted(itemID):
                await handleBargeIn(itemID: itemID, generation: generation)

            case .responseCompleted:
                if !audio.endAssistantResponse() {
                    activeAssistantItemID = nil
                }

            case let .toolCall(call):
                toolCalls.append(call)

            case let .error(message):
                // Provider errors are terminal for a microphone-bearing client.
                // Fail closed rather than leaving a hot mic behind an error UI.
                fail(message, generation: generation)
                return
            }
        }
        if !toolCalls.isEmpty {
            await fulfill(toolCalls, generation: generation)
        }
    }

    private func sendSessionConfiguration(generation: UInt64) async throws {
        let update: [String: Any] = [
            "type": "session.update",
            "session": [
                "type": "realtime",
                "output_modalities": ["audio"],
                "audio": [
                    "input": [
                        "format": ["type": "audio/pcm", "rate": 24_000],
                        "turn_detection": [
                            "type": "semantic_vad",
                            "create_response": true,
                            "interrupt_response": true,
                        ],
                    ],
                    "output": [
                        "format": ["type": "audio/pcm", "rate": 24_000],
                    ],
                ],
            ],
        ]
        try await send(update, generation: generation)
    }

    private func startAudio(generation: UInt64) throws {
        guard isCurrent(generation) else { return }
        var continuation: AsyncStream<Data>.Continuation?
        let stream = AsyncStream<Data>(bufferingPolicy: .bufferingNewest(8)) {
            continuation = $0
        }
        guard let continuation else {
            throw URLError(.cannotCreateFile)
        }
        audioContinuation = continuation
        audioSendTask = Task { [weak self] in
            for await chunk in stream {
                guard let self,
                      self.isCurrent(generation),
                      !Task.isCancelled
                else {
                    return
                }
                do {
                    try await self.send([
                        "type": "input_audio_buffer.append",
                        "audio": chunk.base64EncodedString(),
                    ], generation: generation)
                } catch {
                    self.fail(error.localizedDescription, generation: generation)
                    return
                }
            }
        }

        try audio.start(
            onInput: { [weak self, continuation] chunk, level in
                let yield = continuation.yield(chunk)
                Task { @MainActor [weak self] in
                    guard let self, self.isCurrent(generation) else { return }
                    self.microphoneLevel = level
                    if case .dropped = yield {
                        self.fail("Realtime audio upload fell behind.", generation: generation)
                    }
                }
            },
            onInterruption: { [weak self] in
                Task { @MainActor [weak self] in
                    self?.fail(
                        "Audio was interrupted. Tap Talk to reconnect.",
                        generation: generation
                    )
                }
            }
        )
        stage = .live
        status = "Listening"
    }

    private func handleBargeIn(itemID: String?, generation: UInt64) async {
        guard isCurrent(generation) else { return }
        let playedMilliseconds = audio.interruptPlayback()
        status = "Listening"
        stage = .interrupted
        if let itemID = itemID ?? activeAssistantItemID, playedMilliseconds > 0 {
            try? await send([
                "type": "conversation.item.truncate",
                "item_id": itemID,
                "content_index": 0,
                "audio_end_ms": playedMilliseconds,
            ], generation: generation)
        }
        if isCurrent(generation) {
            stage = .live
        }
    }

    private func fulfill(_ calls: [BuddyRealtimeToolCall], generation: UInt64) async {
        guard let toolBroker, isCurrent(generation) else { return }
        var sentOutput = false

        do {
            for call in calls {
                guard isCurrent(generation), remember(callID: call.callID) else { continue }
                guard toolQuota.consume(call.name) else {
                    fail(
                        "Realtime tool quota reached. Start a new voice chat to use more tools.",
                        generation: generation
                    )
                    return
                }
                let result = await toolBroker.fulfill(call)
                guard isCurrent(generation) else { return }
                if let card = result.card {
                    self.card = card
                }
                try await send([
                    "type": "conversation.item.create",
                    "item": [
                        "type": "function_call_output",
                        "call_id": call.callID,
                        "output": result.output,
                    ],
                ], generation: generation)
                sentOutput = true
            }
            if sentOutput {
                try await send(["type": "response.create"], generation: generation)
            }
        } catch {
            fail(error.localizedDescription, generation: generation)
        }
    }

    private func remember(callID: String) -> Bool {
        guard !fulfilledCallIDs.contains(callID) else { return false }
        fulfilledCallIDs.insert(callID)
        fulfilledCallOrder.append(callID)
        if fulfilledCallOrder.count > 128 {
            let evicted = fulfilledCallOrder.removeFirst()
            fulfilledCallIDs.remove(evicted)
        }
        return true
    }

    private func updateTranscript(replyID: String, text: String, replace: Bool) {
        transcript = transcriptBuffer.update(replyID: replyID, text: text, replace: replace)
    }

    private func send(_ object: [String: Any], generation: UInt64) async throws {
        guard isCurrent(generation), let socket else {
            throw CancellationError()
        }
        let data = try JSONSerialization.data(withJSONObject: object)
        guard let text = String(data: data, encoding: .utf8) else {
            throw URLError(.cannotParseResponse)
        }
        try await socket.send(.string(text))
    }

    private func fail(_ rawMessage: String, generation: UInt64) {
        guard isCurrent(generation) else { return }
        self.generation &+= 1
        teardownTransport()
        let message = rawMessage.contains("no OpenAI Realtime voice credential")
            ? "Ocean voice needs an OpenAI Realtime credential."
            : String(rawMessage.prefix(500))
        errorMessage = message
        status = "Voice unavailable"
        stage = .failed
        microphoneLevel = 0
    }

    private func teardownTransport() {
        connectTask?.cancel()
        receiveTask?.cancel()
        audioSendTask?.cancel()
        connectTask = nil
        receiveTask = nil
        audioSendTask = nil
        audioContinuation?.finish()
        audioContinuation = nil
        audio.stop()
        socket?.cancel(with: .goingAway, reason: nil)
        socket = nil
        socketSession?.invalidateAndCancel()
        socketSession = nil
        toolBroker = nil
        configuredSession = false
        audioStarted = false
        activeAssistantItemID = nil
        fulfilledCallIDs.removeAll(keepingCapacity: true)
        fulfilledCallOrder.removeAll(keepingCapacity: true)
        toolQuota.reset()
        reducer.reset()
    }

    deinit {
        connectTask?.cancel()
        receiveTask?.cancel()
        audioSendTask?.cancel()
        audioContinuation?.finish()
        audio.stop()
        socket?.cancel(with: .goingAway, reason: nil)
        socketSession?.invalidateAndCancel()
    }

    private func isCurrent(_ value: UInt64) -> Bool {
        generation == value
    }
}
