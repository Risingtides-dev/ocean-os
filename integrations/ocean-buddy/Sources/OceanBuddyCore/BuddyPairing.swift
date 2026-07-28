import Foundation

/// A parsed `ocean-buddy://pair` payload.
///
/// Pairing deliberately carries only the daemon address and an optional Ocean
/// session ID. It never carries provider keys, minted voice credentials, or any
/// other secret: the daemon remains the credential authority and Buddy clients
/// mint short-lived secrets over the paired endpoint at connect time.
public struct BuddyPairingPayload: Equatable, Sendable {
    public let daemonURL: URL
    public let sessionID: String?
    /// True when this endpoint is acceptable only after the visible Debug
    /// cleartext-LAN switch is enabled. The UI must surface that switch flip
    /// explicitly; parsing never mutates stored settings.
    public let requiresInsecureLocalNetworkOptIn: Bool

    public init(
        daemonURL: URL,
        sessionID: String?,
        requiresInsecureLocalNetworkOptIn: Bool
    ) {
        self.daemonURL = daemonURL
        self.sessionID = sessionID
        self.requiresInsecureLocalNetworkOptIn = requiresInsecureLocalNetworkOptIn
    }
}

public enum BuddyPairingError: Error, Equatable {
    case notAPairingLink
    case unsupportedVersion
    case missingDaemonURL
    case invalidDaemonURL
    case endpointNotAllowed
    case invalidSessionID
}

/// Encoder/decoder for the QR/deep-link pairing format:
///
/// `ocean-buddy://pair?v=1&daemon=<percent-encoded URL>[&session=<uuid>]`
public enum BuddyPairingCode {
    public static let scheme = "ocean-buddy"
    public static let host = "pair"
    public static let version = 1
    public static let maximumSessionIDCharacters = 36
    public static let maximumPayloadCharacters = 1_024

    public static func encode(daemonURL: URL, sessionID: String? = nil) throws -> String {
        var components = URLComponents()
        components.scheme = scheme
        components.host = host
        var items = [
            URLQueryItem(name: "v", value: String(version)),
            URLQueryItem(name: "daemon", value: daemonURL.absoluteString),
        ]
        if let sessionID = try validatedSessionID(sessionID) {
            items.append(URLQueryItem(name: "session", value: sessionID))
        }
        components.queryItems = items
        return components.string ?? ""
    }

    /// Parse and validate one scanned/opened pairing string against the same
    /// endpoint policy used at connect time. `allowInsecureLocalNetwork` is the
    /// operator's *current* switch state; a payload that would only be legal
    /// after enabling the Debug LAN switch parses with
    /// `requiresInsecureLocalNetworkOptIn == true` instead of silently passing.
    public static func parse(
        _ raw: String,
        mode: BuddyEndpointSecurityMode,
        allowInsecureLocalNetwork: Bool
    ) throws -> BuddyPairingPayload {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.count <= maximumPayloadCharacters else {
            throw BuddyPairingError.notAPairingLink
        }
        guard let components = URLComponents(string: trimmed),
              components.scheme?.lowercased() == scheme,
              components.host?.lowercased() == host,
              components.user == nil,
              components.password == nil,
              components.port == nil,
              components.path.isEmpty,
              components.fragment == nil
        else {
            throw BuddyPairingError.notAPairingLink
        }

        let items = components.queryItems ?? []
        let allowedNames: Set<String> = ["v", "daemon", "session"]
        guard items.allSatisfy({ allowedNames.contains($0.name) && $0.value != nil }),
              items.filter({ $0.name == "v" }).count == 1,
              items.filter({ $0.name == "daemon" }).count <= 1,
              items.filter({ $0.name == "session" }).count <= 1
        else {
            throw BuddyPairingError.notAPairingLink
        }
        guard value(named: "v", in: items) == String(version) else {
            throw BuddyPairingError.unsupportedVersion
        }
        guard let daemonRaw = value(named: "daemon", in: items), !daemonRaw.isEmpty else {
            throw BuddyPairingError.missingDaemonURL
        }
        guard let daemonURL = URL(string: daemonRaw), daemonURL.scheme != nil else {
            throw BuddyPairingError.invalidDaemonURL
        }

        let allowedWithCurrentSettings = BuddyDaemonEndpointPolicy.allows(
            daemonURL,
            mode: mode,
            allowInsecureLocalNetwork: allowInsecureLocalNetwork
        )
        let allowedWithExplicitOptIn = BuddyDaemonEndpointPolicy.allows(
            daemonURL,
            mode: mode,
            allowInsecureLocalNetwork: true
        )
        guard allowedWithExplicitOptIn else {
            throw BuddyPairingError.endpointNotAllowed
        }

        return BuddyPairingPayload(
            daemonURL: daemonURL,
            sessionID: try validatedSessionID(value(named: "session", in: items)),
            requiresInsecureLocalNetworkOptIn: !allowedWithCurrentSettings
        )
    }

    /// Return Ocean's canonical UUID session id, accept an omitted/blank optional
    /// binding, and reject every non-UUID value before it can enter a URL path or
    /// daemon request body.
    public static func validatedSessionID(_ raw: String?) throws -> String? {
        guard let raw else { return nil }
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return nil }
        guard trimmed.count <= maximumSessionIDCharacters,
              let uuid = UUID(uuidString: trimmed)
        else {
            throw BuddyPairingError.invalidSessionID
        }
        return uuid.uuidString.lowercased()
    }

    private static func value(named name: String, in items: [URLQueryItem]) -> String? {
        items.first(where: { $0.name == name })?.value
    }
}
