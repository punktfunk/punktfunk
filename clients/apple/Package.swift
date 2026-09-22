// swift-tools-version: 5.9
// PunktfunkKit — Swift wrapper around the punktfunk-core C ABI (punktfunk/1 client connector) plus the
// SwiftUI/VideoToolbox presentation layer. Build PunktfunkCore.xcframework first:
//   bash ../../scripts/build-xcframework.sh   (on a Mac; see README.md)
import PackageDescription

let package = Package(
    name: "PunktfunkKit",
    platforms: [.macOS(.v14), .iOS(.v17), .tvOS(.v17)],
    products: [
        .library(name: "PunktfunkKit", targets: ["PunktfunkKit"]),
        // Dependency-free foundation (stored-host model + JSON codec, settings keys, App-Group
        // constant, deep-link grammar, Live Activity attributes). A separate PRODUCT so the widget
        // extension — which must never link PunktfunkKit (Rust staticlib + presentation layer) —
        // can link this and nothing else. PunktfunkKit re-exports it (see SharedReexport.swift).
        .library(name: "PunktfunkShared", targets: ["PunktfunkShared"]),
        .executable(name: "PunktfunkClient", targets: ["PunktfunkClient"]),
    ],
    dependencies: [
        // Progressive (gradient) backdrop blur for the form screens' trays — a real blur with no
        // material tint stage (see GamepadTrayBlur). Pinned by REVISION, not `from:`: the
        // GlurBackdrop product exists only on main — no release carries it (the newest tag,
        // `1.1`, predates it, and is not three-component semver anyway, so version-based
        // resolution stops at 1.0.4). The revision is main's head at adoption time; a revision
        // pin stays reproducible when the branch moves.
        .package(
            url: "https://github.com/joogps/Glur.git",
            revision: "ba4f05d3c9a608ec773b9305f2af6089390de68a"),
    ],
    targets: [
        .binaryTarget(name: "PunktfunkCore", path: "PunktfunkCore.xcframework"),
        // No dependencies by design — an extension process links this alone.
        .target(name: "PunktfunkShared"),
        .target(
            name: "PunktfunkKit",
            dependencies: ["PunktfunkCore", "PunktfunkShared"],
            // OSS attribution shown by the app's Acknowledgements screen. Bundled here (not in the
            // app target) so it rides along via Bundle.module in both `swift build` and the Xcode
            // app, which links the PunktfunkKit product. Refresh with
            // scripts/gen-third-party-notices.sh (it copies the generated file into Resources/).
            resources: [
                .copy("Resources/THIRD-PARTY-NOTICES.txt"),
                .copy("Resources/LICENSE-MIT.txt"),
                .copy("Resources/LICENSE-APACHE.txt"),
                // Geist (SIL OFL 1.1) — the brand typeface, shared with punktfunk-website.
                // Registered with Core Text at first use; see BrandFont.swift.
                .copy("Resources/Fonts"),
                // The host cards' OS marks (template vector imagesets generated from the
                // assets/os-icons masters by scripts/gen-os-icons.sh — per-mark provenance and
                // licensing in that README). `.process` compiles the catalog; loaded via
                // OsIcon.swift.
                .process("Resources/OsIcons.xcassets"),
                // The launcher tiles' brand marks (template vector imagesets generated from the
                // assets/launcher-icons masters by scripts/gen-launcher-icons.sh — per-mark
                // provenance and licensing in that README). Loaded via LauncherIcon.swift.
                .process("Resources/LauncherIcons.xcassets"),
            ],
            linkerSettings: [
                // Rust staticlib system deps.
                .linkedFramework("Security"),
                .linkedFramework("SystemConfiguration"),
                .linkedLibrary("resolv"),
                // Skia, for the console (clients/apple/native).
                .linkedLibrary("c++"),
                .linkedFramework("Metal"),
                .linkedFramework("CoreText"),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("ImageIO"),
                .linkedFramework("ApplicationServices", .when(platforms: [.macOS])),
                .linkedFramework("UIKit", .when(platforms: [.iOS, .tvOS])),
                .linkedFramework("MobileCoreServices", .when(platforms: [.iOS, .tvOS])),
            ]
        ),
        // Development app shell (swift run PunktfunkClient): connect form → stream + input.
        // (The tvOS slide-transition package is referenced by the Xcode PROJECT only —
        // its manifest breaks SwiftPM whole-graph validation on macOS, and only the
        // Punktfunk-tvOS target links it; the #if os(tvOS) import never compiles here.)
        .executableTarget(
            name: "PunktfunkClient",
            dependencies: [
                "PunktfunkKit",
                .product(name: "GlurBackdrop", package: "Glur"),
            ]),
        // PunktfunkCore is a direct dep too so the wire tests can name the C ABI's
        // `PunktfunkInputEvent` / `PUNKTFUNK_INPUT_KIND_*` when asserting the gamepad byte layout.
        .testTarget(
            name: "PunktfunkKitTests",
            dependencies: ["PunktfunkKit", "PunktfunkShared", "PunktfunkCore"],
            resources: [
                // PyroWave golden fixtures: host-encoded AUs + upstream-decoded reference
                // planes (regenerate with punktfunk-host's `pyrowave_dump_golden` on a
                // Vulkan box — see PyroWaveDecoderTests.swift).
                .copy("PyroWaveFixtures")
            ]),
    ]
)
