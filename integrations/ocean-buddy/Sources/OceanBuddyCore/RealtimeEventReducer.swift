import Foundation

/// Parses provider events without letting provider-shaped JSON leak into the UI
/// or capability layer. The reducer is deterministic and platform-neutral.
public struct BuddyRealtimeEventReducer: Sendable {
    private static let maximumServerEventBytes = 512 * 1_024
    private static let maximumTextCharacters = 8_000
    private static let maximumToolArgumentBytes = 32_768
    private static let maximumIdentifierCharacters = 256
    private static let maximumToolNameCharacters = 128
    private var activeAssistantItemID: String?

    public init() {}

    public mutating func reset() {
        activeAssistantItemID = nil
    }

    public mutating func reduce(text: String) -> [BuddyRealtimeEffect] {
        guard text.utf8.count <= Self.maximumServerEventBytes else {
            return [.error("Realtime server event exceeded the Ocean Buddy limit.")]
        }
        guard let data = text.data(using: .utf8),
              let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let type = root["type"] as? String
        else {
            return []
        }

        switch type {
        case "session.created":
            return [.sessionCreated]

        case "session.updated":
            return [.sessionUpdated]

        case "response.output_audio.delta":
            guard let encoded = root["delta"] as? String,
                  let audio = Data(base64Encoded: encoded),
                  !audio.isEmpty
            else {
                return []
            }
            return [.audio(audio)]

        case "response.output_audio_transcript.delta", "response.output_text.delta":
            guard let delta = bounded(root["delta"] as? String), !delta.isEmpty else {
                return []
            }
            return [.assistantText(
                replyID: replyID(in: root),
                text: delta,
                replace: false
            )]

        case "response.output_audio_transcript.done", "response.output_text.done":
            let complete = (root["transcript"] as? String) ?? (root["text"] as? String)
            guard let complete = bounded(complete), !complete.isEmpty else {
                return []
            }
            return [.assistantText(
                replyID: replyID(in: root),
                text: complete,
                replace: true
            )]

        case "response.output_item.added":
            guard let item = root["item"] as? [String: Any],
                  item["type"] as? String == "message",
                  let itemID = boundedNonempty(
                      item["id"] as? String,
                      maximumCharacters: Self.maximumIdentifierCharacters
                  )
            else {
                return []
            }
            activeAssistantItemID = itemID
            return [.assistantAudioItem(itemID)]

        case "input_audio_buffer.speech_started":
            return [.interrupted(itemID: activeAssistantItemID)]

        case "response.done":
            // The audio adapter retains a completed item only while its final
            // queued buffers are still playing; ordinary later speech must not
            // truncate a stale item.
            activeAssistantItemID = nil
            return [.responseCompleted] + toolEffects(from: root)

        case "error":
            let nested = root["error"] as? [String: Any]
            let message = bounded((nested?["message"] as? String) ?? "Realtime session error")
                ?? "Realtime session error"
            return [.error(message)]

        default:
            if type.hasSuffix("_error") {
                let message = bounded(root["message"] as? String) ?? "Realtime session error"
                return [.error(message)]
            }
            return []
        }
    }

    private func toolEffects(from root: [String: Any]) -> [BuddyRealtimeEffect] {
        guard let response = root["response"] as? [String: Any],
              response["status"] as? String == "completed",
              let output = response["output"] as? [[String: Any]]
        else {
            return []
        }

        var effects: [BuddyRealtimeEffect] = []
        for item in output where item["type"] as? String == "function_call" {
            if let status = item["status"] as? String, status != "completed" {
                continue
            }
            guard let name = boundedNonempty(
                      item["name"] as? String,
                      maximumCharacters: Self.maximumToolNameCharacters
                  ),
                  let callID = boundedNonempty(
                      item["call_id"] as? String,
                      maximumCharacters: Self.maximumIdentifierCharacters
                  )
            else {
                effects.append(.error("Realtime tool identifiers exceeded the Ocean Buddy limit."))
                continue
            }
            let arguments = (item["arguments"] as? String) ?? "{}"
            guard arguments.utf8.count <= Self.maximumToolArgumentBytes else {
                effects.append(.error("Realtime tool arguments exceeded the Ocean Buddy limit."))
                continue
            }
            effects.append(.toolCall(BuddyRealtimeToolCall(
                name: name,
                callID: callID,
                arguments: arguments
            )))
        }
        return effects
    }

    private func replyID(in root: [String: Any]) -> String {
        boundedNonempty(
            root["item_id"] as? String,
            maximumCharacters: Self.maximumIdentifierCharacters
        )
            ?? boundedNonempty(
                root["response_id"] as? String,
                maximumCharacters: Self.maximumIdentifierCharacters
            )
            ?? activeAssistantItemID
            ?? "current"
    }

    private func bounded(_ value: String?) -> String? {
        value.map { String($0.prefix(Self.maximumTextCharacters)) }
    }

    private func boundedNonempty(_ value: String?, maximumCharacters: Int) -> String? {
        guard let value,
              !value.isEmpty,
              value.count <= maximumCharacters
        else {
            return nil
        }
        return value
    }
}
