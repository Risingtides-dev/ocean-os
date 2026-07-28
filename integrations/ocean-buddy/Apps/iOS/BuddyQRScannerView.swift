import SwiftUI
#if canImport(VisionKit)
import VisionKit
#endif

/// Full-screen QR scanner for `ocean-buddy://pair` codes shown by the Ocean
/// desktop. Uses VisionKit's DataScanner; unavailable environments (Simulator,
/// no camera permission) fall back to guidance text and manual entry.
struct BuddyQRScannerSheet: View {
    let onScanned: (String) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Group {
                #if canImport(VisionKit)
                if BuddyDataScanner.isSupported {
                    BuddyDataScanner { payload in
                        onScanned(payload)
                    }
                    .ignoresSafeArea()
                } else {
                    unavailable
                }
                #else
                unavailable
                #endif
            }
            .background(BuddyTheme.abyss)
            .navigationTitle("Scan Ocean QR")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                        .foregroundStyle(BuddyTheme.secondaryText)
                }
            }
        }
        .preferredColorScheme(.dark)
    }

    private var unavailable: some View {
        VStack(spacing: 12) {
            Image(systemName: "qrcode.viewfinder")
                .font(.system(size: 44, weight: .medium))
                .foregroundStyle(BuddyTheme.accent)
            Text("Camera scanning isn’t available here")
                .font(.headline)
                .foregroundStyle(BuddyTheme.text)
                .multilineTextAlignment(.center)
            Text("On a real iPhone, point the camera at the QR code shown by your Ocean desktop. In the Simulator, enter the address manually instead.")
                .font(.callout)
                .foregroundStyle(BuddyTheme.secondaryText)
                .multilineTextAlignment(.center)
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

#if canImport(VisionKit)
@MainActor
struct BuddyDataScanner: UIViewControllerRepresentable {
    let onScanned: (String) -> Void

    static var isSupported: Bool {
        DataScannerViewController.isSupported && DataScannerViewController.isAvailable
    }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            recognizesMultipleItems: false,
            isHighlightingEnabled: true
        )
        scanner.delegate = context.coordinator
        try? scanner.startScanning()
        return scanner
    }

    func updateUIViewController(_ uiViewController: DataScannerViewController, context: Context) {}

    func makeCoordinator() -> Coordinator {
        Coordinator(onScanned: onScanned)
    }

    @MainActor
    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        private let onScanned: (String) -> Void
        private var delivered = false

        init(onScanned: @escaping (String) -> Void) {
            self.onScanned = onScanned
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController,
            didAdd addedItems: [RecognizedItem],
            allItems: [RecognizedItem]
        ) {
            guard !delivered else { return }
            for item in addedItems {
                if case let .barcode(barcode) = item,
                   let payload = barcode.payloadStringValue,
                   !payload.isEmpty {
                    delivered = true
                    dataScanner.stopScanning()
                    onScanned(payload)
                    return
                }
            }
        }
    }
}
#endif
