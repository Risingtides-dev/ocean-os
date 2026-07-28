import Foundation
import OceanBuddyCore
import os
#if canImport(WatchConnectivity)
@preconcurrency import WatchConnectivity
#endif

/// Shared UserDefaults keys used by both shells and by WatchConnectivity sync.
enum BuddyStorageKeys {
    static let daemonURL = "oceanBuddy.daemonURL"
    static let sessionID = "oceanBuddy.sessionID"
    static let allowInsecureLocalNetwork = "oceanBuddy.allowInsecureLocalNetwork"
}

struct BuddyPhotoApprovalReply: Codable, Sendable {
    let card: BuddyCard
    let isError: Bool
}

private struct BuddyPhotoApprovalRequest: Codable, Sendable {
    let kind: String
    let card: BuddyCard
}

/// WatchConnectivity's legacy callback is not annotated Sendable. This wrapper
/// is confined to one delegate invocation and calls the callback exactly once.
private final class BuddyReplyHandler: @unchecked Sendable {
    private let callback: (Data) -> Void

    init(_ callback: @escaping (Data) -> Void) {
        self.callback = callback
    }

    func callAsFunction(_ data: Data) {
        callback(data)
    }
}

private enum BuddyDeviceSyncError: LocalizedError {
    case phoneUnavailable
    case invalidReply

    var errorDescription: String? {
        switch self {
        case .phoneUnavailable:
            "iPhone is unavailable. Bring it online and try again."
        case .invalidReply:
            "iPhone returned an invalid Buddy response."
        }
    }
}

/// Carries the daemon connection configuration from the iPhone to the Watch so
/// nobody types URLs on a watch. Sync moves configuration only — the daemon
/// address and optional session ID — never provider keys or minted credentials;
/// the Watch mints its own short-lived secret from the daemon at connect time.
@MainActor
final class BuddyDeviceSync: NSObject, ObservableObject {
    static let shared = BuddyDeviceSync()

    @Published private(set) var isActivated = false
    @Published private(set) var lastReceivedAt: Date?

    /// iPhone-side source of the current connection configuration, consulted
    /// whenever the session (re)activates so cold launches always publish.
    var configurationProvider:
        (@MainActor () -> (daemonURL: String, sessionID: String, allowInsecureLocalNetwork: Bool))?

    /// iPhone-side fulfillment boundary for a Watch-approved capability. The
    /// Watch never instantiates the camera broker or posts the attachment event.
    var photoApprovalHandler: (@MainActor (BuddyCard) async -> BuddyPhotoApprovalReply)?

    private let log = Logger(subsystem: "dev.risingtides.oceanbuddy", category: "pairing-sync")

    private override init() {
        super.init()
    }

    func activate() {
        #if canImport(WatchConnectivity)
        guard WCSession.isSupported() else {
            log.notice("watch connectivity unsupported on this device")
            return
        }
        let session = WCSession.default
        session.delegate = self
        if session.activationState != .activated {
            log.notice("activating watch connectivity session")
            session.activate()
        } else {
            isActivated = true
            publishFromProvider()
        }
        #endif
    }

    /// Publish the provider-supplied configuration if one is registered.
    func publishFromProvider() {
        guard let configurationProvider else { return }
        let configuration = configurationProvider()
        publishConfiguration(
            daemonURL: configuration.daemonURL,
            sessionID: configuration.sessionID,
            allowInsecureLocalNetwork: configuration.allowInsecureLocalNetwork
        )
    }

    /// iPhone-side publish of the current connection configuration.
    func publishConfiguration(
        daemonURL: String,
        sessionID: String,
        allowInsecureLocalNetwork: Bool
    ) {
        #if os(iOS) && canImport(WatchConnectivity)
        guard WCSession.isSupported() else { return }
        let session = WCSession.default
        guard session.activationState == .activated else {
            log.notice("publish skipped: session not activated")
            return
        }
        #if !targetEnvironment(simulator)
        // Simulator pairs misreport these; on device they gate useless work.
        guard session.isPaired, session.isWatchAppInstalled else {
            log.notice("publish skipped: paired=\(session.isPaired) watchApp=\(session.isWatchAppInstalled)")
            return
        }
        #endif
        do {
            try session.updateApplicationContext([
                BuddyStorageKeys.daemonURL: daemonURL,
                BuddyStorageKeys.sessionID: sessionID,
                BuddyStorageKeys.allowInsecureLocalNetwork: allowInsecureLocalNetwork,
            ])
            log.notice("published configuration context")
        } catch {
            log.error("publish failed: \(error.localizedDescription)")
        }
        #endif
    }

    /// Watch-side request for the iPhone-targeted mock capture. Interactive
    /// WatchConnectivity is intentionally required: an unreachable phone fails
    /// closed with the documented retryable error card.
    func requestPhotoAttachment(_ card: BuddyCard) async throws -> BuddyPhotoApprovalReply {
        #if os(watchOS) && canImport(WatchConnectivity)
        let session = WCSession.default
        guard session.activationState == .activated, session.isReachable else {
            throw BuddyDeviceSyncError.phoneUnavailable
        }
        let request = BuddyPhotoApprovalRequest(kind: "photo_to_context", card: card)
        let data = try JSONEncoder().encode(request)
        return try await withCheckedThrowingContinuation { continuation in
            session.sendMessageData(data) { replyData in
                do {
                    continuation.resume(
                        returning: try JSONDecoder().decode(
                            BuddyPhotoApprovalReply.self,
                            from: replyData
                        )
                    )
                } catch {
                    continuation.resume(throwing: BuddyDeviceSyncError.invalidReply)
                }
            } errorHandler: { _ in
                continuation.resume(throwing: BuddyDeviceSyncError.phoneUnavailable)
            }
        }
        #else
        throw BuddyDeviceSyncError.phoneUnavailable
        #endif
    }

    fileprivate func applyReceivedConfiguration(
        daemonURL: String?,
        sessionID: String?,
        allowInsecureLocalNetwork: Bool?
    ) {
        #if os(watchOS)
        let defaults = UserDefaults.standard
        if let daemonURL {
            defaults.set(daemonURL, forKey: BuddyStorageKeys.daemonURL)
        }
        if let sessionID {
            defaults.set(sessionID, forKey: BuddyStorageKeys.sessionID)
        }
        if let allowInsecureLocalNetwork {
            defaults.set(
                allowInsecureLocalNetwork,
                forKey: BuddyStorageKeys.allowInsecureLocalNetwork
            )
        }
        lastReceivedAt = Date()
        log.notice("applied received configuration context")
        #endif
    }
}

#if canImport(WatchConnectivity)
extension BuddyDeviceSync: WCSessionDelegate {
    nonisolated func session(
        _ session: WCSession,
        activationDidCompleteWith activationState: WCSessionActivationState,
        error: Error?
    ) {
        let activated = activationState == .activated
        Task { @MainActor in
            self.isActivated = activated
            self.log.notice("session activation completed: \(activated)")
            if activated {
                self.publishFromProvider()
            }
        }
    }

    nonisolated func session(
        _ session: WCSession,
        didReceiveApplicationContext applicationContext: [String: Any]
    ) {
        let daemonURL = applicationContext[BuddyStorageKeys.daemonURL] as? String
        let sessionID = applicationContext[BuddyStorageKeys.sessionID] as? String
        let allowInsecure =
            applicationContext[BuddyStorageKeys.allowInsecureLocalNetwork] as? Bool
        Task { @MainActor in
            self.applyReceivedConfiguration(
                daemonURL: daemonURL,
                sessionID: sessionID,
                allowInsecureLocalNetwork: allowInsecure
            )
        }
    }

    #if os(iOS)
    nonisolated func session(
        _ session: WCSession,
        didReceiveMessageData messageData: Data,
        replyHandler: @escaping (Data) -> Void
    ) {
        let sendReply = BuddyReplyHandler(replyHandler)
        Task { @MainActor in
            let reply: BuddyPhotoApprovalReply
            do {
                let request = try JSONDecoder().decode(
                    BuddyPhotoApprovalRequest.self,
                    from: messageData
                )
                guard request.kind == "photo_to_context",
                      let handler = self.photoApprovalHandler
                else {
                    throw BuddyDeviceSyncError.invalidReply
                }
                reply = await handler(request.card)
            } catch {
                reply = BuddyPhotoApprovalReply(
                    card: BuddyCard(
                        id: UUID(),
                        kind: .errorCard,
                        title: "Photo was not attached.",
                        detail: "iPhone could not validate the approval request."
                    ),
                    isError: true
                )
            }
            let encoded = (try? JSONEncoder().encode(reply)) ?? Data()
            sendReply(encoded)
        }
    }

    nonisolated func sessionDidBecomeInactive(_ session: WCSession) {}

    nonisolated func sessionDidDeactivate(_ session: WCSession) {
        session.activate()
    }
    #endif
}
#endif
