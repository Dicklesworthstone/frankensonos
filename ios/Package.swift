// swift-tools-version: 6.0
import PackageDescription
import Foundation

let liveChecks = ProcessInfo.processInfo.environment["FSONOS_LIVE_CHECKS"] == "1"

let package = Package(
    name: "FrankenSonosSketch",
    platforms: [.macOS(.v14)],
    products: liveChecks ? [] : [.executable(name: "SketchShots", targets: ["SketchShots"])],
    targets: liveChecks ? [
        .target(name: "LiveModel", path: "FrankenSonos", exclude: ["Model/MockZoneStore.swift", "Model/SystemColors.swift", "Views", "FrankenSonosApp.swift", "SketchRenderer.swift"], sources: ["Model", "Net"], swiftSettings: [.define("LIVE_CHECKS")]),
        .testTarget(name: "LiveChecks", dependencies: ["LiveModel"], path: "Tests", resources: [.copy("Fixtures")])
    ] : [
        .executableTarget(
            name: "SketchShots",
            path: "FrankenSonos",
            swiftSettings: [.unsafeFlags(["-Xfrontend", "-disable-sandbox"])]
        )
    ],
    swiftLanguageModes: [.v5]
)
