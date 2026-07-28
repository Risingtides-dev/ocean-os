import Foundation

/// High-level lifecycle exposed to the Watch and iPhone shells.
public enum BuddyRealtimeStage: String, Equatable, Sendable {
    case off
    case connecting
    case live
    case interrupted
    case failed
}

/// The daemon-normalized ephemeral Realtime credential. Provider credentials
/// never live in app configuration or source control.
public struct BuddyRealtimeSecret: Codable, Equatable, Sendable {
    public let clientSecret: String
    public let expiresAt: JSONScalar?
    public let model: String
    public let workspaceRoot: String?

    public init(
        clientSecret: String,
        expiresAt: JSONScalar? = nil,
        model: String,
        workspaceRoot: String? = nil
    ) {
        self.clientSecret = clientSecret
        self.expiresAt = expiresAt
        self.model = model
        self.workspaceRoot = workspaceRoot
    }

    private enum CodingKeys: String, CodingKey {
        case model
        case clientSecret = "client_secret"
        case expiresAt = "expires_at"
        case workspaceRoot = "workspace_root"
    }

    func expires(within allowance: TimeInterval, now: Date = Date()) -> Bool {
        let epoch: Double?
        switch expiresAt {
        case let .some(.number(value)):
            epoch = value
        case let .some(.string(value)):
            epoch = Double(value)
        case .some(.bool), .none:
            epoch = nil
        }
        return epoch.map { $0 <= now.timeIntervalSince1970 + allowance } ?? false
    }
}

/// The mint response currently uses either a numeric epoch, a string, or null
/// for `expires_at`; keeping this scalar avoids coupling Buddy to upstream drift.
public enum JSONScalar: Codable, Equatable, Sendable {
    case string(String)
    case number(Double)
    case bool(Bool)

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if let value = try? container.decode(String.self) {
            self = .string(value)
        } else if let value = try? container.decode(Double.self) {
            self = .number(value)
        } else if let value = try? container.decode(Bool.self) {
            self = .bool(value)
        } else if container.decodeNil() {
            throw DecodingError.valueNotFound(
                JSONScalar.self,
                .init(codingPath: decoder.codingPath, debugDescription: "null has no scalar value")
            )
        } else {
            throw DecodingError.typeMismatch(
                JSONScalar.self,
                .init(codingPath: decoder.codingPath, debugDescription: "unsupported JSON scalar")
            )
        }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case let .string(value): try container.encode(value)
        case let .number(value): try container.encode(value)
        case let .bool(value): try container.encode(value)
        }
    }
}

public struct BuddyRealtimeToolCall: Equatable, Sendable {
    public let name: String
    public let callID: String
    public let arguments: String

    public init(name: String, callID: String, arguments: String) {
        self.name = name
        self.callID = callID
        self.arguments = arguments
    }
}

/// A deliberately small projection of a richer Surface component. Buddy never
/// treats model-authored component actions as device commands.
public struct BuddyRealtimeCard: Equatable, Identifiable, Sendable {
    public let id: String
    public let kind: String
    public let title: String
    public let detail: String?

    public init(id: String, kind: String, title: String, detail: String? = nil) {
        self.id = id
        self.kind = kind
        self.title = title
        self.detail = detail
    }
}

/// Pure effects emitted by the Realtime event reducer.
public enum BuddyRealtimeEffect: Equatable, Sendable {
    case sessionCreated
    case sessionUpdated
    case audio(Data)
    case assistantText(replyID: String, text: String, replace: Bool)
    case assistantAudioItem(String)
    case interrupted(itemID: String?)
    case responseCompleted
    case toolCall(BuddyRealtimeToolCall)
    case error(String)
}

public struct BuddyRealtimeToolResult: Equatable, Sendable {
    public let output: String
    public let card: BuddyRealtimeCard?

    public init(output: String, card: BuddyRealtimeCard? = nil) {
        self.output = output
        self.card = card
    }
}
