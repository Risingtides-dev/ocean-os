import Foundation
import XCTest
@testable import OceanBuddyCore

private struct FailingBuddyHTTPLoader: BuddyHTTPLoading {
    let error: BuddyHTTPTransportError

    func load(_ request: URLRequest, maximumResponseBytes: Int) async throws -> BuddyHTTPResponse {
        throw error
    }
}

final class RealtimeTests: XCTestCase {
    func testReducerDecodesSessionAudioTranscriptAndToolCalls() throws {
        var reducer = BuddyRealtimeEventReducer()

        XCTAssertEqual(
            reducer.reduce(text: #"{"type":"session.created"}"#),
            [.sessionCreated]
        )

        let audio = Data([0x01, 0x02, 0x03, 0x04])
        let audioEvent = #"{"type":"response.output_audio.delta","delta":"\#(audio.base64EncodedString())"}"#
        XCTAssertEqual(reducer.reduce(text: audioEvent), [.audio(audio)])

        let delta = #"{"type":"response.output_audio_transcript.delta","item_id":"item-1","delta":"Hello"}"#
        XCTAssertEqual(
            reducer.reduce(text: delta),
            [.assistantText(replyID: "item-1", text: "Hello", replace: false)]
        )

        let done = #"""
        {
          "type":"response.done",
          "response":{"status":"completed","output":[{
            "type":"function_call",
            "status":"completed",
            "name":"write_handoff",
            "call_id":"call-1",
            "arguments":"{\"note\":\"Ship it\"}"
          }]}
        }
        """#
        XCTAssertEqual(
            reducer.reduce(text: done),
            [
                .responseCompleted,
                .toolCall(.init(
                    name: "write_handoff",
                    callID: "call-1",
                    arguments: #"{"note":"Ship it"}"#
                )),
            ]
        )
    }

    func testReducerClearsAssistantItemWhenResponseCompletes() {
        var reducer = BuddyRealtimeEventReducer()
        let added = #"""
        {
          "type":"response.output_item.added",
          "item":{"type":"message","role":"assistant","id":"item-a"}
        }
        """#
        XCTAssertEqual(reducer.reduce(text: added), [.assistantAudioItem("item-a")])
        XCTAssertEqual(
            reducer.reduce(text: #"{"type":"input_audio_buffer.speech_started"}"#),
            [.interrupted(itemID: "item-a")]
        )
        XCTAssertEqual(
            reducer.reduce(text: #"{"type":"response.done","response":{"status":"completed","output":[]}}"#),
            [.responseCompleted]
        )
        XCTAssertEqual(
            reducer.reduce(text: #"{"type":"input_audio_buffer.speech_started"}"#),
            [.interrupted(itemID: nil)]
        )
    }

    func testReducerRejectsOversizedToolArguments() {
        var reducer = BuddyRealtimeEventReducer()
        let huge = String(repeating: "x", count: 33_000)
        let object: [String: Any] = [
            "type": "response.done",
            "response": [
                "status": "completed",
                "output": [[
                    "type": "function_call",
                    "status": "completed",
                    "name": "render_component",
                    "call_id": "call-big",
                    "arguments": huge,
                ]],
            ],
        ]
        let data = try! JSONSerialization.data(withJSONObject: object)
        let text = String(decoding: data, as: UTF8.self)
        XCTAssertEqual(
            reducer.reduce(text: text),
            [
                .responseCompleted,
                .error("Realtime tool arguments exceeded the Ocean Buddy limit."),
            ]
        )
    }

    func testCancelledResponseNeverDispatchesToolCalls() {
        var reducer = BuddyRealtimeEventReducer()
        let cancelled = #"""
        {
          "type":"response.done",
          "response":{"status":"cancelled","output":[{
            "type":"function_call",
            "status":"completed",
            "name":"write_handoff",
            "call_id":"call-cancelled",
            "arguments":"{\"note\":\"must not persist\"}"
          }]}
        }
        """#
        XCTAssertEqual(reducer.reduce(text: cancelled), [.responseCompleted])
    }

    func testReducerRejectsOversizedToolIdentifiers() throws {
        var reducer = BuddyRealtimeEventReducer()
        let object: [String: Any] = [
            "type": "response.done",
            "response": [
                "status": "completed",
                "output": [[
                    "type": "function_call",
                    "status": "completed",
                    "name": "render_component",
                    "call_id": String(repeating: "c", count: 257),
                    "arguments": "{}",
                ]],
            ],
        ]
        let data = try JSONSerialization.data(withJSONObject: object)
        XCTAssertEqual(
            reducer.reduce(text: String(decoding: data, as: UTF8.self)),
            [
                .responseCompleted,
                .error("Realtime tool identifiers exceeded the Ocean Buddy limit."),
            ]
        )
    }

    func testTranscriptBufferBoundsReplyIDsTextAndRetainedHistory() {
        var buffer = BuddyRealtimeTranscriptBuffer()
        let oversizedDelta = String(
            repeating: "x",
            count: BuddyRealtimeTranscriptBuffer.maximumReplyCharacters + 200
        )
        var visible = buffer.update(replyID: "open", text: oversizedDelta, replace: false)
        XCTAssertEqual(visible.count, BuddyRealtimeTranscriptBuffer.maximumVisibleCharacters)

        let oversizedID = String(
            repeating: "i",
            count: BuddyRealtimeTranscriptBuffer.maximumReplyIdentifierCharacters + 1
        )
        XCTAssertEqual(
            buffer.update(replyID: oversizedID, text: "must not be retained", replace: true),
            visible
        )

        for index in 0...BuddyRealtimeTranscriptBuffer.maximumReplies {
            visible = buffer.update(replyID: "reply-\(index)", text: "reply-\(index)", replace: true)
        }
        XCTAssertFalse(visible.contains("reply-0\n\n"))
        XCTAssertTrue(visible.hasSuffix("reply-\(BuddyRealtimeTranscriptBuffer.maximumReplies)"))
        XCTAssertLessThanOrEqual(
            buffer.storedReplyCount,
            BuddyRealtimeTranscriptBuffer.maximumReplies
        )
        XCTAssertLessThanOrEqual(
            buffer.storedCharacterCount,
            BuddyRealtimeTranscriptBuffer.maximumStoredCharacters
        )
        XCTAssertLessThanOrEqual(
            buffer.storedIdentifierCharacterCount,
            BuddyRealtimeTranscriptBuffer.maximumStoredIdentifierCharacters
        )
    }

    func testPlaybackAccountingDoesNotTurnSilenceOrPartialBuffersIntoCredit() throws {
        var accounting = BuddyPlaybackQueueAccounting()
        accounting.beginResponse()
        let first = try XCTUnwrap(accounting.reserve(frames: 2_400))
        accounting.didPlay(first)
        _ = try XCTUnwrap(accounting.reserve(frames: 2_400))

        XCTAssertEqual(accounting.interrupt(), 2_400)
    }

    func testPlaybackAccountingBoundsQueueAndIgnoresStaleCompletions() throws {
        var accounting = BuddyPlaybackQueueAccounting()
        accounting.beginResponse()
        let full = try XCTUnwrap(accounting.reserve(
            frames: BuddyPlaybackQueueAccounting.maximumQueuedFrames
        ))
        XCTAssertNil(accounting.reserve(frames: 1))

        XCTAssertTrue(accounting.reset())
        accounting.didPlay(full)
        accounting.beginResponse()
        XCTAssertNotNil(accounting.reserve(
            frames: BuddyPlaybackQueueAccounting.maximumQueuedFrames
        ))
        XCTAssertEqual(accounting.interrupt(), 0)
    }

    func testBeginningResponsePreservesScheduledQueueAndSeparatesPlaybackCredit() throws {
        var accounting = BuddyPlaybackQueueAccounting()
        accounting.beginResponse()
        let prior = try XCTUnwrap(accounting.reserve(
            frames: BuddyPlaybackQueueAccounting.maximumQueuedFrames / 2
        ))
        accounting.beginResponse()
        let current = try XCTUnwrap(accounting.reserve(
            frames: BuddyPlaybackQueueAccounting.maximumQueuedFrames / 2
        ))
        XCTAssertNil(accounting.reserve(frames: 1))

        accounting.didPlay(prior)
        accounting.didPlay(current)
        XCTAssertEqual(
            accounting.interrupt(),
            BuddyPlaybackQueueAccounting.maximumQueuedFrames / 2
        )
    }

    func testCompletedResponseRetainsCreditUntilQueuedAudioDrains() throws {
        var accounting = BuddyPlaybackQueueAccounting()
        accounting.beginResponse()
        let completed = try XCTUnwrap(accounting.reserve(frames: 4_800))
        accounting.didPlay(completed)
        XCTAssertFalse(accounting.endResponse())
        XCTAssertEqual(accounting.interrupt(), 0)

        accounting.beginResponse()
        let heard = try XCTUnwrap(accounting.reserve(frames: 2_400))
        _ = try XCTUnwrap(accounting.reserve(frames: 4_800))
        accounting.didPlay(heard)
        XCTAssertTrue(accounting.endResponse())
        XCTAssertEqual(accounting.interrupt(), 2_400)
    }

    func testToolQuotaPersistsAcrossChainedResponsesAndStopsContinuation() {
        XCTAssertEqual(BuddyRealtimeToolQuotaState.maximumCalls, 32)
        XCTAssertEqual(BuddyRealtimeToolQuotaState.maximumRenders, 4)
        XCTAssertEqual(BuddyRealtimeToolQuotaState.maximumHandoffs, 1)

        var renderQuota = BuddyRealtimeToolQuotaState()
        for _ in 0..<BuddyRealtimeToolQuotaState.maximumRenders {
            XCTAssertTrue(renderQuota.consume("render_component"))
        }
        XCTAssertFalse(renderQuota.consume("render_component"))
        XCTAssertFalse(renderQuota.continuationEnabled)
        XCTAssertFalse(renderQuota.consume("write_handoff"))

        var handoffQuota = BuddyRealtimeToolQuotaState()
        for _ in 0..<BuddyRealtimeToolQuotaState.maximumHandoffs {
            XCTAssertTrue(handoffQuota.consume("write_handoff"))
        }
        XCTAssertFalse(handoffQuota.consume("write_handoff"))
        XCTAssertFalse(handoffQuota.continuationEnabled)

        var totalQuota = BuddyRealtimeToolQuotaState()
        for _ in 0..<BuddyRealtimeToolQuotaState.maximumCalls {
            XCTAssertTrue(totalQuota.consume("unknown_tool"))
        }
        XCTAssertFalse(totalQuota.consume("unknown_tool"))
        XCTAssertFalse(totalQuota.continuationEnabled)
    }

    func testBoundedCardProjectionIgnoresActionsAndTruncatesContent() throws {
        let oversized = String(repeating: "A", count: 700)
        let object: [String: Any] = [
            "component": [
                "component_id": "component-1",
                "kind": "approval_card",
                "props": [
                    "title": "Approve dangerous arbitrary action",
                    "detail": oversized,
                    "actions": [["label": "Execute", "tool": "shell"]],
                ],
            ],
        ]
        let data = try JSONSerialization.data(withJSONObject: object)
        let card = try XCTUnwrap(
            BuddyBoundedCardProjector().project(arguments: String(decoding: data, as: UTF8.self))
        )

        XCTAssertEqual(card.id, "component-1")
        XCTAssertEqual(card.kind, "approval_card")
        XCTAssertEqual(card.title, "Approve dangerous arbitrary action")
        XCTAssertEqual(card.detail?.count, BuddyBoundedCardProjector.maximumDetailCharacters)
    }

    func testToolBrokerExplicitlyNarrowsUnavailableCapabilities() async {
        let broker = BuddyRealtimeToolBroker(
            baseURL: URL(string: "http://127.0.0.1:4780")!,
            sessionID: nil
        )
        let workspace = await broker.fulfill(.init(
            name: "read_workspace_file",
            callID: "call-read",
            arguments: #"{"path":"README.md"}"#
        ))
        XCTAssertTrue(workspace.output.contains("unavailable on Ocean Buddy"))
        XCTAssertNil(workspace.card)

        let handoff = await broker.fulfill(.init(
            name: "write_handoff",
            callID: "call-handoff",
            arguments: #"{"note":"do work"}"#
        ))
        XCTAssertTrue(handoff.output.contains("no Ocean session"))
        XCTAssertNil(handoff.card)
    }

    func testSensitiveSessionsRejectEveryRedirect() throws {
        let delegate = BuddyRejectRedirectDelegate()
        let session = URLSession(
            configuration: .ephemeral,
            delegate: delegate,
            delegateQueue: nil
        )
        defer { session.invalidateAndCancel() }

        let originalURL = try XCTUnwrap(URL(string: "https://ocean.example.com/mint"))
        let redirectedURL = try XCTUnwrap(URL(string: "http://192.168.1.20/steal"))
        let task = session.dataTask(with: originalURL)
        let response = try XCTUnwrap(HTTPURLResponse(
            url: originalURL,
            statusCode: 302,
            httpVersion: nil,
            headerFields: ["Location": redirectedURL.absoluteString]
        ))
        var followedRequest: URLRequest?
        delegate.urlSession(
            session,
            task: task,
            willPerformHTTPRedirection: response,
            newRequest: URLRequest(url: redirectedURL)
        ) { followedRequest = $0 }

        XCTAssertNil(followedRequest)
        let productionSession = BuddySecureURLSession.make()
        defer { productionSession.invalidateAndCancel() }
        XCTAssertTrue(productionSession.delegate is BuddyRejectRedirectDelegate)
    }

    func testCredentialAndHandoffRejectRedirectedHTTP() async {
        let failingLoader = FailingBuddyHTTPLoader(error: .redirectRejected)
        let secretClient = HTTPBuddyRealtimeSecretClient(
            baseURL: URL(string: "https://ocean.example.com")!,
            loader: failingLoader
        )
        do {
            _ = try await secretClient.mint(sessionID: nil, model: nil)
            XCTFail("credential redirects must be rejected")
        } catch {
            XCTAssertEqual(error as? BuddyRealtimeSecretError, .redirectRejected)
        }

        let broker = BuddyRealtimeToolBroker(
            baseURL: URL(string: "https://ocean.example.com")!,
            sessionID: UUID().uuidString,
            loader: failingLoader
        )
        let result = await broker.fulfill(.init(
            name: "write_handoff",
            callID: "redirected-handoff",
            arguments: #"{"note":"must not cross a redirect"}"#
        ))
        XCTAssertFalse(result.output.contains("handoff recorded"))
        XCTAssertTrue(result.output.contains("request failed"))
    }

    func testHandoffRequiresExplicitDecodedSuccess() {
        let trueBody = Data(#"{"ok":true}"#.utf8)
        XCTAssertTrue(BuddyHandoffAcknowledgement.isExplicitSuccess(
            data: trueBody,
            statusCode: 200
        ))

        let failures: [(Int, Data)] = [
            (204, Data()),
            (200, Data()),
            (200, Data("not-json".utf8)),
            (200, Data("{}".utf8)),
            (200, Data(#"{"ok":false}"#.utf8)),
            (200, Data(#"{"ok":"true"}"#.utf8)),
            (500, trueBody),
        ]
        for (status, data) in failures {
            XCTAssertFalse(
                BuddyHandoffAcknowledgement.isExplicitSuccess(data: data, statusCode: status),
                "status=\(status), body=\(String(decoding: data, as: UTF8.self))"
            )
        }
    }

    func testDaemonEndpointPolicyFailsClosedUnlessDebugExplicitlyOptsIn() {
        XCTAssertTrue(BuddyDaemonEndpointPolicy.allows(URL(string: "http://localhost:4780")!))
        XCTAssertTrue(BuddyDaemonEndpointPolicy.allows(URL(string: "http://127.42.0.1:4780")!))
        XCTAssertTrue(BuddyDaemonEndpointPolicy.allows(URL(string: "https://ocean.example.com")!))

        let localHTTP = [
            "http://risings-mac-mini.local:4780",
            "http://10.0.0.8:4780",
            "http://172.16.0.8:4780",
            "http://192.168.1.8:4780",
            "http://100.108.56.88:4780",
        ]
        for rawURL in localHTTP {
            let url = URL(string: rawURL)!
            XCTAssertFalse(BuddyDaemonEndpointPolicy.allows(url), rawURL)
            XCTAssertFalse(BuddyDaemonEndpointPolicy.allows(
                url,
                mode: .development,
                allowInsecureLocalNetwork: false
            ), rawURL)
            #if DEBUG
            XCTAssertTrue(BuddyDaemonEndpointPolicy.allows(
                url,
                mode: .development,
                allowInsecureLocalNetwork: true
            ), rawURL)
            #else
            XCTAssertFalse(BuddyDaemonEndpointPolicy.allows(
                url,
                mode: .development,
                allowInsecureLocalNetwork: true
            ), rawURL)
            #endif
            XCTAssertFalse(BuddyDaemonEndpointPolicy.allows(
                url,
                mode: .release,
                allowInsecureLocalNetwork: true
            ), rawURL)
        }

        XCTAssertFalse(BuddyDaemonEndpointPolicy.allows(URL(string: "http://ocean.example.com:4780")!))
        XCTAssertFalse(BuddyDaemonEndpointPolicy.allows(URL(string: "file:///tmp/ocean")!))
        XCTAssertFalse(BuddyDaemonEndpointPolicy.allows(URL(string: "http://user:pass@localhost:4780")!))
    }

    @MainActor
    func testControllerReportsActionableFailureBeforeMintingOverCleartextLAN() {
        let controller = OceanBuddyRealtimeController()
        controller.start(baseURL: URL(string: "http://192.168.1.10:4780")!)
        XCTAssertEqual(controller.stage, .failed)
        XCTAssertEqual(controller.status, "Voice unavailable")
        XCTAssertTrue(controller.errorMessage?.contains("HTTPS") == true)
        XCTAssertTrue(controller.errorMessage?.contains("Debug builds") == true)
        controller.stop()
    }

    func testRealtimeSecretUsesEphemeralCacheDisabledSessionAndFailsClosed() async {
        let client = HTTPBuddyRealtimeSecretClient(
            baseURL: URL(string: "http://192.168.1.10:4780")!
        )
        do {
            _ = try await client.mint(sessionID: nil, model: nil)
            XCTFail("cleartext LAN mint should fail before transport")
        } catch {
            XCTAssertEqual(error as? BuddyRealtimeSecretError, .insecureEndpoint)
        }

        let configuration = HTTPBuddyRealtimeSecretClient.ephemeralConfiguration()
        XCTAssertEqual(configuration.requestCachePolicy, .reloadIgnoringLocalCacheData)
        XCTAssertNil(configuration.urlCache)
        XCTAssertFalse(configuration.httpShouldSetCookies)
        XCTAssertNil(configuration.httpCookieStorage)
        XCTAssertEqual(configuration.timeoutIntervalForRequest, 20)
        XCTAssertEqual(configuration.timeoutIntervalForResource, 30)
    }

    func testBuddyCardDecoderRejectsOversizedDaemonResponses() throws {
        XCTAssertEqual(HTTPBuddyBackendClient.maximumResponseBytes, 64 * 1_024)
        let cardID = UUID().uuidString
        let actionID = UUID().uuidString

        func decodeCard(_ object: [String: Any]) throws -> BuddyCard {
            let data = try JSONSerialization.data(withJSONObject: object)
            return try JSONDecoder().decode(BuddyCard.self, from: data)
        }

        XCTAssertThrowsError(try decodeCard([
            "id": cardID,
            "kind": "result_card",
            "title": String(repeating: "t", count: BuddyCard.maximumTitleCharacters + 1),
        ]))
        XCTAssertThrowsError(try decodeCard([
            "id": cardID,
            "kind": "result_card",
            "title": "Result",
            "detail": String(repeating: "d", count: BuddyCard.maximumDetailCharacters + 1),
        ]))
        let action: [String: Any] = [
            "id": actionID,
            "label": "Approve",
            "kind": "photo_to_context",
            "requires_confirmation": true,
            "target_device": "i_phone",
        ]
        XCTAssertThrowsError(try decodeCard([
            "id": cardID,
            "kind": "approval_card",
            "title": "Approval",
            "actions": Array(repeating: action, count: BuddyCard.maximumActions + 1),
        ]))
        var oversizedLabelAction = action
        oversizedLabelAction["label"] = String(
            repeating: "a",
            count: BuddyAction.maximumLabelCharacters + 1
        )
        XCTAssertThrowsError(try decodeCard([
            "id": cardID,
            "kind": "approval_card",
            "title": "Approval",
            "actions": [oversizedLabelAction],
        ]))
    }

    func testRealtimeSecretDecodesDaemonShape() throws {
        let data = Data(#"""
        {
          "client_secret":"ek_test",
          "expires_at":1234,
          "model":"gpt-realtime-2.1",
          "workspace_root":"/tmp/ocean"
        }
        """#.utf8)
        let secret = try JSONDecoder().decode(BuddyRealtimeSecret.self, from: data)
        XCTAssertEqual(secret.clientSecret, "ek_test")
        XCTAssertEqual(secret.expiresAt, .number(1234))
        XCTAssertEqual(secret.model, "gpt-realtime-2.1")
        XCTAssertEqual(secret.workspaceRoot, "/tmp/ocean")
    }

    func testRealtimeSecretExpiryAcceptsNumericAndStringEpochs() {
        let now = Date(timeIntervalSince1970: 1_000)
        let numeric = BuddyRealtimeSecret(
            clientSecret: "ek_numeric",
            expiresAt: .number(1_010),
            model: "gpt-realtime-2.1"
        )
        let string = BuddyRealtimeSecret(
            clientSecret: "ek_string",
            expiresAt: .string("1010"),
            model: "gpt-realtime-2.1"
        )
        let missing = BuddyRealtimeSecret(
            clientSecret: "ek_missing",
            model: "gpt-realtime-2.1"
        )

        XCTAssertTrue(numeric.expires(within: 15, now: now))
        XCTAssertTrue(string.expires(within: 15, now: now))
        XCTAssertFalse(missing.expires(within: 15, now: now))
    }
}
