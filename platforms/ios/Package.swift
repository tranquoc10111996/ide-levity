// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "IDELevityiOS",
    platforms: [
        .iOS(.v17),
        .macOS(.v14)
    ],
    products: [
        .library(
            name: "IDELevityiOS",
            targets: ["IDELevityiOS"]
        )
    ],
    targets: [
        .systemLibrary(
            name: "ide_ffiFFI",
            path: "Sources/ide_ffiFFI"
        ),
        .target(
            name: "IDELevityiOS",
            dependencies: ["ide_ffiFFI"],
            path: "Sources/IDELevityiOS",
            swiftSettings: [
                .swiftLanguageMode(.v6)
            ]
        )
    ]
)
