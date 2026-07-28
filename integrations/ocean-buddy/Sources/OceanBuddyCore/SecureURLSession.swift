import Foundation
#if canImport(FoundationNetworking)
import FoundationNetworking
#endif

/// Sensitive Buddy requests never follow redirects. Rejecting every redirect is
/// simpler and safer than trying to preserve method/body/auth semantics while
/// re-evaluating a destination after the request has left its trusted origin.
final class BuddyRejectRedirectDelegate: NSObject, URLSessionTaskDelegate, @unchecked Sendable {
    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        completionHandler(nil)
    }
}

enum BuddySecureURLSession {
    static func ephemeralConfiguration(
        requestTimeout: TimeInterval = 20,
        resourceTimeout: TimeInterval = 30
    ) -> URLSessionConfiguration {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.requestCachePolicy = .reloadIgnoringLocalCacheData
        configuration.urlCache = nil
        configuration.httpShouldSetCookies = false
        configuration.httpCookieStorage = nil
        configuration.timeoutIntervalForRequest = requestTimeout
        configuration.timeoutIntervalForResource = resourceTimeout
        return configuration
    }

    static func make(configuration: URLSessionConfiguration? = nil) -> URLSession {
        URLSession(
            configuration: configuration ?? ephemeralConfiguration(),
            delegate: BuddyRejectRedirectDelegate(),
            delegateQueue: nil
        )
    }
}
