import Accessibility
import OceanBuddyCore
import SwiftUI

struct BuddyPhoneView: View {
    @StateObject private var realtime = OceanBuddyRealtimeController()
    @ObservedObject private var deviceSync = BuddyDeviceSync.shared
    @AppStorage(BuddyStorageKeys.daemonURL) private var daemonURL = BuddyAppDefaults.daemonURL
    @AppStorage(BuddyStorageKeys.sessionID) private var sessionID = ""
    @AppStorage(BuddyStorageKeys.allowInsecureLocalNetwork) private var allowInsecureLocalNetwork = false
    @Environment(\.scenePhase) private var scenePhase
    @State private var showsSettings = false
    @State private var pairingPrompt: BuddyPairingPayload?
    @State private var pairingErrorMessage: String?

    var body: some View {
        ZStack {
            BuddyTheme.abyss.ignoresSafeArea()

            VStack(spacing: 0) {
                header

                ScrollView {
                    VStack(spacing: 0) {
                        voiceStage

                        if let card = realtime.card {
                            BuddyCardView(card: card)
                                .padding(.top, 30)
                        }

                        if !realtime.transcript.isEmpty {
                            transcript
                                .padding(.top, 14)
                        }
                    }
                    .frame(maxWidth: 540)
                    .padding(.horizontal, 24)
                    .padding(.bottom, 130)
                }
                .scrollIndicators(.hidden)
            }
        }
        .safeAreaInset(edge: .bottom) {
            voiceControl
        }
        .sheet(isPresented: $showsSettings) {
            BuddyPhoneSettingsView(
                daemonURL: $daemonURL,
                sessionID: $sessionID,
                allowInsecureLocalNetwork: $allowInsecureLocalNetwork,
                pairingPrompt: $pairingPrompt,
                pairingErrorMessage: $pairingErrorMessage,
                onPairingString: handlePairingString
            )
            .presentationDetents([.medium, .large])
            .presentationDragIndicator(.visible)
        }
        .preferredColorScheme(.dark)
        .task {
            BuddyDeviceSync.shared.configurationProvider = {
                (daemonURL, sessionID, allowInsecureLocalNetwork)
            }
            BuddyDeviceSync.shared.photoApprovalHandler = { card in
                await fulfillPhotoApproval(card)
            }
            BuddyDeviceSync.shared.activate()
        }
        .onOpenURL { url in
            handlePairingString(url.absoluteString)
            showsSettings = true
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background {
                realtime.stop()
            } else if phase == .active {
                publishConfigurationToWatch()
            }
        }
        .onChange(of: realtime.stage) { _, stage in
            AccessibilityNotification.Announcement("Ocean status: \(stage.buddyLabel)").post()
        }
        .onChange(of: deviceSync.isActivated) { _, _ in publishConfigurationToWatch() }
        .onChange(of: daemonURL) { _, _ in publishConfigurationToWatch() }
        .onChange(of: sessionID) { _, _ in publishConfigurationToWatch() }
        .onChange(of: allowInsecureLocalNetwork) { _, _ in publishConfigurationToWatch() }
    }

    private var header: some View {
        HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 1) {
                Text("OCEAN")
                    .font(.caption2.weight(.bold))
                    .tracking(2.4)
                    .foregroundStyle(BuddyTheme.accent)
                Text("Buddy")
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(BuddyTheme.text)
            }

            Spacer()

            Button {
                showsSettings = true
            } label: {
                Image(systemName: "slider.horizontal.3")
                    .font(.body.weight(.semibold))
                    .foregroundStyle(BuddyTheme.secondaryText)
                    .frame(width: 44, height: 44)
                    .background(BuddyTheme.elevated, in: Circle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Connection settings")
            .accessibilityHint("Configure the Ocean daemon and optional session")
        }
        .frame(maxWidth: 540)
        .padding(.horizontal, 24)
        .padding(.top, 8)
        .padding(.bottom, 4)
    }

    private var voiceStage: some View {
        VStack(spacing: 0) {
            if !requiresConnectionSetup {
                BuddyStatusLabel(stage: realtime.stage)
            } else {
                Label("Setup needed", systemImage: "link.badge.plus")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(BuddyTheme.warning)
                    .padding(.horizontal, 10)
                    .padding(.vertical, 6)
                    .background(BuddyTheme.elevated, in: Capsule())
            }

            OceanWaveMark(stage: realtime.stage, level: realtime.microphoneLevel, diameter: 174)
                .padding(.top, 34)

            Text(requiresConnectionSetup ? "Connect to your Ocean" : realtime.stage.buddyHeadline)
                .font(.system(.title, design: .rounded, weight: .semibold))
                .foregroundStyle(BuddyTheme.text)
                .multilineTextAlignment(.center)
                .padding(.top, 30)

            Text(stageGuidance)
                .font(.body)
                .foregroundStyle(BuddyTheme.secondaryText)
                .multilineTextAlignment(.center)
                .lineSpacing(3)
                .frame(maxWidth: 390)
                .padding(.top, 10)

            if let error = realtime.errorMessage, !requiresConnectionSetup {
                Label {
                    Text(error)
                        .multilineTextAlignment(.leading)
                } icon: {
                    Image(systemName: "exclamationmark.circle.fill")
                }
                .font(.callout)
                .foregroundStyle(BuddyTheme.error)
                .frame(maxWidth: 420, alignment: .leading)
                .padding(.top, 18)
                .accessibilityElement(children: .combine)
            }
        }
        .padding(.top, 44)
    }

    private var stageGuidance: String {
        if requiresConnectionSetup {
            return "Add the secure daemon address once. Voice credentials stay short-lived and Ocean remains in control."
        }
        if realtime.stage == .connecting {
            return realtime.status
        }
        return realtime.stage.buddyGuidance
    }

    private var transcript: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label("Conversation", systemImage: "quote.bubble.fill")
                .font(.caption.weight(.semibold))
                .foregroundStyle(BuddyTheme.accent)

            Text(realtime.transcript)
                .font(.body)
                .foregroundStyle(BuddyTheme.secondaryText)
                .frame(maxWidth: .infinity, alignment: .leading)
                .accessibilityLabel("Ocean transcript")
        }
        .padding(16)
        .background(BuddyTheme.elevated, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(alignment: .top) {
            Rectangle()
                .fill(.white.opacity(0.06))
                .frame(height: 1)
                .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        }
    }

    private var voiceControl: some View {
        VStack(spacing: 0) {
            LinearGradient(
                colors: [BuddyTheme.abyss.opacity(0), BuddyTheme.abyss],
                startPoint: .top,
                endPoint: .bottom
            )
            .frame(height: 24)
            .allowsHitTesting(false)

            Button(action: toggleVoice) {
                Label(primaryActionTitle, systemImage: primaryActionSymbol)
                    .font(.headline)
                    .multilineTextAlignment(.center)
                    .lineLimit(2)
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 16)
                    .frame(minHeight: 56)
                    .foregroundStyle(primaryActionForeground)
                    .background(primaryActionBackground, in: Capsule())
                    .overlay {
                        if realtime.stage != .off && realtime.stage != .failed {
                            Capsule()
                                .stroke(realtime.stage.buddyColor.opacity(0.34), lineWidth: 1)
                        }
                    }
            }
            .buttonStyle(.plain)
            .frame(maxWidth: 540)
            .accessibilityHint(primaryActionHint)
            .padding(.horizontal, 24)
            .padding(.bottom, 10)
            .background(BuddyTheme.abyss)
        }
    }

    private var primaryActionTitle: String {
        switch realtime.stage {
        case .connecting: return "Cancel"
        case .live, .interrupted: return "End conversation"
        case .off: return endpointIsReady ? "Talk to Ocean" : "Set up connection"
        case .failed: return endpointIsReady ? "Try again" : "Set up connection"
        }
    }

    private var primaryActionSymbol: String {
        switch realtime.stage {
        case .connecting: return "xmark"
        case .live, .interrupted: return "stop.fill"
        case .off, .failed: return endpointIsReady ? "mic.fill" : "link"
        }
    }

    private var primaryActionForeground: Color {
        if endpointIsReady, realtime.stage == .off || realtime.stage == .failed {
            return Color(red: 0.012, green: 0.094, blue: 0.102)
        }
        return BuddyTheme.text
    }

    private var primaryActionBackground: Color {
        if endpointIsReady, realtime.stage == .off || realtime.stage == .failed {
            return BuddyTheme.accent
        }
        return BuddyTheme.elevated
    }

    private var primaryActionHint: String {
        switch realtime.stage {
        case .connecting: return "Stops connecting"
        case .live, .interrupted: return "Stops listening and closes the voice connection"
        case .off, .failed:
            return endpointIsReady
                ? "Starts a foreground voice conversation"
                : "Opens connection settings"
        }
    }

    private var voiceIsActive: Bool {
        switch realtime.stage {
        case .connecting, .live, .interrupted: true
        case .off, .failed: false
        }
    }

    private var requiresConnectionSetup: Bool {
        !endpointIsReady && !voiceIsActive
    }

    private var endpointIsReady: Bool {
        guard let url = configuredURL else { return false }
        return BuddyDaemonEndpointPolicy.allows(
            url,
            mode: BuddyAppDefaults.endpointSecurityMode,
            allowInsecureLocalNetwork: allowInsecureLocalNetwork
        )
    }

    private var configuredURL: URL? {
        let value = daemonURL.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !value.isEmpty else { return nil }
        return URL(string: value)
    }

    private func toggleVoice() {
        if voiceIsActive {
            realtime.stop()
            return
        }
        guard endpointIsReady, let url = configuredURL else {
            showsSettings = true
            return
        }
        realtime.start(
            baseURL: url,
            sessionID: sessionID,
            endpointSecurityMode: BuddyAppDefaults.endpointSecurityMode,
            allowInsecureLocalNetwork: allowInsecureLocalNetwork
        )
    }

    private func publishConfigurationToWatch() {
        BuddyDeviceSync.shared.publishConfiguration(
            daemonURL: daemonURL,
            sessionID: sessionID,
            allowInsecureLocalNetwork: allowInsecureLocalNetwork
        )
    }

    private func fulfillPhotoApproval(_ card: BuddyCard) async -> BuddyPhotoApprovalReply {
        guard endpointIsReady, let url = configuredURL else {
            return .init(
                card: BuddyCard(
                    id: UUID(),
                    kind: .errorCard,
                    title: "Photo was not attached.",
                    detail: "Ocean connection is not set up on iPhone."
                ),
                isError: true
            )
        }
        let flow = OceanBuddyFlow(
            renderer: MockBuddyCardRenderer(),
            cameraBroker: MockIPhoneCameraBroker(),
            backend: HTTPBuddyBackendClient(
                baseURL: url,
                endpointSecurityMode: BuddyAppDefaults.endpointSecurityMode,
                allowInsecureLocalNetwork: allowInsecureLocalNetwork
            )
        )
        do {
            switch try await flow.approve(card) {
            case let .success(result):
                return .init(
                    card: result.resultEvent.card ?? BuddyCard(
                        id: UUID(),
                        kind: .errorCard,
                        title: "Photo was not attached.",
                        detail: "Ocean returned no result card."
                    ),
                    isError: result.resultEvent.card == nil
                )
            case let .failure(failure):
                return .init(
                    card: failure.failedEvent.card ?? BuddyCard(
                        id: UUID(),
                        kind: .errorCard,
                        title: "Photo was not attached.",
                        detail: "iPhone capture failed."
                    ),
                    isError: true
                )
            }
        } catch {
            return .init(
                card: BuddyCard(
                    id: UUID(),
                    kind: .errorCard,
                    title: "Photo was not attached.",
                    detail: String(error.localizedDescription.prefix(200))
                ),
                isError: true
            )
        }
    }

    private func handlePairingString(_ raw: String) {
        do {
            pairingPrompt = try BuddyPairingCode.parse(
                raw,
                mode: BuddyAppDefaults.endpointSecurityMode,
                allowInsecureLocalNetwork: allowInsecureLocalNetwork
            )
            pairingErrorMessage = nil
        } catch let error as BuddyPairingError {
            pairingPrompt = nil
            pairingErrorMessage = error.buddyUserMessage
        } catch {
            pairingPrompt = nil
            pairingErrorMessage = "That code is not an Ocean pairing code."
        }
    }
}

extension BuddyPairingError {
    var buddyUserMessage: String {
        switch self {
        case .notAPairingLink:
            "That code is not an Ocean pairing code."
        case .unsupportedVersion:
            "This pairing code needs a newer version of Ocean Buddy."
        case .missingDaemonURL, .invalidDaemonURL:
            "The pairing code doesn’t contain a usable Ocean address."
        case .endpointNotAllowed:
            "This build can’t use that Ocean address. Use HTTPS, or a Debug build for local HTTP."
        case .invalidSessionID:
            "The pairing code’s session ID is invalid."
        }
    }
}

private struct BuddyPhoneSettingsView: View {
    @Binding var daemonURL: String
    @Binding var sessionID: String
    @Binding var allowInsecureLocalNetwork: Bool
    @Binding var pairingPrompt: BuddyPairingPayload?
    @Binding var pairingErrorMessage: String?
    let onPairingString: (String) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var showsScanner = false

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Button {
                        showsScanner = true
                    } label: {
                        Label("Scan QR from Ocean desktop", systemImage: "qrcode.viewfinder")
                            .foregroundStyle(BuddyTheme.accent)
                    }
                } header: {
                    Text("Pair")
                } footer: {
                    Text("Ocean shows a QR code that fills this in for you. Pairing codes carry the address and optional session only — never keys.")
                }

                Section {
                    TextField("https://ocean.example.com", text: $daemonURL)
                        .keyboardType(.URL)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                } header: {
                    Text("Ocean daemon")
                } footer: {
                    Text("Release builds require HTTPS outside this device. HTTP loopback is accepted for local simulator work.")
                }

                Section {
                    TextField("Optional session ID", text: $sessionID)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                } header: {
                    Text("Continue a session")
                } footer: {
                    Text("Leave this empty to let Ocean choose the voice session.")
                }

                #if DEBUG
                Section {
                    Toggle("Allow unencrypted LAN HTTP", isOn: $allowInsecureLocalNetwork)
                        .tint(BuddyTheme.accent)
                } header: {
                    Text("Development")
                } footer: {
                    Text("Debug only. Prefer Tailscale HTTPS for device installs.")
                        .foregroundStyle(BuddyTheme.warning)
                }
                #endif

                Section {
                    Label("Provider keys stay in Ocean", systemImage: "key.slash")
                    Label("Voice credentials expire", systemImage: "timer")
                    Label("Audio stops in the background", systemImage: "mic.slash")
                } header: {
                    Text("Privacy")
                }
            }
            .scrollContentBackground(.hidden)
            .background(BuddyTheme.abyss)
            .navigationTitle("Connection")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                        .foregroundStyle(BuddyTheme.accent)
                }
            }
            .sheet(isPresented: $showsScanner) {
                BuddyQRScannerSheet { raw in
                    showsScanner = false
                    onPairingString(raw)
                }
            }
            .alert(
                "Connect to this Ocean?",
                isPresented: Binding(
                    get: { pairingPrompt != nil },
                    set: { if !$0 { pairingPrompt = nil } }
                ),
                presenting: pairingPrompt
            ) { payload in
                Button("Connect") { apply(payload) }
                Button("Cancel", role: .cancel) {}
            } message: { payload in
                Text(pairingSummary(payload))
            }
            .alert(
                "Pairing failed",
                isPresented: Binding(
                    get: { pairingErrorMessage != nil },
                    set: { if !$0 { pairingErrorMessage = nil } }
                ),
                presenting: pairingErrorMessage
            ) { _ in
                Button("OK", role: .cancel) {}
            } message: { message in
                Text(message)
            }
        }
        .preferredColorScheme(.dark)
    }

    private func apply(_ payload: BuddyPairingPayload) {
        daemonURL = payload.daemonURL.absoluteString
        sessionID = payload.sessionID ?? ""
        if payload.requiresInsecureLocalNetworkOptIn {
            // Debug-only path: the visible LAN switch flips on as part of this
            // explicit confirmation and remains visible in this sheet.
            allowInsecureLocalNetwork = true
        }
    }

    private func pairingSummary(_ payload: BuddyPairingPayload) -> String {
        var lines = [payload.daemonURL.absoluteString]
        if let session = payload.sessionID {
            lines.append("Session: \(session)")
        }
        if payload.requiresInsecureLocalNetworkOptIn {
            lines.append("This enables the unencrypted LAN HTTP switch (Debug only).")
        }
        lines.append("The Watch gets this automatically from your iPhone.")
        return lines.joined(separator: "\n")
    }
}
