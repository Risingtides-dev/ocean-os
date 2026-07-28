import Foundation
import XCTest
@testable import OceanBuddyCore

actor RecordingBackend: BuddyBackendClient {
    private(set) var events: [BuddyEvent] = []

    func send(_ event: BuddyEvent) async throws -> BuddyEventResponse {
        events.append(event)
        guard event.state == .attached else {
            throw OceanBuddyFlowError.backendRejected
        }
        return BuddyEventResponse(
            accepted: true,
            receivedEventID: event.eventID,
            card: BuddyCard(
                id: UUID(),
                kind: .resultCard,
                title: "Photo attached to current Ocean context.",
                detail: "Mock iPhone capture accepted by Ocean."
            )
        )
    }
}

final class OceanBuddyFlowTests: XCTestCase {
    func testApprovalCardUsesExactActionContract() {
        let flow = OceanBuddyFlow(
            renderer: MockBuddyCardRenderer(),
            cameraBroker: MockIPhoneCameraBroker(),
            backend: RecordingBackend()
        )
        let card = flow.approvalCard()
        let rendered = flow.renderApproval(card)

        XCTAssertEqual(rendered.title, "Attach Photo to Current Ocean Context.")
        XCTAssertEqual(rendered.buttons, ["Approve"])
        XCTAssertEqual(card.actions.first?.kind, .photoToContext)
        XCTAssertEqual(card.actions.first?.requiresConfirmation, true)
        XCTAssertEqual(card.actions.first?.targetDevice, .iPhone)
        XCTAssertNotNil(BuddyPhotoApprovalContract.action(in: card))
    }

    func testFlowRejectsEveryMutatedPhotoApprovalField() async {
        let backend = RecordingBackend()
        let flow = OceanBuddyFlow(
            renderer: MockBuddyCardRenderer(),
            cameraBroker: MockIPhoneCameraBroker(),
            backend: backend
        )
        let valid = flow.approvalCard()
        let validAction = valid.actions[0]
        let invalidCards = [
            BuddyCard(id: valid.id, kind: .resultCard, title: valid.title, actions: [validAction]),
            BuddyCard(id: valid.id, kind: .approvalCard, title: "Attach something else", actions: [validAction]),
            BuddyCard(
                id: valid.id,
                kind: .approvalCard,
                title: valid.title,
                detail: "model-added scope",
                actions: [validAction]
            ),
            BuddyCard(id: valid.id, kind: .approvalCard, title: valid.title, actions: []),
            BuddyCard(
                id: valid.id,
                kind: .approvalCard,
                title: valid.title,
                actions: [validAction, validAction]
            ),
            BuddyCard(id: valid.id, kind: .approvalCard, title: valid.title, actions: [BuddyAction(
                id: validAction.id,
                label: "Go",
                kind: .photoToContext,
                requiresConfirmation: true,
                targetDevice: .iPhone
            )]),
            BuddyCard(id: valid.id, kind: .approvalCard, title: valid.title, actions: [BuddyAction(
                id: validAction.id,
                label: BuddyPhotoApprovalContract.actionLabel,
                kind: .photoToContext,
                requiresConfirmation: false,
                targetDevice: .iPhone
            )]),
            BuddyCard(id: valid.id, kind: .approvalCard, title: valid.title, actions: [BuddyAction(
                id: validAction.id,
                label: BuddyPhotoApprovalContract.actionLabel,
                kind: .photoToContext,
                requiresConfirmation: true,
                targetDevice: .appleWatch
            )]),
        ]

        for card in invalidCards {
            do {
                _ = try await flow.approve(card)
                XCTFail("mutated approval card should be rejected: \(card)")
            } catch {
                XCTAssertEqual(error as? OceanBuddyFlowError, .invalidApprovalContract)
            }
        }
        let backendEvents = await backend.events
        XCTAssertTrue(backendEvents.isEmpty)
    }

    func testCameraBrokerRevalidatesImmutableApprovalCardAndAction() async throws {
        let broker = MockIPhoneCameraBroker()
        let action = BuddyAction(
            id: UUID(),
            label: BuddyPhotoApprovalContract.actionLabel,
            kind: .photoToContext,
            requiresConfirmation: true,
            targetDevice: .iPhone
        )
        let card = BuddyCard(
            id: UUID(),
            kind: .approvalCard,
            title: BuddyPhotoApprovalContract.cardTitle,
            actions: [action]
        )
        let validEvent = BuddyEvent(
            eventID: UUID(),
            flowID: UUID(),
            state: .approved,
            occurredAt: Date(timeIntervalSince1970: 0),
            action: action,
            card: card
        )
        let attachment = try await broker.capturePhoto(for: validEvent)
        XCTAssertEqual(attachment.kind, .photo)

        let wrongTargetAction = BuddyAction(
            id: action.id,
            label: action.label,
            kind: .photoToContext,
            requiresConfirmation: action.requiresConfirmation,
            targetDevice: .appleWatch
        )
        let invalidEvents = [
            BuddyEvent(
                eventID: UUID(), flowID: validEvent.flowID, state: .requested,
                occurredAt: validEvent.occurredAt, action: action, card: card
            ),
            BuddyEvent(
                eventID: UUID(), flowID: validEvent.flowID, state: .approved,
                occurredAt: validEvent.occurredAt, action: action
            ),
            BuddyEvent(
                eventID: UUID(), flowID: validEvent.flowID, state: .approved,
                occurredAt: validEvent.occurredAt, action: wrongTargetAction, card: card
            ),
            BuddyEvent(
                eventID: UUID(), flowID: validEvent.flowID, state: .approved,
                occurredAt: validEvent.occurredAt, action: action,
                card: BuddyCard(
                    id: card.id,
                    kind: .approvalCard,
                    title: BuddyPhotoApprovalContract.cardTitle,
                    actions: [wrongTargetAction]
                )
            ),
            BuddyEvent(
                eventID: UUID(), flowID: validEvent.flowID, state: .approved,
                occurredAt: validEvent.occurredAt, action: action, card: card,
                failure: BuddyFailure(
                    code: .phoneUnavailable,
                    message: "unexpected",
                    retryable: false
                )
            ),
        ]

        for event in invalidEvents {
            do {
                _ = try await broker.capturePhoto(for: event)
                XCTFail("camera broker accepted a mutated approval event")
            } catch {
                XCTAssertEqual(error as? BuddyCameraBrokerError, .invalidApprovalContract)
            }
        }
    }

    func testApprovalRunsMockCaptureAndRendersResultCard() async throws {
        let backend = RecordingBackend()
        let flow = OceanBuddyFlow(
            renderer: MockBuddyCardRenderer(),
            cameraBroker: MockIPhoneCameraBroker(),
            backend: backend
        )

        let outcome = try await flow.approve(
            flow.approvalCard(),
            flowID: UUID(uuidString: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")!,
            occurredAt: Date(timeIntervalSince1970: 0)
        )
        guard case let .success(result) = outcome else {
            return XCTFail("expected success")
        }

        XCTAssertEqual(result.requestedEvent.state, .requested)
        XCTAssertEqual(result.approvedEvent.state, .approved)
        XCTAssertEqual(result.approvedEvent.action?.targetDevice, .iPhone)
        XCTAssertEqual(result.approvedEvent.card, result.requestedEvent.card)
        XCTAssertEqual(result.approvedEvent.card?.kind, .approvalCard)
        XCTAssertEqual(result.attachedEvent.state, .attached)
        XCTAssertEqual(result.attachedEvent.target, .currentOceanContext)
        XCTAssertTrue(result.attachedEvent.attachment?.mockCapture == true)
        XCTAssertEqual(result.attachedEvent.attachment?.byteCount, 0)
        XCTAssertEqual(result.resultEvent.state, .result)
        XCTAssertEqual(
            result.renderedResultCard.title,
            "Photo attached to current Ocean context."
        )

        let events = await backend.events
        XCTAssertEqual(events, [result.attachedEvent])
    }

    func testPhoneUnavailableRendersClearErrorCard() async throws {
        let backend = RecordingBackend()
        let flow = OceanBuddyFlow(
            renderer: MockBuddyCardRenderer(),
            cameraBroker: MockIPhoneCameraBroker(mode: .unavailable),
            backend: backend
        )

        let outcome = try await flow.approve(flow.approvalCard())
        guard case let .failure(failure) = outcome else {
            return XCTFail("expected phone-unavailable failure")
        }

        XCTAssertEqual(failure.failedEvent.state, .failed)
        XCTAssertEqual(failure.failedEvent.failure?.code, .phoneUnavailable)
        XCTAssertEqual(failure.failedEvent.card?.kind, .errorCard)
        XCTAssertEqual(failure.renderedErrorCard.title, "Photo was not attached.")
        XCTAssertEqual(
            failure.renderedErrorCard.detail,
            "iPhone is unavailable. Bring it online and try again."
        )
        let backendEvents = await backend.events
        XCTAssertTrue(backendEvents.isEmpty)
    }

    func testMockRendererDisplaysApprovalResultAndErrorSamples() {
        let renderer = MockBuddyCardRenderer()
        let cards = [
            BuddyCard(
                id: UUID(),
                kind: .approvalCard,
                title: "Attach Photo to Current Ocean Context.",
                actions: [BuddyAction(
                    id: UUID(),
                    label: "Approve",
                    kind: .photoToContext,
                    requiresConfirmation: true,
                    targetDevice: .iPhone
                )]
            ),
            BuddyCard(id: UUID(), kind: .resultCard, title: "Photo attached."),
            BuddyCard(id: UUID(), kind: .errorCard, title: "Photo was not attached."),
        ]

        let rendered = renderer.render(cards)
        XCTAssertEqual(rendered.map(\.title), [
            "Attach Photo to Current Ocean Context.",
            "Photo attached.",
            "Photo was not attached.",
        ])
    }

    func testActionUsesRustWireFieldNames() throws {
        let action = BuddyAction(
            id: UUID(),
            label: "Approve",
            kind: .photoToContext,
            requiresConfirmation: true,
            targetDevice: .iPhone
        )
        let encoder = JSONEncoder()
        let object = try XCTUnwrap(
            JSONSerialization.jsonObject(with: encoder.encode(action)) as? [String: Any]
        )

        XCTAssertEqual(object["kind"] as? String, "photo_to_context")
        XCTAssertEqual(object["requires_confirmation"] as? Bool, true)
        XCTAssertEqual(object["target_device"] as? String, "i_phone")
    }
}
