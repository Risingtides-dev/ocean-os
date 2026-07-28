@preconcurrency import AVFoundation
import Foundation

public enum BuddyRealtimeAudioError: Error, LocalizedError {
    case microphonePermissionDenied
    case microphoneUnavailable
    case unsupportedAudioFormat
    case conversionUnavailable

    public var errorDescription: String? {
        switch self {
        case .microphonePermissionDenied:
            "Microphone access is required for an Ocean voice chat."
        case .microphoneUnavailable:
            "The microphone is unavailable."
        case .unsupportedAudioFormat:
            "The microphone audio format is unsupported."
        case .conversionUnavailable:
            "Ocean Buddy could not configure realtime audio."
        }
    }
}

private final class BuddyConversionInput: @unchecked Sendable {
    let buffer: AVAudioPCMBuffer
    var supplied = false

    init(_ buffer: AVAudioPCMBuffer) {
        self.buffer = buffer
    }
}

private final class BuddyPCMConverter: @unchecked Sendable {
    private let converter: AVAudioConverter
    private let outputFormat: AVAudioFormat

    init(inputFormat: AVAudioFormat) throws {
        guard let outputFormat = AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 24_000,
            channels: 1,
            interleaved: false
        ), let converter = AVAudioConverter(from: inputFormat, to: outputFormat) else {
            throw BuddyRealtimeAudioError.conversionUnavailable
        }
        self.converter = converter
        self.outputFormat = outputFormat
    }

    func convert(_ input: AVAudioPCMBuffer) -> Data? {
        let ratio = outputFormat.sampleRate / input.format.sampleRate
        let capacity = AVAudioFrameCount((Double(input.frameLength) * ratio).rounded(.up)) + 16
        guard let output = AVAudioPCMBuffer(pcmFormat: outputFormat, frameCapacity: capacity) else {
            return nil
        }

        let source = BuddyConversionInput(input)
        var conversionError: NSError?
        let status = converter.convert(to: output, error: &conversionError) { _, state in
            if source.supplied {
                state.pointee = .noDataNow
                return nil
            }
            source.supplied = true
            state.pointee = .haveData
            return source.buffer
        }
        guard status != .error,
              conversionError == nil,
              output.frameLength > 0,
              let samples = output.int16ChannelData?[0]
        else {
            return nil
        }
        return Data(bytes: samples, count: Int(output.frameLength) * MemoryLayout<Int16>.size)
    }

    func level(_ input: AVAudioPCMBuffer) -> Float {
        guard input.frameLength > 0,
              let channel = input.floatChannelData?[0]
        else {
            return 0
        }
        var sum: Float = 0
        for index in 0..<Int(input.frameLength) {
            let sample = channel[index]
            sum += sample * sample
        }
        return min(1, sqrt(sum / Float(input.frameLength)) * 4)
    }
}

/// Native foreground audio adapter for iOS/watchOS. It owns the microphone tap
/// and speaker queue and tears both down synchronously when voice ends. Public
/// calls are serialized by the main-actor controller; the audio tap itself must
/// remain nonisolated because AVAudioEngine invokes it on a realtime queue.
public final class BuddyRealtimeAudioPipeline: @unchecked Sendable {
    private var engine: AVAudioEngine?
    private var player: AVAudioPlayerNode?
    private let playbackLock = NSLock()
    private var playbackAccounting = BuddyPlaybackQueueAccounting()
    private var outputFormat: AVAudioFormat?
    private var interruptionObserver: NSObjectProtocol?
    private var tapInstalled = false

    public init() {}

    /// Ask before minting a short-lived credential so the first system prompt
    /// cannot consume the credential or race microphone startup.
    public static func requestMicrophonePermission() async -> Bool {
        #if os(iOS) || os(watchOS)
        return await withCheckedContinuation { continuation in
            AVAudioApplication.requestRecordPermission { granted in
                continuation.resume(returning: granted)
            }
        }
        #else
        return true
        #endif
    }

    public func start(
        onInput: @escaping @Sendable (Data, Float) -> Void,
        onInterruption: @escaping @Sendable () -> Void
    ) throws {
        stop()

        #if os(iOS) || os(watchOS)
        let audioSession = AVAudioSession.sharedInstance()
        #if os(iOS)
        try audioSession.setCategory(
            .playAndRecord,
            mode: .voiceChat,
            options: [.defaultToSpeaker, .allowBluetoothHFP]
        )
        try? audioSession.setPreferredSampleRate(24_000)
        try? audioSession.setPreferredIOBufferDuration(0.02)
        #else
        try audioSession.setCategory(.playAndRecord, mode: .voiceChat)
        #endif
        try audioSession.setActive(true)
        interruptionObserver = NotificationCenter.default.addObserver(
            forName: AVAudioSession.interruptionNotification,
            object: audioSession,
            queue: .main
        ) { notification in
            let raw = notification.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt
            if raw == AVAudioSession.InterruptionType.began.rawValue {
                onInterruption()
            }
        }
        #endif

        let engine = AVAudioEngine()
        let player = AVAudioPlayerNode()
        let input = engine.inputNode
        let inputFormat = input.inputFormat(forBus: 0)
        guard inputFormat.channelCount > 0, inputFormat.sampleRate > 0 else {
            throw BuddyRealtimeAudioError.microphoneUnavailable
        }
        guard let outputFormat = AVAudioFormat(
            commonFormat: .pcmFormatInt16,
            sampleRate: 24_000,
            channels: 1,
            interleaved: false
        ) else {
            throw BuddyRealtimeAudioError.unsupportedAudioFormat
        }
        let converter = try BuddyPCMConverter(inputFormat: inputFormat)

        engine.attach(player)
        engine.connect(player, to: engine.mainMixerNode, format: outputFormat)
        input.installTap(onBus: 0, bufferSize: 2_048, format: inputFormat) { buffer, _ in
            guard let chunk = converter.convert(buffer), !chunk.isEmpty else { return }
            onInput(chunk, converter.level(buffer))
        }
        tapInstalled = true

        engine.prepare()
        try engine.start()
        player.play()

        self.engine = engine
        self.player = player
        self.outputFormat = outputFormat
    }

    public func beginAssistantResponse() {
        playbackLock.lock()
        playbackAccounting.beginResponse()
        playbackLock.unlock()
    }

    /// Returns true while completed-response audio is still waiting to play.
    @discardableResult
    public func endAssistantResponse() -> Bool {
        playbackLock.lock()
        let hasQueuedOutput = playbackAccounting.endResponse()
        playbackLock.unlock()
        return hasQueuedOutput
    }

    @discardableResult
    public func enqueueOutput(_ pcm16: Data) -> Bool {
        guard let player, let outputFormat else { return false }
        let byteCount = pcm16.count - (pcm16.count % MemoryLayout<Int16>.size)
        guard byteCount > 0 else { return false }
        let frames = AVAudioFrameCount(byteCount / MemoryLayout<Int16>.size)
        guard let buffer = AVAudioPCMBuffer(pcmFormat: outputFormat, frameCapacity: frames),
              let destination = buffer.int16ChannelData?[0]
        else {
            return false
        }
        buffer.frameLength = frames
        pcm16.withUnsafeBytes { bytes in
            guard let source = bytes.baseAddress else { return }
            memcpy(destination, source, byteCount)
        }

        playbackLock.lock()
        let reservation = playbackAccounting.reserve(frames: UInt64(frames))
        playbackLock.unlock()
        guard let reservation else { return false }

        player.scheduleBuffer(buffer, completionCallbackType: .dataPlayedBack) { [weak self] _ in
            self?.didPlay(reservation)
        }
        if !player.isPlaying {
            player.play()
        }
        return true
    }

    /// Stop queued assistant audio and return conservative playback backed only
    /// by buffers AVAudioPlayerNode confirmed as played. An in-flight partial
    /// buffer is omitted so truncation can never overstate rendered duration.
    public func interruptPlayback() -> Int {
        playbackLock.lock()
        let playedFrames = playbackAccounting.interrupt()
        playbackLock.unlock()

        player?.stop()
        player?.play()
        return Int((playedFrames * 1_000) / BuddyPlaybackQueueAccounting.sampleRate)
    }

    private func didPlay(_ reservation: BuddyPlaybackQueueAccounting.Reservation) {
        playbackLock.lock()
        playbackAccounting.didPlay(reservation)
        playbackLock.unlock()
    }

    @discardableResult
    private func resetPlaybackAccounting() -> Bool {
        playbackLock.lock()
        let hadQueuedOutput = playbackAccounting.reset()
        playbackLock.unlock()
        return hadQueuedOutput
    }

    public func stop() {
        if let engine, tapInstalled {
            engine.inputNode.removeTap(onBus: 0)
        }
        tapInstalled = false
        _ = resetPlaybackAccounting()
        player?.stop()
        engine?.stop()
        engine?.reset()
        player = nil
        engine = nil
        outputFormat = nil

        if let interruptionObserver {
            NotificationCenter.default.removeObserver(interruptionObserver)
            self.interruptionObserver = nil
        }
        #if os(iOS) || os(watchOS)
        try? AVAudioSession.sharedInstance().setActive(
            false,
            options: .notifyOthersOnDeactivation
        )
        #endif
    }

    deinit {
        stop()
    }
}
