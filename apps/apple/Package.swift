// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "FrankenSonosKit",
    platforms: [.iOS(.v17), .macOS(.v14)],
    products: [.library(name: "FrankenSonosKit", targets: ["FrankenSonosKit"])],
    targets: [
        .target(name: "FrankenSonosKit"),
        .testTarget(name: "FrankenSonosKitTests", dependencies: ["FrankenSonosKit"])
    ]
)
