import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

struct BuddyRealtimeToolQuotaState: Equatable, Sendable {
    static let maximumCalls = 32
    static let maximumRenders = 4
    static let maximumHandoffs = 1

    private(set) var callCount = 0
    private(set) var renderCount = 0
    private(set) var handoffCount = 0
    private(set) var continuationEnabled = true

    mutating func consume(_ toolName: String) -> Bool {
        guard continuationEnabled, callCount < Self.maximumCalls else {
            continuationEnabled = false
            return false
        }
        switch toolName {
        case "render_component":
            guard renderCount < Self.maximumRenders else {
                continuationEnabled = false
                return false
            }
            renderCount += 1
        case "write_handoff":
            guard handoffCount < Self.maximumHandoffs else {
                continuationEnabled = false
                return false
            }
            handoffCount += 1
        default:
            break
        }
        callCount += 1
        return true
    }

    mutating func reset() {
        self = .init()
    }
}

struct BuddyHandoffAcknowledgement {
    static func isExplicitSuccess(data: Data, statusCode: Int) -> Bool {
        guard statusCode != 204,
              (200..<300).contains(statusCode),
              !data.isEmpty,
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              object["ok"] as? Bool == true
        else {
            return false
        }
        return true
    }
}

/// Converts a permissive Surface render request into a non-interactive,
/// size-bounded Watch card. Model-authored buttons are intentionally ignored.
public struct BuddyBoundedCardProjector: Sendable {
    public static let maximumArgumentsBytes = 32_768
    public static let maximumTitleCharacters = 80
    public static let maximumDetailCharacters = 500

    public init() {}

    public func project(arguments: String) -> BuddyRealtimeCard? {
        guard arguments.utf8.count <= Self.maximumArgumentsBytes,
              let data = arguments.data(using: .utf8),
              let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else {
            return nil
        }

        let inner = (root["component"] as? [String: Any]) ?? root
        let props = (inner["props"] as? [String: Any]) ?? inner
        let rawKind = (inner["kind"] as? String) ?? "component"
        let kind = safeKind(rawKind)
        let id = bounded(
            (inner["component_id"] as? String) ?? UUID().uuidString.lowercased(),
            to: 100
        )
        let title = firstString(in: props, keys: ["title", "heading", "label", "name"])
            .map { bounded(clean($0), to: Self.maximumTitleCharacters) }
            .flatMap { $0.isEmpty ? nil : $0 }
            ?? defaultTitle(for: kind)
        let detail = firstString(
            in: props,
            keys: ["detail", "text", "body", "markdown", "content", "value", "status"]
        )
        .map { bounded(clean($0), to: Self.maximumDetailCharacters) }
        .flatMap { $0.isEmpty ? nil : $0 }
        ?? "Ocean rendered a \(kind) component. Open Ocean Surface for the full view."

        return BuddyRealtimeCard(id: id, kind: kind, title: title, detail: detail)
    }

    private func firstString(in object: [String: Any], keys: [String]) -> String? {
        for key in keys {
            if let value = object[key] as? String {
                return value
            }
        }
        return nil
    }

    private func safeKind(_ value: String) -> String {
        let filtered = value.lowercased().filter { $0.isLetter || $0.isNumber || $0 == "_" || $0 == "-" }
        return filtered.isEmpty ? "component" : bounded(filtered, to: 40)
    }

    private func defaultTitle(for kind: String) -> String {
        switch kind {
        case "error", "error_card": "Ocean needs attention"
        case "result", "result_card": "Ocean finished"
        case "approval", "approval_card": "Ocean requests approval"
        case "status", "progress": "Ocean status"
        default: "Ocean"
        }
    }

    private func clean(_ value: String) -> String {
        value
            .replacingOccurrences(of: "\0", with: "")
            .split(whereSeparator: { $0.isWhitespace })
            .joined(separator: " ")
    }

    private func bounded(_ value: String, to limit: Int) -> String {
        String(value.prefix(limit))
    }
}

/// The only capability broker reachable from Buddy's Realtime connection.
/// Rich components become inert cards; handoffs use the existing daemon seam;
/// workspace reads and unknown tools are explicitly reported unavailable.
public actor BuddyRealtimeToolBroker {
    private static let maximumHandoffBytes = 8 * 1_024
    private static let maximumResponseBytes = 64 * 1_024

    private let baseURL: URL
    private let sessionID: String?
    private let loader: any BuddyHTTPLoading
    private let projector: BuddyBoundedCardProjector

    public init(
        baseURL: URL,
        sessionID: String?,
        projector: BuddyBoundedCardProjector = .init()
    ) {
        self.baseURL = baseURL
        self.sessionID = (try? BuddyPairingCode.validatedSessionID(sessionID)) ?? nil
        loader = BuddyBoundedHTTPClient()
        self.projector = projector
    }

    init(
        baseURL: URL,
        sessionID: String?,
        loader: any BuddyHTTPLoading,
        projector: BuddyBoundedCardProjector = .init()
    ) {
        self.baseURL = baseURL
        self.sessionID = (try? BuddyPairingCode.validatedSessionID(sessionID)) ?? nil
        self.loader = loader
        self.projector = projector
    }

    public func fulfill(_ call: BuddyRealtimeToolCall) async -> BuddyRealtimeToolResult {
        switch call.name {
        case "render_component":
            guard let card = projector.project(arguments: call.arguments) else {
                return .init(output: jsonOutput(ok: false, message: "component rejected by Buddy bounds"))
            }
            return .init(
                output: jsonOutput(ok: true, message: "rendered as a bounded, non-interactive Buddy card"),
                card: card
            )

        case "write_handoff":
            return await writeHandoff(arguments: call.arguments)

        case "list_workspace", "read_workspace_file":
            return .init(output: jsonOutput(
                ok: false,
                message: "\(call.name) is unavailable on Ocean Buddy; open Ocean Surface for workspace inspection"
            ))

        default:
            return .init(output: jsonOutput(
                ok: false,
                message: "tool \(call.name) is unavailable on Ocean Buddy"
            ))
        }
    }

    private func writeHandoff(arguments: String) async -> BuddyRealtimeToolResult {
        guard let sessionID, !sessionID.isEmpty else {
            return .init(output: jsonOutput(ok: false, message: "no Ocean session is bound to this voice chat"))
        }
        guard arguments.utf8.count <= BuddyBoundedCardProjector.maximumArgumentsBytes,
              let data = arguments.data(using: .utf8),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let rawNote = object["note"] as? String
        else {
            return .init(output: jsonOutput(ok: false, message: "handoff note is invalid"))
        }
        let note = rawNote.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !note.isEmpty else {
            return .init(output: jsonOutput(ok: false, message: "handoff note is empty"))
        }
        guard note.utf8.count <= Self.maximumHandoffBytes else {
            return .init(output: jsonOutput(ok: false, message: "handoff note exceeds the Buddy limit"))
        }

        let endpoint = baseURL
            .appendingPathComponent("v1")
            .appendingPathComponent("agent")
            .appendingPathComponent("sessions")
            .appendingPathComponent(sessionID)
            .appendingPathComponent("messages")
        var request = URLRequest(url: endpoint)
        request.httpMethod = "POST"
        request.timeoutInterval = 20
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try? JSONSerialization.data(withJSONObject: [
            "role": "user",
            "content": note,
            "kind": "handoff",
        ])

        do {
            let loaded = try await loader.load(
                request,
                maximumResponseBytes: Self.maximumResponseBytes
            )
            guard BuddyHandoffAcknowledgement.isExplicitSuccess(
                data: loaded.data,
                statusCode: loaded.response.statusCode
            ) else {
                return .init(output: jsonOutput(
                    ok: false,
                    message: "Ocean did not explicitly acknowledge the handoff (\(loaded.response.statusCode))"
                ))
            }
            return .init(output: jsonOutput(ok: true, message: "handoff recorded for the text agent"))
        } catch {
            return .init(output: jsonOutput(ok: false, message: "Ocean handoff request failed"))
        }
    }

    private func jsonOutput(ok: Bool, message: String) -> String {
        let data = try? JSONSerialization.data(withJSONObject: ["ok": ok, "message": message])
        return data.flatMap { String(data: $0, encoding: .utf8) }
            ?? "{\"ok\":false,\"message\":\"serialization failed\"}"
    }
}
