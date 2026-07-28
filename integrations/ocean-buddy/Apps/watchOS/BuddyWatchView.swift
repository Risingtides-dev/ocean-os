import Accessibility
import OceanBuddyCore
import SwiftUI

struct BuddyWatchView: View {
    @StateObject private var realtime = OceanBuddyRealtimeController()
    @StateObject private var inbox = BuddyCardInboxController()
    @ObservedObject private var deviceSync = BuddyDeviceSync.shared
    @AppStorage(BuddyStorageKeys.daemonURL) private var daemonURL = BuddyAppDefaults.daemonURL
    @AppStorage(BuddyStorageKeys.sessionID) private var sessionID = ""
    @AppStorage(BuddyStorageKeys.allowInsecureLocalNetwork) private var allowInsecureLocalNetwork = false
    @Environment(\.scenePhase) private var scenePhase
    @State private var showsSettings = false

    var body: some View {
        ZStack {
            BuddyTheme.abyss.ignoresSafeArea()

            ScrollView {
                VStack(spacing: 3) {
                    header

                    inboxSection

                    OceanWaveMark(stage: realtime.stage, level: realtime.microphoneLevel, diameter: 44)

                    Text(requiresConnectionSetup ? "Connect to Ocean" : realtime.stage.buddyHeadline)
                        .font(.subheadline.weight(.semibold))
                        .foregroundStyle(BuddyTheme.text)
                        .multilineTextAlignment(.center)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)

                    voiceControl

                    if realtime.stage == .connecting {
                        Text(realtime.status)
                            .font(.caption2)
                            .foregroundStyle(BuddyTheme.secondaryText)
                            .multilineTextAlignment(.center)
                    }

                    if let error = realtime.errorMessage {
                        Label(error, systemImage: "exclamationmark.circle.fill")
                            .font(.caption2)
                            .foregroundStyle(BuddyTheme.error)
                            .multilineTextAlignment(.center)
                            .accessibilityElement(children: .combine)
                    }

                    if let card = realtime.card {
                        BuddyCardView(card: card)
                    }

                    if !realtime.transcript.isEmpty {
                        VStack(alignment: .leading, spacing: 5) {
                            Text("OCEAN")
                                .font(.system(size: 9, weight: .bold))
                                .tracking(1.4)
                                .foregroundStyle(BuddyTheme.accent)
                            Text(realtime.transcript)
                                .font(.caption2)
                                .foregroundStyle(BuddyTheme.secondaryText)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                        .padding(10)
                        .background(
                            BuddyTheme.elevated,
                            in: RoundedRectangle(cornerRadius: 10, style: .continuous)
                        )
                        .accessibilityElement(children: .combine)
                        .accessibilityLabel("Ocean transcript: \(realtime.transcript)")
                    }
                }
                .padding(.horizontal, 5)
                .padding(.bottom, 8)
            }
            .scrollIndicators(.hidden)
        }
        .sheet(isPresented: $showsSettings) {
            BuddyWatchSettingsView(
                daemonURL: $daemonURL,
                sessionID: $sessionID,
                allowInsecureLocalNetwork: $allowInsecureLocalNetwork,
                syncedAt: deviceSync.lastReceivedAt,
                onPreviewApproval: {
                    showsSettings = false
                    inbox.presentPhotoApprovalRequest()
                }
            )
        }
        .preferredColorScheme(.dark)
        .task {
            BuddyDeviceSync.shared.activate()
            #if DEBUG
            // Simulator/CI smoke hook: present (and optionally auto-approve)
            // the demo approval card without UI scripting.
            let environment = ProcessInfo.processInfo.environment
            if environment["OCEAN_BUDDY_DEMO_APPROVAL"] == "1" {
                inbox.presentPhotoApprovalRequest()
                if environment["OCEAN_BUDDY_DEMO_AUTOAPPROVE"] == "1" {
                    approvePendingCard()
                }
            }
            #endif
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background {
                realtime.stop()
            }
        }
        .onChange(of: realtime.stage) { _, stage in
            AccessibilityNotification.Announcement("Ocean status: \(stage.buddyLabel)").post()
        }
        .onChange(of: inbox.phase) { _, phase in
            if case let .outcome(rendered, _) = phase {
                AccessibilityNotification.Announcement(rendered.title).post()
            }
        }
    }

    private var header: some View {
        HStack(spacing: 5) {
            VStack(alignment: .leading, spacing: 1) {
                Text("OCEAN")
                    .font(.system(size: 9, weight: .bold))
                    .tracking(1.5)
                    .foregroundStyle(BuddyTheme.accent)
                Text(requiresConnectionSetup ? "Setup needed" : realtime.stage.buddyLabel)
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(requiresConnectionSetup ? BuddyTheme.warning : realtime.stage.buddyColor)
            }

            Spacer(minLength: 2)

            Button {
                showsSettings = true
            } label: {
                Image(systemName: "slider.horizontal.3")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(BuddyTheme.secondaryText)
                    .frame(width: 32, height: 32)
                    .background(BuddyTheme.elevated, in: Circle())
                    .frame(width: 40, height: 40)
                    .contentShape(Circle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("Connection settings")
        }
    }

    @ViewBuilder private var inboxSection: some View {
        switch inbox.phase {
        case .empty:
            EmptyView()
        case let .pendingApproval(card):
            BuddyApprovalCardView(
                card: card,
                onApprove: { approvePendingCard() },
                onDismiss: { inbox.dismiss() }
            )
        case .approving:
            HStack(spacing: 8) {
                ProgressView()
                Text("Asking iPhone…")
                    .font(.caption2)
                    .foregroundStyle(BuddyTheme.secondaryText)
            }
            .frame(maxWidth: .infinity)
            .padding(10)
            .background(BuddyTheme.elevated, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        case let .outcome(rendered, isError):
            BuddyOutcomeCardView(rendered: rendered, isError: isError) {
                inbox.dismiss()
            }
        }
    }

    private func approvePendingCard() {
        Task {
            await inbox.approve()
        }
    }

    private var voiceControl: some View {
        Button(action: toggleVoice) {
            Label(primaryActionTitle, systemImage: primaryActionSymbol)
                .font(.caption.weight(.semibold))
                .multilineTextAlignment(.center)
                .lineLimit(2)
                .frame(maxWidth: .infinity)
                .padding(.vertical, 9)
                .frame(minHeight: 40)
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
        .accessibilityHint(primaryActionHint)
    }

    private var primaryActionTitle: String {
        switch realtime.stage {
        case .connecting: return "Cancel"
        case .live, .interrupted: return "Stop"
        case .off: return endpointIsReady ? "Talk to Ocean" : "Set up"
        case .failed: return endpointIsReady ? "Try again" : "Set up"
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
}

private struct BuddyApprovalCardView: View {
    let card: BuddyCard
    let onApprove: () -> Void
    let onDismiss: () -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var dragOffset: CGFloat = 0

    private static let approveThreshold: CGFloat = 56

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Label("Approval needed", systemImage: "hand.raised.fill")
                .font(.system(size: 10, weight: .bold))
                .foregroundStyle(BuddyTheme.warning)

            Text(card.title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(BuddyTheme.text)
                .fixedSize(horizontal: false, vertical: true)

            Text("Swipe right to approve, left to dismiss.")
                .font(.system(size: 9))
                .foregroundStyle(BuddyTheme.mutedText)

            HStack(spacing: 6) {
                Button(action: onDismiss) {
                    Text("Not now")
                        .font(.caption2.weight(.semibold))
                        .frame(maxWidth: .infinity)
                        .frame(minHeight: 40)
                        .foregroundStyle(BuddyTheme.secondaryText)
                        .background(BuddyTheme.well, in: Capsule())
                }
                .buttonStyle(.plain)
                .accessibilityHint("Dismisses the approval request")

                Button(action: onApprove) {
                    Text(card.actions.first?.label ?? "Approve")
                        .font(.caption2.weight(.semibold))
                        .frame(maxWidth: .infinity)
                        .frame(minHeight: 40)
                        .foregroundStyle(Color(red: 0.012, green: 0.094, blue: 0.102))
                        .background(BuddyTheme.accent, in: Capsule())
                }
                .buttonStyle(.plain)
                .accessibilityHint("Approves and runs the mock iPhone capture")
            }
        }
        .padding(10)
        .background(BuddyTheme.elevated, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 10, style: .continuous)
                .stroke(BuddyTheme.warning.opacity(0.35), lineWidth: 1)
        )
        .offset(x: reduceMotion ? 0 : dragOffset)
        .gesture(
            DragGesture(minimumDistance: 12)
                .onChanged { value in
                    dragOffset = value.translation.width
                }
                .onEnded { value in
                    let translation = value.translation.width
                    dragOffset = 0
                    if translation > Self.approveThreshold {
                        onApprove()
                    } else if translation < -Self.approveThreshold {
                        onDismiss()
                    }
                }
        )
        .animation(reduceMotion ? nil : .easeOut(duration: 0.16), value: dragOffset)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Approval needed: \(card.title)")
    }
}

private struct BuddyOutcomeCardView: View {
    let rendered: RenderedBuddyCard
    let isError: Bool
    let onDone: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label(
                isError ? "Didn’t go through" : "Done",
                systemImage: isError ? "exclamationmark.circle.fill" : "checkmark.circle.fill"
            )
            .font(.system(size: 10, weight: .bold))
            .foregroundStyle(isError ? BuddyTheme.error : BuddyTheme.accentBright)

            Text(rendered.title)
                .font(.caption.weight(.semibold))
                .foregroundStyle(BuddyTheme.text)
                .fixedSize(horizontal: false, vertical: true)

            if let detail = rendered.detail, !detail.isEmpty {
                Text(detail)
                    .font(.system(size: 10))
                    .foregroundStyle(BuddyTheme.secondaryText)
                    .fixedSize(horizontal: false, vertical: true)
            }

            Button(action: onDone) {
                Text("OK")
                    .font(.caption2.weight(.semibold))
                    .frame(maxWidth: .infinity)
                    .frame(minHeight: 40)
                    .foregroundStyle(BuddyTheme.text)
                    .background(BuddyTheme.well, in: Capsule())
            }
            .buttonStyle(.plain)
        }
        .padding(10)
        .background(BuddyTheme.elevated, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
        .accessibilityElement(children: .combine)
    }
}

private struct BuddyWatchSettingsView: View {
    @Binding var daemonURL: String
    @Binding var sessionID: String
    @Binding var allowInsecureLocalNetwork: Bool
    let syncedAt: Date?
    let onPreviewApproval: () -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                Section("Ocean daemon") {
                    if syncedAt != nil {
                        Label("Synced from iPhone", systemImage: "iphone.and.arrow.forward")
                            .font(.caption2)
                            .foregroundStyle(BuddyTheme.accent)
                    }
                    TextField("URL", text: $daemonURL)
                        .textInputAutocapitalization(.never)
                    Text("Pair on the iPhone; the Watch fills in automatically.")
                        .font(.caption2)
                        .foregroundStyle(BuddyTheme.secondaryText)
                }

                Section("Session") {
                    TextField("Optional ID", text: $sessionID)
                        .textInputAutocapitalization(.never)
                }

                #if DEBUG
                Section("Development") {
                    Toggle("Allow LAN HTTP", isOn: $allowInsecureLocalNetwork)
                        .tint(BuddyTheme.accent)
                    Text("Prefer Tailscale HTTPS for installs.")
                        .font(.caption2)
                        .foregroundStyle(BuddyTheme.warning)
                    Button("Preview photo approval", action: onPreviewApproval)
                        .font(.caption2)
                }
                #endif

                Section("Privacy") {
                    Text("Buddy receives only a daemon-minted, short-lived voice credential and stops audio in the background.")
                        .font(.caption2)
                        .foregroundStyle(BuddyTheme.secondaryText)
                }
            }
            .navigationTitle("Connection")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                        .foregroundStyle(BuddyTheme.accent)
                }
            }
        }
        .preferredColorScheme(.dark)
    }
}
