// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "FrankenSonosKit",
    platforms: [.iOS(.v17), .macOS(.v14)],
    products: [
        .library(name: "FrankenSonosKit", targets: ["FrankenSonosKit"]),
        .executable(name: "FrankenSonosRemote", targets: ["FrankenSonosRemote"])
    ],
    targets: [
        .target(name: "FrankenSonosKit"),
        .executableTarget(name: "FrankenSonosRemote", dependencies: ["FrankenSonosKit"]),
        .testTarget(name: "FrankenSonosKitTests", dependencies: ["FrankenSonosKit"])
    ]
)
