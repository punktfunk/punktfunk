// swift-tools-version: 6.0
// Punktfunk Host's menu bar item and permission onboarding: a dynamic library the Rust host
// loads at run time (Contents/Frameworks/libPunktfunkHostUI.dylib), its C ABI in
// Sources/CPunktfunkHostUI/include/pf_host_ui.h. `HostUIPreview` runs the same UI without the
// host and renders every screen to PNG offscreen.
// Run scripts/gen-l10n-hostui.sh before `swift build` or `swift test`: it writes the strings.
import PackageDescription

let package = Package(
    name: "PunktfunkHostUI",
    defaultLocalization: "en",
    platforms: [.macOS(.v15)],
    products: [
        .library(name: "PunktfunkHostUI", type: .dynamic, targets: ["PunktfunkHostUI"]),
        .executable(name: "HostUIPreview", targets: ["HostUIPreview"]),
    ],
    targets: [
        .target(name: "CPunktfunkHostUI"),
        .target(
            name: "PunktfunkHostUI",
            dependencies: ["CPunktfunkHostUI"],
            resources: [.process("Generated/Strings")],
            linkerSettings: [.linkedFramework("ServiceManagement"), .linkedFramework("CoreAudio")]),
        // The embedded Info.plist lets `-AppleLanguages` pick the language a render shows.
        .executableTarget(
            name: "HostUIPreview",
            dependencies: ["PunktfunkHostUI"],
            exclude: ["Info.plist"],
            linkerSettings: [.unsafeFlags([
                "-Xlinker", "-sectcreate", "-Xlinker", "__TEXT", "-Xlinker", "__info_plist",
                "-Xlinker", Context.packageDirectory + "/Sources/HostUIPreview/Info.plist",
            ])]),
        .testTarget(name: "PunktfunkHostUITests", dependencies: ["PunktfunkHostUI"]),
    ]
)
