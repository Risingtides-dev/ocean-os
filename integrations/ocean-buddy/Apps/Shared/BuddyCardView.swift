import OceanBuddyCore
import SwiftUI

enum BuddyTheme {
    static let abyss = Color(red: 0.024, green: 0.024, blue: 0.024)
    static let raised = Color(red: 0.039, green: 0.039, blue: 0.039)
    static let elevated = Color(red: 0.078, green: 0.078, blue: 0.078)
    static let well = Color(red: 0.137, green: 0.145, blue: 0.169)
    static let text = Color(red: 0.980, green: 0.988, blue: 1.000)
    static let secondaryText = Color(red: 0.722, green: 0.725, blue: 0.733)
    static let mutedText = Color(red: 0.565, green: 0.565, blue: 0.596)
    static let accent = Color(red: 0.000, green: 0.843, blue: 0.843)
    static let accentBright = Color(red: 0.000, green: 1.000, blue: 0.843)
    static let oceanDeep = Color(red: 0.000, green: 0.529, blue: 0.686)
    static let oceanAbyss = Color(red: 0.000, green: 0.000, blue: 0.373)
    static let error = Color(red: 1.000, green: 0.302, blue: 0.404)
    static let warning = Color(red: 1.000, green: 0.698, blue: 0.141)
}

struct BuddyCardView: View {
    let card: BuddyRealtimeCard

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            Label("From Ocean", systemImage: "sparkles")
                .font(.caption.weight(.semibold))
                .foregroundStyle(BuddyTheme.accent)

            Text(card.title)
                .font(.headline)
                .foregroundStyle(BuddyTheme.text)
                .lineLimit(3)

            if let detail = card.detail, !detail.isEmpty {
                Text(detail)
                    .font(.callout)
                    .foregroundStyle(BuddyTheme.secondaryText)
                    .lineLimit(8)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(14)
        .background(BuddyTheme.elevated, in: RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(alignment: .top) {
            Rectangle()
                .fill(.white.opacity(0.06))
                .frame(height: 1)
                .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        }
        .accessibilityElement(children: .combine)
    }
}

struct BuddyStatusLabel: View {
    let stage: BuddyRealtimeStage

    var body: some View {
        Label(stage.buddyLabel, systemImage: stage.buddyStatusSymbol)
            .font(.caption.weight(.semibold))
            .foregroundStyle(stage.buddyColor)
            .padding(.horizontal, 10)
            .padding(.vertical, 6)
            .background(BuddyTheme.elevated, in: Capsule())
            .accessibilityLabel("Ocean status: \(stage.buddyLabel)")
    }
}

struct OceanWaveMark: View {
    let stage: BuddyRealtimeStage
    let level: Float
    let diameter: CGFloat

    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var phase: CGFloat = 0

    var body: some View {
        ZStack {
            Circle()
                .fill(
                    LinearGradient(
                        colors: [BuddyTheme.raised, BuddyTheme.oceanAbyss.opacity(0.34)],
                        startPoint: .top,
                        endPoint: .bottom
                    )
                )

            Group {
                OceanWaveLine(phase: phase, verticalPosition: 0.29, amplitude: 0.035)
                    .stroke(BuddyTheme.accentBright, style: lineStyle)
                OceanWaveLine(phase: phase + 0.8, verticalPosition: 0.41, amplitude: 0.045)
                    .stroke(BuddyTheme.accent, style: lineStyle)
                OceanWaveLine(phase: phase + 1.6, verticalPosition: 0.54, amplitude: 0.052)
                    .stroke(BuddyTheme.oceanDeep, style: lineStyle)
                OceanWaveLine(phase: phase + 2.4, verticalPosition: 0.67, amplitude: 0.04)
                    .stroke(BuddyTheme.oceanAbyss.opacity(0.95), style: lineStyle)
            }
            .padding(diameter * 0.12)
            .mask(Circle().padding(diameter * 0.07))

            Circle()
                .strokeBorder(
                    LinearGradient(
                        colors: [.white.opacity(0.22), BuddyTheme.accent.opacity(0.18), .black.opacity(0.72)],
                        startPoint: .topLeading,
                        endPoint: .bottomTrailing
                    ),
                    lineWidth: max(1, diameter * 0.012)
                )

            Circle()
                .stroke(stage.buddyColor.opacity(stage == .off ? 0.10 : 0.34), lineWidth: max(1, diameter * 0.01))
                .padding(diameter * 0.055)
        }
        .frame(width: diameter, height: diameter)
        .scaleEffect(voiceScale)
        .shadow(
            color: BuddyTheme.accent.opacity(stage == .live || stage == .interrupted ? 0.22 : 0.08),
            radius: stage == .live || stage == .interrupted ? diameter * 0.16 : diameter * 0.07
        )
        .animation(reduceMotion ? nil : .linear(duration: 0.09), value: level)
        .onAppear(perform: beginWaveMotion)
        .onChange(of: reduceMotion) { _, isReduced in
            if isReduced {
                withAnimation(nil) { phase = 0 }
            } else {
                beginWaveMotion()
            }
        }
        .accessibilityHidden(true)
    }

    private var lineStyle: StrokeStyle {
        StrokeStyle(lineWidth: max(1.5, diameter * 0.018), lineCap: .round)
    }

    private var voiceScale: CGFloat {
        guard !reduceMotion, stage == .live || stage == .interrupted else { return 1 }
        return 1 + CGFloat(min(max(level, 0), 1)) * 0.055
    }

    private func beginWaveMotion() {
        guard !reduceMotion else { return }
        phase = 0
        withAnimation(.linear(duration: 6).repeatForever(autoreverses: false)) {
            phase = .pi * 2
        }
    }
}

private struct OceanWaveLine: Shape {
    var phase: CGFloat
    let verticalPosition: CGFloat
    let amplitude: CGFloat

    var animatableData: CGFloat {
        get { phase }
        set { phase = newValue }
    }

    func path(in rect: CGRect) -> Path {
        var path = Path()
        let baseline = rect.height * verticalPosition
        let step = max(1, rect.width / 80)
        var x: CGFloat = 0

        while x <= rect.width + step {
            let progress = x / max(rect.width, 1)
            let y = baseline + sin(progress * .pi * 2.2 + phase) * rect.height * amplitude
            if x == 0 {
                path.move(to: CGPoint(x: x, y: y))
            } else {
                path.addLine(to: CGPoint(x: x, y: y))
            }
            x += step
        }
        return path
    }
}

extension BuddyRealtimeStage {
    var buddyLabel: String {
        switch self {
        case .off: "Ready"
        case .connecting: "Connecting"
        case .live: "Listening"
        case .interrupted: "Listening"
        case .failed: "Unavailable"
        }
    }

    var buddyHeadline: String {
        switch self {
        case .off: "Ocean is ready"
        case .connecting: "Opening a private line"
        case .live: "I’m listening"
        case .interrupted: "Go ahead"
        case .failed: "Ocean couldn’t connect"
        }
    }

    var buddyGuidance: String {
        switch self {
        case .off: "Start a foreground conversation. Your provider key never leaves Ocean."
        case .connecting: "Securing a short-lived voice session with your Ocean daemon."
        case .live: "Speak naturally. Tap below when you’re finished."
        case .interrupted: "Ocean stopped speaking so you can continue."
        case .failed: "Check the connection, then try again."
        }
    }

    var buddyStatusSymbol: String {
        switch self {
        case .off: "circle.fill"
        case .connecting: "arrow.triangle.2.circlepath"
        case .live, .interrupted: "waveform"
        case .failed: "exclamationmark.circle.fill"
        }
    }

    var buddyColor: Color {
        switch self {
        case .off: BuddyTheme.secondaryText
        case .connecting: BuddyTheme.warning
        case .live: BuddyTheme.accentBright
        case .interrupted: BuddyTheme.accent
        case .failed: BuddyTheme.error
        }
    }
}

enum BuddyAppDefaults {
    #if DEBUG
    // Development remains explicit: this URL works only after the visible
    // cleartext-LAN switch is enabled.
    static let daemonURL = "http://risings-mac-mini.local:4780"
    static let endpointSecurityMode: BuddyEndpointSecurityMode = .development
    #else
    // Release requires the operator to provide a real HTTPS endpoint.
    static let daemonURL = ""
    static let endpointSecurityMode: BuddyEndpointSecurityMode = .release
    #endif
}
