// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "OceanIMessage",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "ocean-imessage", targets: ["OceanIMessage"])],
    targets: [
        .executableTarget(
            name: "OceanIMessage",
            linkerSettings: [.linkedLibrary("sqlite3")]
        ),
        .testTarget(name: "OceanIMessageTests", dependencies: ["OceanIMessage"]),
    ]
)
