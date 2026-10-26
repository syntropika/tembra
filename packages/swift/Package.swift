// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "TembraNative",
    platforms: [.iOS("15.1"), .macOS(.v11)],
    products: [.library(name: "TembraNative", targets: ["TembraNative"])],
    targets: [
        .binaryTarget(name: "TembraFFI", path: "native/TembraFFI.xcframework"),
        .target(name: "TembraNative", dependencies: ["TembraFFI"]),
        .testTarget(name: "TembraNativeTests", dependencies: ["TembraNative"]),
    ]
)
