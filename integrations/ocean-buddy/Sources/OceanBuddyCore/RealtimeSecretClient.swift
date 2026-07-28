import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

protocol BuddyHTTPLoading: Sendable {
    func load(_ request: URLRequest, maximumResponseBytes: Int) async throws -> BuddyHTTPResponse
}

struct BuddyHTTPResponse: Sendable {
    let data: Data
    let response: HTTPURLResponse
}

enum BuddyHTTPTransportError: Error, Equatable, Sendable {
    case invalidResponse
    case redirectRejected
    case responseTooLarge
}

/// Per-request, fail-closed HTTP loader. Redirects are never followed and body
/// bytes are capped while URLSession delivers them, before decoding or copying
/// them into a model.
struct BuddyBoundedHTTPClient: BuddyHTTPLoading, @unchecked Sendable {
    private let configuration: URLSessionConfiguration

    init(configuration: URLSessionConfiguration = HTTPBuddyRealtimeSecretClient.ephemeralConfiguration()) {
        self.configuration = configuration
    }

    func load(_ request: URLRequest, maximumResponseBytes: Int) async throws -> BuddyHTTPResponse {
        try await withCheckedThrowingContinuation { continuation in
            let transfer = BuddyBoundedHTTPTransfer(
                configuration: configuration,
                request: request,
                maximumResponseBytes: maximumResponseBytes,
                continuation: continuation
            )
            transfer.start()
        }
    }
}

private final class BuddyBoundedHTTPTransfer: NSObject, URLSessionDataDelegate, @unchecked Sendable {
    private let configuration: URLSessionConfiguration
    private let request: URLRequest
    private let maximumResponseBytes: Int
    private var continuation: CheckedContinuation<BuddyHTTPResponse, Error>?
    private var session: URLSession?
    private var response: HTTPURLResponse?
    private var data = Data()
    private var terminalError: Error?

    init(
        configuration: URLSessionConfiguration,
        request: URLRequest,
        maximumResponseBytes: Int,
        continuation: CheckedContinuation<BuddyHTTPResponse, Error>
    ) {
        self.configuration = configuration
        self.request = request
        self.maximumResponseBytes = maximumResponseBytes
        self.continuation = continuation
    }

    func start() {
        let queue = OperationQueue()
        queue.maxConcurrentOperationCount = 1
        let session = URLSession(configuration: configuration, delegate: self, delegateQueue: queue)
        self.session = session
        session.dataTask(with: request).resume()
    }

    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        terminalError = BuddyHTTPTransportError.redirectRejected
        completionHandler(nil)
    }

    func urlSession(
        _ session: URLSession,
        dataTask: URLSessionDataTask,
        didReceive response: URLResponse,
        completionHandler: @escaping (URLSession.ResponseDisposition) -> Void
    ) {
        guard terminalError == nil,
              let http = response as? HTTPURLResponse
        else {
            completionHandler(.cancel)
            return
        }
        guard response.expectedContentLength < 0
                || response.expectedContentLength <= Int64(maximumResponseBytes)
        else {
            terminalError = BuddyHTTPTransportError.responseTooLarge
            completionHandler(.cancel)
            return
        }
        self.response = http
        completionHandler(.allow)
    }

    func urlSession(_ session: URLSession, dataTask: URLSessionDataTask, didReceive chunk: Data) {
        guard terminalError == nil else { return }
        guard chunk.count <= maximumResponseBytes - data.count else {
            terminalError = BuddyHTTPTransportError.responseTooLarge
            dataTask.cancel()
            return
        }
        data.append(chunk)
    }

    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        didCompleteWithError error: Error?
    ) {
        let result: Result<BuddyHTTPResponse, Error>
        if let terminalError {
            result = .failure(terminalError)
        } else if let error {
            result = .failure(error)
        } else if let response {
            result = .success(BuddyHTTPResponse(data: data, response: response))
        } else {
            result = .failure(BuddyHTTPTransportError.invalidResponse)
        }

        let continuation = self.continuation
        self.continuation = nil
        self.session?.invalidateAndCancel()
        self.session = nil
        continuation?.resume(with: result)
    }
}

public protocol BuddyRealtimeSecretProviding: Sendable {
    func mint(sessionID: String?, model: String?) async throws -> BuddyRealtimeSecret
}

public enum BuddyRealtimeSecretError: Error, Equatable, LocalizedError {
    case insecureEndpoint
    case invalidSessionID
    case invalidResponse
    case redirectRejected
    case responseTooLarge
    case status(Int, String)
    case emptyCredential
    case expiredCredential

    public var errorDescription: String? {
        switch self {
        case .insecureEndpoint:
            "Ocean voice credentials require HTTPS or loopback; Debug builds may explicitly allow local HTTP."
        case .invalidSessionID:
            "Ocean session IDs must be UUIDs."
        case .invalidResponse:
            "Ocean returned an invalid voice credential response."
        case .redirectRejected:
            "Ocean voice credential redirects are not allowed."
        case .responseTooLarge:
            "Ocean returned an oversized voice credential response."
        case let .status(code, message):
            "Ocean voice credential failed (\(code)): \(message)"
        case .emptyCredential:
            "Ocean returned an empty voice credential."
        case .expiredCredential:
            "Ocean returned an expired voice credential."
        }
    }
}

/// Thin client for the daemon-owned ephemeral credential seam. It never reads
/// or stores an OpenAI API key.
public struct HTTPBuddyRealtimeSecretClient: BuddyRealtimeSecretProviding {
    private static let maximumResponseBytes = 64 * 1_024

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
            .appendingPathComponent("voice")
            .appendingPathComponent("realtime")
            .appendingPathComponent("client-secret")
        endpointAllowed = BuddyDaemonEndpointPolicy.allows(
            baseURL,
            mode: endpointSecurityMode,
            allowInsecureLocalNetwork: allowInsecureLocalNetwork
        )
        self.loader = loader
    }

    /// Internal for pure configuration verification. Production callers receive
    /// a fresh loader-owned session rather than `URLSession.shared`.
    static func ephemeralConfiguration() -> URLSessionConfiguration {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.urlCache = nil
        configuration.httpShouldSetCookies = false
        configuration.httpCookieStorage = nil
        configuration.timeoutIntervalForRequest = 20
        configuration.timeoutIntervalForResource = 30
        return configuration
    }

    public func mint(sessionID: String?, model: String?) async throws -> BuddyRealtimeSecret {
        guard endpointAllowed else {
            throw BuddyRealtimeSecretError.insecureEndpoint
        }
        var body: [String: String] = ["purpose": "conversation"]
        let canonicalSessionID: String?
        do {
            canonicalSessionID = try BuddyPairingCode.validatedSessionID(sessionID)
        } catch {
            throw BuddyRealtimeSecretError.invalidSessionID
        }
        if let canonicalSessionID {
            body["session_id"] = canonicalSessionID
        }
        if let model = model?.trimmingCharacters(in: .whitespacesAndNewlines), !model.isEmpty {
            body["model"] = model
        }

        var request = URLRequest(url: endpoint)
        request.httpMethod = "POST"
        request.cachePolicy = .reloadIgnoringLocalCacheData
        request.timeoutInterval = 20
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.setValue("no-store", forHTTPHeaderField: "Cache-Control")
        request.httpBody = try JSONEncoder().encode(body)

        let loaded: BuddyHTTPResponse
        do {
            loaded = try await loader.load(request, maximumResponseBytes: Self.maximumResponseBytes)
        } catch BuddyHTTPTransportError.redirectRejected {
            throw BuddyRealtimeSecretError.redirectRejected
        } catch BuddyHTTPTransportError.responseTooLarge {
            throw BuddyRealtimeSecretError.responseTooLarge
        } catch BuddyHTTPTransportError.invalidResponse {
            throw BuddyRealtimeSecretError.invalidResponse
        }
        let data = loaded.data
        let http = loaded.response
        guard (200..<300).contains(http.statusCode) else {
            let upstream = (try? JSONSerialization.jsonObject(with: data) as? [String: Any])
            let raw = (upstream?["error"] as? String) ?? "request failed"
            throw BuddyRealtimeSecretError.status(http.statusCode, String(raw.prefix(500)))
        }

        let secret = try JSONDecoder().decode(BuddyRealtimeSecret.self, from: data)
        guard !secret.clientSecret.isEmpty, !secret.model.isEmpty else {
            throw BuddyRealtimeSecretError.emptyCredential
        }
        return secret
    }
}
