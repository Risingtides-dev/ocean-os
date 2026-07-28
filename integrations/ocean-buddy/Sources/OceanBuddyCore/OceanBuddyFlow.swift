import Foundation

public protocol IPhoneCameraBrokering: Sendable {
    func capturePhoto(for approvedEvent: BuddyEvent) async throws -> BuddyAttachment
}

public enum MockIPhoneCameraMode: Equatable, Sendable {
    case available
    case unavailable
}

public enum BuddyCameraBrokerError: Error, Equatable {
    case phoneUnavailable
    case invalidApprovalContract
}

/// The immutable boundary for the only device action implemented by this
/// package. The card is validated at approval, and the approved event/action is
/// validated again by the camera broker before any capture can begin.
public enum BuddyPhotoApprovalContract {
    public static let cardTitle = "Attach Photo to Current Ocean Context."
    public static let actionLabel = "Approve"

    public static func action(in card: BuddyCard) -> BuddyAction? {
        guard card.kind == .approvalCard,
              card.title == cardTitle,
              card.detail == nil,
              card.actions.count == 1,
              let action = card.actions.first,
              validates(action)
        else {
            return nil
        }
        return action
    }

    public static func validates(_ action: BuddyAction) -> Bool {
        action.label == actionLabel
            && action.kind == .photoToContext
            && action.requiresConfirmation
            && action.targetDevice == .iPhone
    }

    public static func validates(approvedEvent: BuddyEvent) -> Bool {
        guard approvedEvent.state == .approved,
              approvedEvent.attachment == nil,
              approvedEvent.target == nil,
              approvedEvent.failure == nil,
              let approvedAction = approvedEvent.action,
              let approvalCard = approvedEvent.card,
              action(in: approvalCard) == approvedAction
        else {
            return false
        }
        return true
    }
}

/// iPhone sensor stub. It acknowledges the Watch-approved action and returns
/// metadata for a zero-byte JPEG; no camera API or photo library is touched.
public struct MockIPhoneCameraBroker: IPhoneCameraBrokering {
    private let mode: MockIPhoneCameraMode

    public init(mode: MockIPhoneCameraMode = .available) {
        self.mode = mode
    }

    public func capturePhoto(for approvedEvent: BuddyEvent) async throws -> BuddyAttachment {
        guard BuddyPhotoApprovalContract.validates(approvedEvent: approvedEvent) else {
            throw BuddyCameraBrokerError.invalidApprovalContract
        }
        guard mode == .available else {
            throw BuddyCameraBrokerError.phoneUnavailable
        }
        return BuddyAttachment(
            id: UUID(),
            kind: .photo,
            mimeType: "image/jpeg",
            filename: "ocean-buddy-mock.jpg",
            byteCount: 0,
            mockCapture: true
        )
    }
}

public protocol BuddyBackendClient: Sendable {
    func send(_ event: BuddyEvent) async throws -> BuddyEventResponse
}

public struct BuddyApprovalResult: Equatable, Sendable {
    public let requestedEvent: BuddyEvent
    public let approvedEvent: BuddyEvent
    public let attachedEvent: BuddyEvent
    public let resultEvent: BuddyEvent
    public let renderedResultCard: RenderedBuddyCard
}

public struct BuddyApprovalFailure: Equatable, Sendable {
    public let requestedEvent: BuddyEvent
    public let approvedEvent: BuddyEvent
    public let failedEvent: BuddyEvent
    public let renderedErrorCard: RenderedBuddyCard
}

public enum BuddyApprovalOutcome: Equatable, Sendable {
    case success(BuddyApprovalResult)
    case failure(BuddyApprovalFailure)
}

public enum OceanBuddyFlowError: Error, Equatable {
    case invalidApprovalContract
    case backendRejected
}

/// The first end-to-end device flow:
/// Watch approval -> iPhone mock capture -> Rust event ingress -> Watch result.
public struct OceanBuddyFlow<Renderer, CameraBroker, Backend>: Sendable
where Renderer: BuddyCardRendering,
      CameraBroker: IPhoneCameraBrokering,
      Backend: BuddyBackendClient {
    private let renderer: Renderer
    private let cameraBroker: CameraBroker
    private let backend: Backend

    public init(renderer: Renderer, cameraBroker: CameraBroker, backend: Backend) {
        self.renderer = renderer
        self.cameraBroker = cameraBroker
        self.backend = backend
    }

    public func approvalCard(cardID: UUID = UUID(), actionID: UUID = UUID()) -> BuddyCard {
        BuddyCard(
            id: cardID,
            kind: .approvalCard,
            title: BuddyPhotoApprovalContract.cardTitle,
            actions: [BuddyAction(
                id: actionID,
                label: BuddyPhotoApprovalContract.actionLabel,
                kind: .photoToContext,
                requiresConfirmation: true,
                targetDevice: .iPhone
            )]
        )
    }

    public func renderApproval(_ card: BuddyCard) -> RenderedBuddyCard {
        renderer.render(card)
    }

    public func approve(
        _ card: BuddyCard,
        flowID: UUID = UUID(),
        occurredAt: Date = Date()
    ) async throws -> BuddyApprovalOutcome {
        guard let action = BuddyPhotoApprovalContract.action(in: card) else {
            throw OceanBuddyFlowError.invalidApprovalContract
        }

        let requestedEvent = BuddyEvent(
            eventID: UUID(),
            flowID: flowID,
            state: .requested,
            occurredAt: occurredAt,
            action: action,
            card: card
        )
        let approvedEvent = BuddyEvent(
            eventID: UUID(),
            flowID: flowID,
            causationID: requestedEvent.eventID,
            state: .approved,
            occurredAt: occurredAt,
            action: action,
            card: card
        )

        do {
            let attachment = try await cameraBroker.capturePhoto(for: approvedEvent)
            let attachedEvent = BuddyEvent(
                eventID: UUID(),
                flowID: flowID,
                causationID: approvedEvent.eventID,
                state: .attached,
                occurredAt: occurredAt,
                attachment: attachment,
                target: .currentOceanContext
            )
            let response = try await backend.send(attachedEvent)
            guard response.accepted else {
                throw OceanBuddyFlowError.backendRejected
            }
            let resultEvent = BuddyEvent(
                eventID: UUID(),
                flowID: flowID,
                causationID: attachedEvent.eventID,
                state: .result,
                occurredAt: occurredAt,
                card: response.card
            )
            return .success(BuddyApprovalResult(
                requestedEvent: requestedEvent,
                approvedEvent: approvedEvent,
                attachedEvent: attachedEvent,
                resultEvent: resultEvent,
                renderedResultCard: renderer.render(response.card)
            ))
        } catch BuddyCameraBrokerError.phoneUnavailable {
            let errorCard = BuddyCard(
                id: UUID(),
                kind: .errorCard,
                title: "Photo was not attached.",
                detail: "iPhone is unavailable. Bring it online and try again."
            )
            let failedEvent = BuddyEvent(
                eventID: UUID(),
                flowID: flowID,
                causationID: approvedEvent.eventID,
                state: .failed,
                occurredAt: occurredAt,
                card: errorCard,
                failure: BuddyFailure(
                    code: .phoneUnavailable,
                    message: "iPhone is unavailable.",
                    retryable: true
                )
            )
            return .failure(BuddyApprovalFailure(
                requestedEvent: requestedEvent,
                approvedEvent: approvedEvent,
                failedEvent: failedEvent,
                renderedErrorCard: renderer.render(errorCard)
            ))
        }
    }
}
