// swift-tools-version: 6.2
import PackageDescription

let package = Package(
    name: "CaptureScribeAppleSpeech",
    platforms: [.macOS(.v26)],
    products: [
        .executable(name: "apple-transcriber", targets: ["AppleTranscriber"])
    ],
    targets: [
        .executableTarget(
            name: "AppleTranscriber",
            path: "Sources/AppleTranscriber"
        ),
        .testTarget(name: "AppleTranscriberTests", dependencies: ["AppleTranscriber"])
    ]
)
