import SQLite3
import XCTest
@testable import OceanIMessage

final class OceanIMessageTests: XCTestCase {
    private func message(
        remote: String = FixedPair.remote,
        local: String = FixedPair.local,
        service: String = "iMessage",
        text: String = "hello",
        participants: Int = 1,
        fromMe: Bool = false,
        attachment: Bool = false,
        reaction: Bool = false
    ) -> IncomingMessage {
        IncomingMessage(
            id: "opaque-id", remote: remote, local: local, service: service, text: text,
            participantCount: participants, isFromMe: fromMe, hasAttachment: attachment, isReaction: reaction
        )
    }

    func testDaemonTurnAcknowledgementAcceptsCanonicalAndLegacyStatusOnly() {
        XCTAssertTrue(isAcceptedDaemonTurnStatus(202))
        XCTAssertTrue(isAcceptedDaemonTurnStatus(200))
        XCTAssertFalse(isAcceptedDaemonTurnStatus(201))
        XCTAssertFalse(isAcceptedDaemonTurnStatus(204))
        XCTAssertFalse(isAcceptedDaemonTurnStatus(500))
    }

    func testOnlyExactPairOneToOneInboundTextIsAdmitted() {
        XCTAssertEqual(admit(message()), .accepted(AllowedMessage(id: "opaque-id", text: "hello")))
    }

    func testEveryPairOrMessageShapeDeviationIsRejected() {
        let cases = [
            message(remote: "+17035081859"),
            message(local: "+15717451650"),
            message(remote: "5717451650"),
            message(service: "SMS"),
            message(participants: 2),
            message(fromMe: true),
            message(attachment: true),
            message(reaction: true),
            message(text: ""),
            message(text: String(repeating: "x", count: FixedPair.maximumInboundCharacters + 1)),
        ]
        for candidate in cases {
            XCTAssertEqual(admit(candidate), .rejected)
        }
    }

    func testStateNeverPersistsTextAndPreventsReplyReplay() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = StateStore(url: root.appendingPathComponent("state.json"))
        let accepted = AllowedMessage(id: "message-id", text: "secret body must not persist")
        XCTAssertTrue(try store.recordAdmission(accepted, cursor: 12))
        XCTAssertFalse(try store.recordAdmission(accepted, cursor: 13))
        let bytes = try String(contentsOf: root.appendingPathComponent("state.json"), encoding: .utf8)
        XCTAssertFalse(bytes.contains(accepted.text))
        XCTAssertTrue(bytes.contains(accepted.id))
        try store.claimReply(for: accepted.id, now: Date(timeIntervalSince1970: 1))
        XCTAssertThrowsError(try store.claimReply(for: accepted.id, now: Date(timeIntervalSince1970: 2))) { error in
            XCTAssertEqual(error as? BridgeError, .alreadyReplied)
        }
        XCTAssertThrowsError(try store.claimReply(for: "unknown")) { error in
            XCTAssertEqual(error as? BridgeError, .unknownMessage)
        }
    }

    func testCursorCanAdvancePastRejectedRowsWithoutPersistingBodies() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let stateURL = root.appendingPathComponent("state.json")
        let store = StateStore(url: stateURL)
        try store.advanceCursor(to: 42)
        XCTAssertEqual(try store.load().cursor, 42)
        XCTAssertFalse(try String(contentsOf: stateURL, encoding: .utf8).contains("untrusted body"))
    }

    func testReplyRateLimitFailsClosed() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = StateStore(url: root.appendingPathComponent("state.json"))
        let now = Date(timeIntervalSince1970: 1_000)
        for index in 0..<FixedPair.maximumRepliesPerHour {
            let message = AllowedMessage(id: "m-\(index)", text: "ok")
            try store.recordAdmission(message, cursor: Int64(index + 1))
            try store.claimReply(for: message.id, now: now)
        }
        let overflow = AllowedMessage(id: "overflow", text: "ok")
        try store.recordAdmission(overflow, cursor: 99)
        XCTAssertThrowsError(try store.claimReply(for: overflow.id, now: now)) { error in
            XCTAssertEqual(error as? BridgeError, .unsafeReply)
        }
    }

    func testE164NormalizationRejectsAmbiguousIdentifiers() {
        XCTAssertEqual(normalizeE164(FixedPair.remote), FixedPair.remote)
        XCTAssertNil(normalizeE164("571-745-1650"))
        XCTAssertNil(normalizeE164("7035081859"))
        XCTAssertNil(normalizeE164("+1 5717451650"))
    }

    func testDatabaseScanRejectsAllowedHandleInWrongConversation() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
        let databaseURL = root.appendingPathComponent("chat.db")
        var database: OpaquePointer?
        XCTAssertEqual(sqlite3_open(databaseURL.path, &database), SQLITE_OK)
        let db = try XCTUnwrap(database)
        defer { sqlite3_close(db) }

        try execute(db, """
        CREATE TABLE message (
          ROWID INTEGER PRIMARY KEY, guid TEXT, text TEXT, handle_id INTEGER,
          is_from_me INTEGER, service TEXT, destination_caller_id TEXT,
          cache_has_attachments INTEGER, associated_message_guid TEXT,
          associated_message_type INTEGER, item_type INTEGER
        );
        CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
        CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
        CREATE TABLE chat_handle_join (chat_id INTEGER, handle_id INTEGER);
        INSERT INTO handle VALUES (1, '+15717451650');
        INSERT INTO handle VALUES (2, '+19999999999');
        INSERT INTO message VALUES (1, 'wrong-chat', 'SENTINEL-WRONG-CHAT', 1, 0, 'iMessage', '+17035081859', 0, NULL, 0, 0);
        INSERT INTO chat_message_join VALUES (10, 1);
        INSERT INTO chat_handle_join VALUES (10, 2);
        INSERT INTO message VALUES (2, 'allowed-chat', 'allowed body', 1, 0, 'iMessage', '+17035081859', 0, NULL, 0, 0);
        INSERT INTO chat_message_join VALUES (11, 2);
        INSERT INTO chat_handle_join VALUES (11, 1);
        """)

        let scan = try MessagesDatabase(path: databaseURL.path).scan(after: 0)
        XCTAssertEqual(scan.admitted.map(\.message.id), ["allowed-chat"])
        XCTAssertFalse(scan.admitted.map(\.message.text).contains("SENTINEL-WRONG-CHAT"))
    }

    func testDelayedWatcherAndReplyMarkerEvictionCannotDuplicateWork() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: root) }
        let store = StateStore(url: root.appendingPathComponent("state.json"))

        for index in 1...256 {
            XCTAssertTrue(try store.recordAdmission(
                AllowedMessage(id: "m-\(index)", text: "allowed"),
                cursor: Int64(index)
            ))
        }
        // Reverse claim order makes m-256 the oldest reply marker while it is
        // still the newest retained admission.
        for (offset, index) in (1...256).reversed().enumerated() {
            try store.claimReply(
                for: "m-\(index)",
                now: Date(timeIntervalSince1970: Double(offset * 3_601))
            )
        }
        XCTAssertTrue(try store.recordAdmission(
            AllowedMessage(id: "m-257", text: "allowed"),
            cursor: 257
        ))
        try store.claimReply(for: "m-257", now: Date(timeIntervalSince1970: Double(257 * 3_601)))

        XCTAssertFalse(try store.recordAdmission(
            AllowedMessage(id: "m-1", text: "stale delayed watcher"),
            cursor: 1
        ))
        XCTAssertThrowsError(
            try store.claimReply(for: "m-256", now: Date(timeIntervalSince1970: Double(258 * 3_601)))
        ) { error in
            XCTAssertEqual(error as? BridgeError, .alreadyReplied)
        }
    }

    func testSenderRequiresExplicitEnrolledAccount() {
        XCTAssertThrowsError(try FixedRecipientSender(accountID: nil).send("reply")) { error in
            XCTAssertEqual(error as? BridgeError, .senderFailure)
        }
    }

    func testDaemonURLAndSSEGapPoliciesFailClosed() {
        XCTAssertTrue(isAllowedOceanDaemonURL(URL(string: "http://127.0.0.1:4780/")!))
        for raw in [
            "http://localhost:4780/",
            "https://127.0.0.1:4780/",
            "http://127.0.0.1:4781/",
            "http://127.0.0.1:4780/path",
            "http://user@127.0.0.1:4780/",
            "http://192.168.1.10:4780/",
        ] {
            XCTAssertFalse(isAllowedOceanDaemonURL(URL(string: raw)!), raw)
        }
        XCTAssertTrue(isFatalDaemonSSEEvent("error"))
        XCTAssertFalse(isFatalDaemonSSEEvent("assistant_text_delta"))
    }

    private func execute(_ database: OpaquePointer, _ sql: String) throws {
        var errorMessage: UnsafeMutablePointer<CChar>?
        let result = sqlite3_exec(database, sql, nil, nil, &errorMessage)
        defer { sqlite3_free(errorMessage) }
        guard result == SQLITE_OK else {
            throw NSError(
                domain: "OceanIMessageTests.SQLite",
                code: Int(result),
                userInfo: [NSLocalizedDescriptionKey: errorMessage.map { String(cString: $0) } ?? "unknown"]
            )
        }
    }
}
