import Foundation
@testable import OceanBuddyCore
import XCTest

final class PairingTests: XCTestCase {
    private let sessionID = "00000000-0000-4000-8000-000000000042"

    func testEncodeParseRoundTripPreservesDaemonAndSession() throws {
        let encoded = try BuddyPairingCode.encode(
            daemonURL: URL(string: "https://ocean.example.com:4780")!,
            sessionID: sessionID
        )
        XCTAssertTrue(encoded.hasPrefix("ocean-buddy://pair?"))

        let payload = try BuddyPairingCode.parse(
            encoded,
            mode: .release,
            allowInsecureLocalNetwork: false
        )
        XCTAssertEqual(payload.daemonURL.absoluteString, "https://ocean.example.com:4780")
        XCTAssertEqual(payload.sessionID, sessionID)
        XCTAssertFalse(payload.requiresInsecureLocalNetworkOptIn)
    }

    func testParseAcceptsUppercaseSchemeAndOmittedSession() throws {
        let payload = try BuddyPairingCode.parse(
            "OCEAN-BUDDY://pair?v=1&daemon=https%3A%2F%2Focean.example.com",
            mode: .release,
            allowInsecureLocalNetwork: false
        )
        XCTAssertEqual(payload.daemonURL.host, "ocean.example.com")
        XCTAssertNil(payload.sessionID)
    }

    func testParseRejectsForeignLinksVersionsAndMissingDaemon() {
        XCTAssertThrowsError(try BuddyPairingCode.parse(
            "https://ocean.example.com/pair?v=1",
            mode: .release,
            allowInsecureLocalNetwork: false
        )) { XCTAssertEqual($0 as? BuddyPairingError, .notAPairingLink) }

        XCTAssertThrowsError(try BuddyPairingCode.parse(
            "ocean-buddy://join?v=1&daemon=https%3A%2F%2Focean.example.com",
            mode: .release,
            allowInsecureLocalNetwork: false
        )) { XCTAssertEqual($0 as? BuddyPairingError, .notAPairingLink) }

        XCTAssertThrowsError(try BuddyPairingCode.parse(
            "ocean-buddy://pair?v=2&daemon=https%3A%2F%2Focean.example.com",
            mode: .release,
            allowInsecureLocalNetwork: false
        )) { XCTAssertEqual($0 as? BuddyPairingError, .unsupportedVersion) }

        XCTAssertThrowsError(try BuddyPairingCode.parse(
            "ocean-buddy://pair?v=1",
            mode: .release,
            allowInsecureLocalNetwork: false
        )) { XCTAssertEqual($0 as? BuddyPairingError, .missingDaemonURL) }
    }

    func testParseNeverAcceptsCredentialedPathedOrArbitraryCleartextEndpoints() {
        for daemon in [
            "https://user:pw@ocean.example.com",
            "https://ocean.example.com/extra/path",
            "http://ocean.example.com",
            "ftp://ocean.example.com",
        ] {
            let encoded = "ocean-buddy://pair?v=1&daemon=" +
                daemon.addingPercentEncoding(withAllowedCharacters: .alphanumerics)!
            XCTAssertThrowsError(try BuddyPairingCode.parse(
                encoded,
                mode: .release,
                allowInsecureLocalNetwork: false
            ), daemon) { XCTAssertEqual($0 as? BuddyPairingError, .endpointNotAllowed) }
        }
    }

    func testParseLoopbackHTTPRequiresNoOptInInAnyMode() throws {
        let payload = try BuddyPairingCode.parse(
            "ocean-buddy://pair?v=1&daemon=http%3A%2F%2F127.0.0.1%3A4780",
            mode: .release,
            allowInsecureLocalNetwork: false
        )
        XCTAssertFalse(payload.requiresInsecureLocalNetworkOptIn)
    }

    func testParseCleartextLANIsDebugOnlyAndFlagsTheVisibleOptIn() throws {
        let encoded = "ocean-buddy://pair?v=1&daemon=" +
            "http://risings-mac-mini.local:4780"
                .addingPercentEncoding(withAllowedCharacters: .alphanumerics)!

        #if DEBUG
        let withoutOptIn = try BuddyPairingCode.parse(
            encoded,
            mode: .development,
            allowInsecureLocalNetwork: false
        )
        XCTAssertTrue(withoutOptIn.requiresInsecureLocalNetworkOptIn)

        let withOptIn = try BuddyPairingCode.parse(
            encoded,
            mode: .development,
            allowInsecureLocalNetwork: true
        )
        XCTAssertFalse(withOptIn.requiresInsecureLocalNetworkOptIn)
        #endif

        XCTAssertThrowsError(try BuddyPairingCode.parse(
            encoded,
            mode: .release,
            allowInsecureLocalNetwork: true
        )) { XCTAssertEqual($0 as? BuddyPairingError, .endpointNotAllowed) }
    }

    func testParseBoundsSessionIDsAndPayloadSize() {
        let longSession = String(repeating: "a", count: 37)
        XCTAssertThrowsError(try BuddyPairingCode.parse(
            "ocean-buddy://pair?v=1&daemon=https%3A%2F%2Focean.example.com&session=\(longSession)",
            mode: .release,
            allowInsecureLocalNetwork: false
        )) { XCTAssertEqual($0 as? BuddyPairingError, .invalidSessionID) }

        let oversized = "ocean-buddy://pair?v=1&daemon=https%3A%2F%2Focean.example.com&session=" +
            String(repeating: "b", count: BuddyPairingCode.maximumPayloadCharacters)
        XCTAssertThrowsError(try BuddyPairingCode.parse(
            oversized,
            mode: .release,
            allowInsecureLocalNetwork: false
        )) { XCTAssertEqual($0 as? BuddyPairingError, .notAPairingLink) }
    }

    func testParseRejectsUnknownDuplicateAndNonUUIDSessionFields() {
        for encoded in [
            "ocean-buddy://pair?v=1&daemon=https%3A%2F%2Focean.example.com&extra=x",
            "ocean-buddy://pair?v=1&v=1&daemon=https%3A%2F%2Focean.example.com",
            "ocean-buddy://pair?v=1&daemon=https%3A%2F%2Focean.example.com&daemon=https%3A%2F%2Fevil.example.com",
            "ocean-buddy://pair?v=1&daemon=https%3A%2F%2Focean.example.com&session=..%2Fevil",
            "ocean-buddy://pair/path?v=1&daemon=https%3A%2F%2Focean.example.com",
            "ocean-buddy://pair?v=1&daemon=https%3A%2F%2Focean.example.com#fragment",
        ] {
            XCTAssertThrowsError(try BuddyPairingCode.parse(
                encoded,
                mode: .release,
                allowInsecureLocalNetwork: false
            ), encoded)
        }
    }

    func testEncodeNeverEmitsSecretsFields() throws {
        let encoded = try BuddyPairingCode.encode(
            daemonURL: URL(string: "https://ocean.example.com")!,
            sessionID: sessionID
        )
        for forbidden in ["secret", "token", "key", "credential"] {
            XCTAssertFalse(encoded.lowercased().contains(forbidden))
        }
    }
}
