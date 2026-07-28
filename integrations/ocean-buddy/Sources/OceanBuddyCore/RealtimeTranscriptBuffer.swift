import Foundation

/// Bounded transcript storage for a foreground Realtime session. Provider
/// deltas may arrive indefinitely or without a matching `done` event, so reply
/// IDs, retained text, and the visible projection all have independent limits.
struct BuddyRealtimeTranscriptBuffer: Sendable {
    static let maximumReplyIdentifierCharacters = 256
    static let maximumStoredIdentifierCharacters = 4_096
    static let maximumReplyCharacters = 12_000
    static let maximumVisibleCharacters = 12_000
    static let maximumStoredCharacters = 24_000
    static let maximumReplies = 32

    private var replies: [String: String] = [:]
    private var replyOrder: [String] = []

    mutating func reset() {
        replies.removeAll(keepingCapacity: true)
        replyOrder.removeAll(keepingCapacity: true)
    }

    var storedReplyCount: Int { replies.count }
    var storedCharacterCount: Int { replies.values.reduce(0) { $0 + $1.count } }
    var storedIdentifierCharacterCount: Int { replyOrder.reduce(0) { $0 + $1.count } }

    mutating func update(replyID: String, text: String, replace: Bool) -> String {
        guard !replyID.isEmpty,
              replyID.count <= Self.maximumReplyIdentifierCharacters
        else {
            return visibleTranscript
        }

        if let existingIndex = replyOrder.firstIndex(of: replyID) {
            replyOrder.remove(at: existingIndex)
        }
        replyOrder.append(replyID)

        let next: String
        if replace {
            next = text
        } else {
            next = replies[replyID, default: ""] + text
        }
        replies[replyID] = String(next.suffix(Self.maximumReplyCharacters))

        while replyOrder.count > Self.maximumReplies
            || storedCharacterCount > Self.maximumStoredCharacters
            || storedIdentifierCharacterCount > Self.maximumStoredIdentifierCharacters {
            let evicted = replyOrder.removeFirst()
            replies.removeValue(forKey: evicted)
        }

        return visibleTranscript
    }

    private var visibleTranscript: String {
        String(
            replyOrder
                .compactMap { replies[$0] }
                .joined(separator: "\n\n")
                .suffix(Self.maximumVisibleCharacters)
        )
    }
}
