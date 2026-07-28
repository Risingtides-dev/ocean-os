import Foundation
import OceanBuddyCore

/// Drives the typed Buddy card inbox on the Watch: one pending approval card at
/// a time, executed through the exact `BuddyPhotoApprovalContract` flow against
/// the configured daemon. This is the actionable capability-broker path and is
/// deliberately separate from inert model-authored realtime cards.
@MainActor
final class BuddyCardInboxController: ObservableObject {
    enum Phase: Equatable {
        case empty
        case pendingApproval(BuddyCard)
        case approving
        case outcome(RenderedBuddyCard, isError: Bool)
    }

    @Published private(set) var phase: Phase = .empty

    /// Present the one approval request supported by the first slice. Sources
    /// today: the Debug preview trigger; later, phone-forwarded requests.
    func presentPhotoApprovalRequest() {
        guard case .empty = phase else { return }
        phase = .pendingApproval(BuddyCard(
            id: UUID(),
            kind: .approvalCard,
            title: BuddyPhotoApprovalContract.cardTitle,
            actions: [BuddyAction(
                id: UUID(),
                label: BuddyPhotoApprovalContract.actionLabel,
                kind: .photoToContext,
                requiresConfirmation: true,
                targetDevice: .iPhone
            )]
        ))
    }

    func dismiss() {
        phase = .empty
    }

    func approve() async {
        guard case let .pendingApproval(card) = phase else { return }
        phase = .approving
        do {
            let reply = try await BuddyDeviceSync.shared.requestPhotoAttachment(card)
            phase = .outcome(
                MockBuddyCardRenderer().render(reply.card),
                isError: reply.isError
            )
        } catch {
            phase = .outcome(RenderedBuddyCard(
                componentID: UUID().uuidString.lowercased(),
                title: "Photo was not attached.",
                detail: String(error.localizedDescription.prefix(200)),
                buttons: []
            ), isError: true)
        }
    }
}
