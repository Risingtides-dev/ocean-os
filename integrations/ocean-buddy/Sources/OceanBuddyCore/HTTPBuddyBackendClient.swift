import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

public enum HTTPBuddyBackendError: Error, Equatable {
    case insecureEndpoint
    case invalidResponse
    case redirectRejected
    case responseTooLarge
    case status(Int)
}

/// Thin iPhone-to-daemon client for `POST /v1/ocean-buddy/events`. The
/// endpoint policy is baked in at construction so no future caller can aim
/// this client at a forbidden origin without an explicit opt-in decision.
public struct HTTPBuddyBackendClient: BuddyBackendClient {
    static let maximumResponseBytes = 64 * 1_024

    private let endpoint: URL
    private let endpointAllowed: Bool
    private let loader: any BuddyHTTPLoading

    public init(
        baseURL: URL,
        endpointSecurityMode: BuddyEndpointSecurityMode = .release,
        allowInsecureLocalNetwork: Bool = false
    ) {
        self.init(
            baseURL: baseURL,
            loader: BuddyBoundedHTTPClient(),
            endpointSecurityMode: endpointSecurityMode,
            allowInsecureLocalNetwork: allowInsecureLocalNetwork
        )
    }

    init(
        baseURL: URL,
        loader: any BuddyHTTPLoading,
        endpointSecurityMode: BuddyEndpointSecurityMode = .release,
        allowInsecureLocalNetwork: Bool = false
    ) {
        endpoint = baseURL
            .appendingPathComponent("v1")
            .appendingPathComponent("ocean-buddy")
            .appendingPathComponent("events")
        endpointAllowed = BuddyDaemonEndpointPolicy.allows(
            baseURL,
            mode: endpointSecurityMode,
            allowInsecureLocalNetwork: allowInsecureLocalNetwork
        )
        self.loader = loader
    }

    public func send(_ event: BuddyEvent) async throws -> BuddyEventResponse {
        guard endpointAllowed else {
            throw HTTPBuddyBackendError.insecureEndpoint
        }
        var request = URLRequest(url: endpoint)
        request.httpMethod = "POST"
        request.timeoutInterval = 20
        request.cachePolicy = .reloadIgnoringLocalCacheData
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("no-store", forHTTPHeaderField: "Cache-Control")

        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        request.httpBody = try encoder.encode(event)

        let loaded: BuddyHTTPResponse
        do {
            loaded = try await loader.load(request, maximumResponseBytes: Self.maximumResponseBytes)
        } catch BuddyHTTPTransportError.responseTooLarge {
            throw HTTPBuddyBackendError.responseTooLarge
        } catch BuddyHTTPTransportError.redirectRejected {
            throw HTTPBuddyBackendError.redirectRejected
        } catch BuddyHTTPTransportError.invalidResponse {
            throw HTTPBuddyBackendError.invalidResponse
        }
        guard (200..<300).contains(loaded.response.statusCode) else {
            throw HTTPBuddyBackendError.status(loaded.response.statusCode)
        }

        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return try decoder.decode(BuddyEventResponse.self, from: loaded.data)
    }
}
