import Foundation

public enum BuddyComponentKind: String, Codable, Sendable {
    case approvalCard = "approval_card"
    case resultCard = "result_card"
    case errorCard = "error_card"
}

public enum BuddyDevice: String, Codable, Sendable {
    case appleWatch = "apple_watch"
    case iPhone = "i_phone"
    case iPad = "i_pad"
    case ocean
}

public enum BuddyActionKind: String, Codable, Sendable {
    case photoToContext = "photo_to_context"
}

public struct BuddyAction: Codable, Equatable, Sendable {
    public static let maximumLabelCharacters = 80

    public let id: UUID
    public let label: String
    public let kind: BuddyActionKind
    public let requiresConfirmation: Bool
    public let targetDevice: BuddyDevice

    public init(
        id: UUID,
        label: String,
        kind: BuddyActionKind,
        requiresConfirmation: Bool,
        targetDevice: BuddyDevice
    ) {
        self.id = id
        self.label = label
        self.kind = kind
        self.requiresConfirmation = requiresConfirmation
        self.targetDevice = targetDevice
    }

    private enum CodingKeys: String, CodingKey {
        case id, label, kind
        case requiresConfirmation = "requires_confirmation"
        case targetDevice = "target_device"
    }

    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(UUID.self, forKey: .id)
        let label = try values.decode(String.self, forKey: .label)
        guard label.count <= Self.maximumLabelCharacters else {
            throw DecodingError.dataCorruptedError(
                forKey: .label,
                in: values,
                debugDescription: "Buddy action label exceeds the supported limit"
            )
        }
        self.label = label
        kind = try values.decode(BuddyActionKind.self, forKey: .kind)
        requiresConfirmation = try values.decode(Bool.self, forKey: .requiresConfirmation)
        targetDevice = try values.decode(BuddyDevice.self, forKey: .targetDevice)
    }
}

public struct BuddyCard: Codable, Equatable, Sendable {
    public static let maximumTitleCharacters = 80
    public static let maximumDetailCharacters = 500
    public static let maximumActions = 4

    public let id: UUID
    public let kind: BuddyComponentKind
    public let title: String
    public let detail: String?
    public let actions: [BuddyAction]

    public init(
        id: UUID,
        kind: BuddyComponentKind,
        title: String,
        detail: String? = nil,
        actions: [BuddyAction] = []
    ) {
        self.id = id
        self.kind = kind
        self.title = title
        self.detail = detail
        self.actions = actions
    }

    private enum CodingKeys: String, CodingKey {
        case id, kind, title, detail, actions
    }

    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        id = try values.decode(UUID.self, forKey: .id)
        kind = try values.decode(BuddyComponentKind.self, forKey: .kind)

        let title = try values.decode(String.self, forKey: .title)
        guard title.count <= Self.maximumTitleCharacters else {
            throw DecodingError.dataCorruptedError(
                forKey: .title,
                in: values,
                debugDescription: "Buddy card title exceeds the supported limit"
            )
        }
        self.title = title

        let detail = try values.decodeIfPresent(String.self, forKey: .detail)
        if let detail, detail.count > Self.maximumDetailCharacters {
            throw DecodingError.dataCorruptedError(
                forKey: .detail,
                in: values,
                debugDescription: "Buddy card detail exceeds the supported limit"
            )
        }
        self.detail = detail

        let actions = try values.decodeIfPresent([BuddyAction].self, forKey: .actions) ?? []
        guard actions.count <= Self.maximumActions else {
            throw DecodingError.dataCorruptedError(
                forKey: .actions,
                in: values,
                debugDescription: "Buddy card action count exceeds the supported limit"
            )
        }
        self.actions = actions
    }
}

public enum BuddyAttachmentKind: String, Codable, Sendable {
    case photo
}

public enum BuddyAttachmentTarget: String, Codable, Sendable {
    case currentOceanContext = "current_ocean_context"
}

public struct BuddyAttachment: Codable, Equatable, Sendable {
    public let id: UUID
    public let kind: BuddyAttachmentKind
    public let mimeType: String
    public let filename: String
    public let byteCount: UInt64
    public let mockCapture: Bool

    public init(
        id: UUID,
        kind: BuddyAttachmentKind,
        mimeType: String,
        filename: String,
        byteCount: UInt64,
        mockCapture: Bool
    ) {
        self.id = id
        self.kind = kind
        self.mimeType = mimeType
        self.filename = filename
        self.byteCount = byteCount
        self.mockCapture = mockCapture
    }

    private enum CodingKeys: String, CodingKey {
        case id, kind, filename
        case mimeType = "mime_type"
        case byteCount = "byte_count"
        case mockCapture = "mock_capture"
    }
}

public enum BuddyEventState: String, Codable, Sendable {
    case requested
    case approved
    case captureStarted = "capture_started"
    case captureCompleted = "capture_completed"
    case uploaded
    case attached
    case result
    case failed
}

public enum BuddyFailureCode: String, Codable, Sendable {
    case phoneUnavailable = "phone_unavailable"
}

public struct BuddyFailure: Codable, Equatable, Sendable {
    public let code: BuddyFailureCode
    public let message: String
    public let retryable: Bool

    public init(code: BuddyFailureCode, message: String, retryable: Bool) {
        self.code = code
        self.message = message
        self.retryable = retryable
    }
}

public struct BuddyEvent: Codable, Equatable, Sendable {
    public let eventID: UUID
    public let flowID: UUID
    public let causationID: UUID?
    public let state: BuddyEventState
    public let occurredAt: Date
    public let action: BuddyAction?
    public let attachment: BuddyAttachment?
    public let target: BuddyAttachmentTarget?
    public let card: BuddyCard?
    public let failure: BuddyFailure?

    public init(
        eventID: UUID,
        flowID: UUID,
        causationID: UUID? = nil,
        state: BuddyEventState,
        occurredAt: Date,
        action: BuddyAction? = nil,
        attachment: BuddyAttachment? = nil,
        target: BuddyAttachmentTarget? = nil,
        card: BuddyCard? = nil,
        failure: BuddyFailure? = nil
    ) {
        self.eventID = eventID
        self.flowID = flowID
        self.causationID = causationID
        self.state = state
        self.occurredAt = occurredAt
        self.action = action
        self.attachment = attachment
        self.target = target
        self.card = card
        self.failure = failure
    }

    private enum CodingKeys: String, CodingKey {
        case state, action, attachment, target, card, failure
        case eventID = "event_id"
        case flowID = "flow_id"
        case causationID = "causation_id"
        case occurredAt = "occurred_at"
    }
}

public struct BuddyEventResponse: Codable, Equatable, Sendable {
    public let accepted: Bool
    public let receivedEventID: UUID
    public let card: BuddyCard

    public init(accepted: Bool, receivedEventID: UUID, card: BuddyCard) {
        self.accepted = accepted
        self.receivedEventID = receivedEventID
        self.card = card
    }

    private enum CodingKeys: String, CodingKey {
        case accepted, card
        case receivedEventID = "received_event_id"
    }
}
