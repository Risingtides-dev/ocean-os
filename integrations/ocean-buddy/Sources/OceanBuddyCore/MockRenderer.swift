/// A platform-neutral rendering result used to prove the Watch card projection
/// before introducing SwiftUI or WatchKit.
public struct RenderedBuddyCard: Equatable, Sendable {
    public let componentID: String
    public let title: String
    public let detail: String?
    public let buttons: [String]

    public init(componentID: String, title: String, detail: String?, buttons: [String]) {
        self.componentID = componentID
        self.title = title
        self.detail = detail
        self.buttons = buttons
    }
}

public protocol BuddyCardRendering: Sendable {
    func render(_ card: BuddyCard) -> RenderedBuddyCard
}

/// Mock renderer for approval and result cards. It deliberately owns no Apple UI
/// framework so the first flow is testable as a plain Swift package.
public struct MockBuddyCardRenderer: BuddyCardRendering {
    public init() {}

    public func render(_ card: BuddyCard) -> RenderedBuddyCard {
        RenderedBuddyCard(
            componentID: card.id.uuidString.lowercased(),
            title: card.title,
            detail: card.detail,
            buttons: card.actions.map(\.label)
        )
    }

    /// Render a bounded sample set (approval, result, or error cards) for tests
    /// and host previews without introducing Watch UI frameworks.
    public func render(_ cards: [BuddyCard]) -> [RenderedBuddyCard] {
        cards.map(render)
    }
}
