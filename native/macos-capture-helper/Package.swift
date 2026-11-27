// swift-tools-version: 6.0

import PackageDescription

let package = Package(
    name: "CaptureScribeCapture",
    platforms: [.macOS(.v15)],
    products: [
        .executable(name: "capture-scribe-capture", targets: ["CaptureScribeCapture"]),
    ],
    targets: [
        .executableTarget(
            name: "CaptureScribeCapture",
            path: "Sources/CaptureScribeCapture",
            swiftSettings: [.swiftLanguageMode(.v5)],
            plugins: [.plugin(name: "CompileRegionLocalization")]
        ),
        .executableTarget(name: "RegionLocalizationGenerator", path: "Tools/RegionLocalizationGenerator"),
        .plugin(name: "CompileRegionLocalization", capability: .buildTool(), dependencies: ["RegionLocalizationGenerator"]),
        .testTarget(name: "CaptureScribeCaptureTests", dependencies: ["CaptureScribeCapture"]),
    ]
)
