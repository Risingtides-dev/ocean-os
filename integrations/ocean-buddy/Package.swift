// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "OceanBuddy",
    platforms: [
        .macOS(.v14),
        .iOS(.v17),
        .watchOS(.v10),
    ],
    products: [
        .library(name: "OceanBuddyCore", targets: ["OceanBuddyCore"]),
    ],
    targets: [
        .target(name: "OceanBuddyCore"),
        .testTarget(name: "OceanBuddyCoreTests", dependencies: ["OceanBuddyCore"]),
    ]
)
