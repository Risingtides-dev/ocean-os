import Foundation

/// Build context used to keep the shipped endpoint policy fail-closed while
/// still permitting an explicit local-network opt-in in Debug builds.
public enum BuddyEndpointSecurityMode: Sendable {
    case release
    case development
}

/// Prevents an ephemeral provider credential from crossing an arbitrary
/// cleartext origin. HTTPS and HTTP loopback are always allowed. LAN, mDNS,
/// and Tailscale-IP HTTP require both a development build and an explicit
/// opt-in; Release builds require HTTPS for every non-loopback daemon.
public enum BuddyDaemonEndpointPolicy {
    public static func allows(
        _ url: URL,
        mode: BuddyEndpointSecurityMode = .release,
        allowInsecureLocalNetwork: Bool = false
    ) -> Bool {
        guard url.user == nil,
              url.password == nil,
              url.query == nil,
              url.fragment == nil,
              url.path.isEmpty || url.path == "/",
              let scheme = url.scheme?.lowercased(),
              let host = url.host?.lowercased(),
              !host.isEmpty
        else {
            return false
        }
        if scheme == "https" {
            return true
        }
        guard scheme == "http" else { return false }
        if host == "localhost" || host == "::1" || isIPv4Loopback(host) {
            return true
        }
        #if DEBUG
        let octets = ipv4Octets(host)
        guard case .development = mode, allowInsecureLocalNetwork else {
            return false
        }
        if host.hasSuffix(".local") {
            return true
        }
        guard octets.count == 4 else { return false }
        switch (octets[0], octets[1]) {
        case (10, _), (192, 168):
            return true
        case (172, 16...31), (100, 64...127):
            return true
        default:
            return false
        }
        #else
        return false
        #endif
    }

    private static func isIPv4Loopback(_ host: String) -> Bool {
        let octets = ipv4Octets(host)
        return octets.count == 4 && octets[0] == 127
    }

    private static func ipv4Octets(_ host: String) -> [UInt8] {
        let parts = host.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 4 else { return [] }
        return parts.compactMap { part in
            guard !part.isEmpty, part.allSatisfy(\.isNumber) else { return nil }
            return UInt8(part)
        }
    }
}
