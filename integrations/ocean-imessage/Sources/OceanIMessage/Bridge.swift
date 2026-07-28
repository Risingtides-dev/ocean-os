import Darwin
import Foundation
import SQLite3

private let sqliteTransient = unsafeBitCast(-1, to: sqlite3_destructor_type.self)

/// The only conversation this adapter is allowed to admit or send to.
public enum FixedPair {
    public static let remote = "+15717451650"
    public static let local = "+17035081859"
    public static let maximumInboundCharacters = 4_096
    public static let maximumOutboundCharacters = 1_000
    public static let maximumRepliesPerHour = 20
}

public struct IncomingMessage: Equatable {
    public let id: String
    public let remote: String
    public let local: String
    public let service: String
    public let text: String
    public let participantCount: Int
    public let isFromMe: Bool
    public let hasAttachment: Bool
    public let isReaction: Bool

    public init(
        id: String,
        remote: String,
        local: String,
        service: String,
        text: String,
        participantCount: Int,
        isFromMe: Bool,
        hasAttachment: Bool,
        isReaction: Bool
    ) {
        self.id = id
        self.remote = remote
        self.local = local
        self.service = service
        self.text = text
        self.participantCount = participantCount
        self.isFromMe = isFromMe
        self.hasAttachment = hasAttachment
        self.isReaction = isReaction
    }
}

public enum Admission: Equatable {
    case accepted(AllowedMessage)
    case rejected
}

public struct AllowedMessage: Equatable, Codable {
    public let id: String
    public let text: String

    public init(id: String, text: String) {
        self.id = id
        self.text = text
    }
}

/// This is deliberately the only content admission point. Every failure is
/// indistinguishable to callers so rejected rows are not reflected in logs,
/// output, or daemon prompts.
public func admit(_ message: IncomingMessage) -> Admission {
    guard message.id.isEmpty == false,
          message.isFromMe == false,
          message.service == "iMessage",
          message.participantCount == 1,
          message.hasAttachment == false,
          message.isReaction == false,
          normalizeE164(message.remote) == FixedPair.remote,
          normalizeE164(message.local) == FixedPair.local,
          message.text.isEmpty == false,
          message.text.count <= FixedPair.maximumInboundCharacters
    else {
        return .rejected
    }
    return .accepted(AllowedMessage(id: message.id, text: message.text))
}

public func normalizeE164(_ value: String) -> String? {
    let scalars = value.unicodeScalars
    guard scalars.first == "+" else { return nil }
    let digits = scalars.dropFirst()
    guard (8...15).contains(digits.count), digits.allSatisfy({ CharacterSet.decimalDigits.contains($0) }) else {
        return nil
    }
    return "+" + String(String.UnicodeScalarView(digits))
}

public enum BridgeError: LocalizedError, Equatable {
    case unsafeDatabaseSchema
    case databaseUnavailable
    case invalidState
    case unknownMessage
    case alreadyReplied
    case unsafeReply
    case daemonFailure
    case daemonTimeout
    case senderFailure

    public var errorDescription: String? {
        // Never include database, prompt, recipient, or Apple-event details.
        switch self {
        case .unsafeDatabaseSchema: return "Messages database is not a supported safe shape"
        case .databaseUnavailable: return "Messages database is unavailable"
        case .invalidState: return "iMessage bridge state is invalid"
        case .unknownMessage: return "message is not admitted"
        case .alreadyReplied: return "message already has a reply"
        case .unsafeReply: return "reply violates bridge policy"
        case .daemonFailure: return "Ocean did not accept the message"
        case .daemonTimeout: return "Ocean did not finish the message in time"
        case .senderFailure: return "Messages did not accept the reply"
        }
    }
}

public struct BridgeState: Codable, Equatable, Sendable {
    public var cursor: Int64
    public var admitted: [String]
    public var replied: [String]
    public var replyTimes: [Date]

    public static let empty = BridgeState(cursor: 0, admitted: [], replied: [], replyTimes: [])
}

/// State contains opaque database IDs and timestamps only. Atomic replacement
/// avoids a partial cursor advance after a crash; the file is private to the
/// local account and never contains text.
public final class StateStore {
    private let url: URL
    private let fileManager: FileManager

    public init(url: URL, fileManager: FileManager = .default) {
        self.url = url
        self.fileManager = fileManager
    }

    public func load() throws -> BridgeState {
        try withExclusiveLock { try loadUnlocked() }
    }

    public func save(_ state: BridgeState) throws {
        try withExclusiveLock { try saveUnlocked(state) }
    }

    /// Atomically admit an opaque message id. Returns false when another
    /// process already admitted it, so duplicate watchers cannot submit the same
    /// allowed body to Ocean twice.
    @discardableResult
    public func recordAdmission(_ message: AllowedMessage, cursor: Int64) throws -> Bool {
        try withExclusiveLock {
            var state = try loadUnlocked()
            guard cursor > state.cursor,
                  state.admitted.contains(message.id) == false
            else {
                return false
            }
            state.cursor = cursor
            state.admitted = Array((state.admitted + [message.id]).suffix(256))
            let retained = Set(state.admitted)
            state.replied = state.replied.filter { retained.contains($0) }
            try saveUnlocked(state)
            return true
        }
    }

    public func advanceCursor(to cursor: Int64) throws {
        try withExclusiveLock {
            var state = try loadUnlocked()
            state.cursor = max(state.cursor, cursor)
            try saveUnlocked(state)
        }
    }

    public func claimReply(for messageID: String, now: Date = Date()) throws {
        try withExclusiveLock {
            var state = try loadUnlocked()
            guard state.admitted.contains(messageID) else { throw BridgeError.unknownMessage }
            guard state.replied.contains(messageID) == false else { throw BridgeError.alreadyReplied }
            let cutoff = now.addingTimeInterval(-3600)
            state.replyTimes = state.replyTimes.filter { $0 >= cutoff }
            guard state.replyTimes.count < FixedPair.maximumRepliesPerHour else {
                throw BridgeError.unsafeReply
            }
            // Reply markers live exactly as long as their retained admission;
            // never evict one independently while its message remains claimable.
            let retained = Set(state.admitted)
            state.replied = state.replied.filter { retained.contains($0) }
            state.replied.append(messageID)
            state.replyTimes.append(now)
            try saveUnlocked(state)
        }
    }

    private func loadUnlocked() throws -> BridgeState {
        guard fileManager.fileExists(atPath: url.path) else { return .empty }
        do {
            return try JSONDecoder().decode(BridgeState.self, from: Data(contentsOf: url))
        } catch {
            throw BridgeError.invalidState
        }
    }

    private func saveUnlocked(_ state: BridgeState) throws {
        let directory = url.deletingLastPathComponent()
        let data = try JSONEncoder().encode(state)
        let temporary = directory.appendingPathComponent(".state-\(UUID().uuidString)")
        try data.write(to: temporary, options: [.atomic])
        try fileManager.setAttributes([.posixPermissions: 0o600], ofItemAtPath: temporary.path)
        if fileManager.fileExists(atPath: url.path) {
            _ = try fileManager.replaceItemAt(
                url,
                withItemAt: temporary,
                backupItemName: nil,
                options: []
            )
        } else {
            try fileManager.moveItem(at: temporary, to: url)
        }
    }

    private func withExclusiveLock<T>(_ body: () throws -> T) throws -> T {
        let directory = url.deletingLastPathComponent()
        try fileManager.createDirectory(
            at: directory,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
        let lockURL = directory.appendingPathComponent(".state.lock")
        let descriptor = Darwin.open(lockURL.path, O_CREAT | O_RDWR, S_IRUSR | S_IWUSR)
        guard descriptor >= 0 else { throw BridgeError.invalidState }
        defer { Darwin.close(descriptor) }
        var lock = flock()
        lock.l_type = Int16(F_WRLCK)
        lock.l_whence = Int16(SEEK_SET)
        guard Darwin.fcntl(descriptor, F_SETLKW, &lock) != -1 else {
            throw BridgeError.invalidState
        }
        defer {
            lock.l_type = Int16(F_UNLCK)
            _ = Darwin.fcntl(descriptor, F_SETLK, &lock)
        }
        return try body()
    }
}

public struct MessageScan {
    public let cursor: Int64
    public let admitted: [(rowID: Int64, message: AllowedMessage)]
}

public final class MessagesDatabase {
    private let path: String

    public init(path: String = ("~/Library/Messages/chat.db" as NSString).expandingTildeInPath) {
        self.path = path
    }

    public func latestRowID() throws -> Int64 {
        var database: OpaquePointer?
        guard sqlite3_open_v2(path, &database, SQLITE_OPEN_READONLY | SQLITE_OPEN_FULLMUTEX, nil) == SQLITE_OK, let database else {
            throw BridgeError.databaseUnavailable
        }
        defer { sqlite3_close(database) }
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(database, "SELECT COALESCE(MAX(ROWID), 0) FROM message", -1, &statement, nil) == SQLITE_OK, let statement else {
            throw BridgeError.unsafeDatabaseSchema
        }
        defer { sqlite3_finalize(statement) }
        guard sqlite3_step(statement) == SQLITE_ROW else { throw BridgeError.unsafeDatabaseSchema }
        return sqlite3_column_int64(statement, 0)
    }

    public func scan(after cursor: Int64) throws -> MessageScan {
        var database: OpaquePointer?
        guard sqlite3_open_v2(path, &database, SQLITE_OPEN_READONLY | SQLITE_OPEN_FULLMUTEX, nil) == SQLITE_OK, let database else {
            throw BridgeError.databaseUnavailable
        }
        defer { sqlite3_close(database) }

        let requiredMessageColumns = [
            "ROWID", "guid", "text", "handle_id", "is_from_me", "service",
            "destination_caller_id", "cache_has_attachments", "associated_message_guid",
            "associated_message_type", "item_type"
        ]
        guard try hasColumns(requiredMessageColumns, table: "message", database: database),
              try hasColumns(["ROWID", "id"], table: "handle", database: database),
              try hasColumns(["chat_id", "message_id"], table: "chat_message_join", database: database),
              try hasColumns(["chat_id", "handle_id"], table: "chat_handle_join", database: database)
        else { throw BridgeError.unsafeDatabaseSchema }

        // Do not select attributedBody, attachments, account metadata, chat names,
        // or any row that fails the SQL pre-filter. Swift repeats the allowlist
        // check before a body can leave this reader.
        let sql = """
        SELECT m.ROWID, m.guid, m.text, h.id, m.destination_caller_id, m.service,
               m.is_from_me, m.cache_has_attachments, m.associated_message_guid,
               m.associated_message_type, m.item_type,
               COUNT(DISTINCT participant.ROWID), GROUP_CONCAT(DISTINCT participant.id)
        FROM message AS m
        JOIN handle AS h ON h.ROWID = m.handle_id
        JOIN chat_message_join AS cmj ON cmj.message_id = m.ROWID
        JOIN chat_handle_join AS chj ON chj.chat_id = cmj.chat_id
        JOIN handle AS participant ON participant.ROWID = chj.handle_id
        WHERE m.ROWID > ?
          AND h.id = ?
          AND m.destination_caller_id = ?
          AND m.is_from_me = 0
          AND m.service = 'iMessage'
          AND m.text IS NOT NULL
          AND m.cache_has_attachments = 0
          AND m.associated_message_guid IS NULL
          AND COALESCE(m.associated_message_type, 0) = 0
          AND COALESCE(m.item_type, 0) = 0
        GROUP BY m.ROWID
        HAVING COUNT(DISTINCT participant.ROWID) = 1
           AND GROUP_CONCAT(DISTINCT participant.id) = ?
        ORDER BY m.ROWID ASC
        LIMIT 128
        """
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(database, sql, -1, &statement, nil) == SQLITE_OK, let statement else {
            throw BridgeError.unsafeDatabaseSchema
        }
        defer { sqlite3_finalize(statement) }
        sqlite3_bind_int64(statement, 1, cursor)
        sqlite3_bind_text(statement, 2, FixedPair.remote, -1, sqliteTransient)
        sqlite3_bind_text(statement, 3, FixedPair.local, -1, sqliteTransient)
        sqlite3_bind_text(statement, 4, FixedPair.remote, -1, sqliteTransient)

        var admitted: [(Int64, AllowedMessage)] = []
        var scannedCursor = cursor
        while sqlite3_step(statement) == SQLITE_ROW {
            let rowID = sqlite3_column_int64(statement, 0)
            scannedCursor = max(scannedCursor, rowID)
            guard let id = text(statement, 1), let body = text(statement, 2),
                  let remote = text(statement, 3), let local = text(statement, 4),
                  let service = text(statement, 5), let participants = text(statement, 12)
            else { continue }
            let candidate = IncomingMessage(
                id: id,
                remote: remote,
                local: local,
                service: service,
                text: body,
                participantCount: Int(sqlite3_column_int(statement, 11)),
                isFromMe: sqlite3_column_int(statement, 6) != 0,
                hasAttachment: sqlite3_column_int(statement, 7) != 0,
                isReaction: sqlite3_column_type(statement, 8) != SQLITE_NULL
                    || sqlite3_column_int(statement, 9) != 0
                    || sqlite3_column_int(statement, 10) != 0
            )
            // SQL prefilters the exact pair before selecting text; Swift repeats
            // both the participant and admission checks before content can leave
            // the reader boundary.
            guard participants == FixedPair.remote else { continue }
            if case let .accepted(message) = admit(candidate) {
                admitted.append((rowID, message))
            }
        }
        // Advance across rejected candidates too. Otherwise a busy unrelated
        // conversation could indefinitely occupy the bounded query window and
        // prevent the allowed row from being reached on a later poll.
        return MessageScan(cursor: scannedCursor, admitted: admitted)
    }

    private func hasColumns(_ required: [String], table: String, database: OpaquePointer) throws -> Bool {
        var statement: OpaquePointer?
        guard sqlite3_prepare_v2(database, "PRAGMA table_info(\(table))", -1, &statement, nil) == SQLITE_OK, let statement else {
            throw BridgeError.unsafeDatabaseSchema
        }
        defer { sqlite3_finalize(statement) }
        var available = Set<String>()
        while sqlite3_step(statement) == SQLITE_ROW {
            if let name = text(statement, 1) { available.insert(name) }
        }
        return Set(required).isSubset(of: available)
    }

    private func text(_ statement: OpaquePointer, _ column: Int32) -> String? {
        guard let pointer = sqlite3_column_text(statement, column) else { return nil }
        return String(cString: pointer)
    }
}

private func runMessagesAppleScript(_ source: String, arguments: [String] = []) throws -> String {
    let process = Process()
    process.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
    process.arguments = ["-e", source] + arguments
    let output = Pipe()
    process.standardOutput = output
    // Never return AppleScript diagnostics: they can contain local account
    // metadata. Callers receive only the fixed BridgeError.
    process.standardError = Pipe()
    try process.run()
    let deadline = Date().addingTimeInterval(15)
    while process.isRunning && Date() < deadline {
        Thread.sleep(forTimeInterval: 0.05)
    }
    if process.isRunning {
        process.terminate()
        throw BridgeError.senderFailure
    }
    guard process.terminationStatus == 0 else { throw BridgeError.senderFailure }
    return String(decoding: output.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
}

public enum MessagesAccounts {
    /// Returns enabled iMessage account identifiers only. This queries no chats,
    /// participants, or message content.
    public static func enabledIDs() throws -> [String] {
        let source = """
        tell application "Messages"
            get id of every account whose enabled is true and service type is iMessage
        end tell
        """
        return try runMessagesAppleScript(source)
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
    }
}

public final class FixedRecipientSender {
    private let configuredAccountID: String?

    /// Sending requires the operator-enrolled Messages account id for the local
    /// +17035081859 identity. Omission always fails closed; account ordering or a
    /// sole enabled account is never treated as identity proof.
    public init(accountID: String?) {
        self.configuredAccountID = accountID?.isEmpty == false ? accountID : nil
    }

    public func send(_ reply: String) throws {
        let clean = reply.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let accountID = configuredAccountID else { throw BridgeError.senderFailure }
        guard clean.isEmpty == false, clean.count <= FixedPair.maximumOutboundCharacters,
              clean.unicodeScalars.allSatisfy({ $0.value >= 0x20 || $0 == "\n" }),
              accountID.isEmpty == false, accountID.count <= 200,
              accountID.unicodeScalars.allSatisfy({ $0.value >= 0x20 && $0.value != 0x22 })
        else { throw BridgeError.unsafeReply }

        let source = """
        on run argv
            set replyText to item 1 of argv
            set enrolledAccountID to item 2 of argv
            tell application "Messages"
                set targetAccount to first account whose id is enrolledAccountID and enabled is true and service type is iMessage
                set targetBuddy to buddy "+15717451650" of targetAccount
                send replyText to targetBuddy
            end tell
        end run
        """
        _ = try runMessagesAppleScript(source, arguments: [clean, accountID])
    }
}
