// App Store screenshot harness — the in-app "shot mode" root.
//
// Launched with PUNKTFUNK_SHOT_SCENE=<name> (one of ShotScenes.all), the app shows that single
// mock-populated scene full-bleed instead of ContentView, so the OS can screenshot the REAL,
// fully-rendered UI (materials, NavigationStack, glass — all the things ImageRenderer can't
// rasterize offscreen). tools/screenshots.sh drives one launch per scene per device.
//
// Capture per platform:
//   • iOS / tvOS simulator → `xcrun simctl io booted screenshot` (native pixels = exact size).
//   • macOS → the app captures its own windows through the window server
//     (PUNKTFUNK_SHOT_SELFCAPTURE=<dir>, see MacSelfCapture): no Screen Recording grant.
//
// Every screen prints `PF_SHOT_READY scene=<name>` to stdout once it has settled, so the driver
// can wait for layout instead of guessing with a fixed sleep.

#if DEBUG
import PunktfunkKit
import SwiftUI
#if os(macOS)
import AppKit
import ImageIO
import ScreenCaptureKit
#endif

@MainActor
enum ScreenshotMode {
    /// This process was launched to capture a screenshot. Cheap enough to consult from the
    /// stores' persistence paths (`HostStore` / `PresetStore`), which must NOT write their
    /// mock contents back into a real user's App Group when the harness runs on a dev Mac.
    static var isActive: Bool {
        !(ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_SCENE"] ?? "").isEmpty
    }

    /// The scene requested via PUNKTFUNK_SHOT_SCENE, or nil for a normal launch.
    static var requestedScene: ShotScene? {
        let name = ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_SCENE"] ?? ""
        guard !name.isEmpty else { return nil }
        return ShotScenes.all.first { $0.name == name }
    }
}

/// Full-bleed host for a single scene, with per-platform window sizing / orientation and a
/// readiness ping for the capture script.
struct ScreenshotHostView: View {
    let scene: ShotScene

    init(scene: ShotScene) {
        self.scene = scene
        // Pin the palette for the capture. The console reads the LIVE `uiPalette` default,
        // and a reused Simulator (or a dev Mac) carries whatever was last picked there — the
        // Apple TV set once shipped out on a sunset palette that a test device had persisted.
        // Idempotent, and only ever runs in shot mode (this view exists behind that gate).
        UserDefaults.standard.set(
            ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_PALETTE"] ?? "violet",
            forKey: DefaultsKey.uiPalette)
    }
    var body: some View {
        scene.make()
            .environment(\.colorScheme, scene.colorScheme)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            // The scene keeps its safe area, so the HUD clears the Dynamic Island; the streamed
            // frame ignores it itself. Black matches the dark iOS window. tvOS and macOS keep the
            // system backdrop and window background the real app sits on.
            #if os(iOS)
            .background(Color.black.ignoresSafeArea())
            #endif
            #if os(macOS)
            .background(MacShotWindowConfigurator(scene: scene))
            #elseif os(iOS)
            .background(IOSOrientationConfigurator(orientation: orientation))
            #endif
            .task {
                // Let layout + materials settle, then signal the driver. PUNKTFUNK_SHOT_DELAY
                // (milliseconds) moves that moment: a scene that ANIMATES — the launch hold's
                // cover leaving its tile — is only capturable by choosing when to look at it.
                let ms = ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_DELAY"]
                    .flatMap(UInt64.init) ?? 900
                try? await Task.sleep(nanoseconds: ms * 1_000_000)
                announceReady()
            }
    }

    #if os(iOS)
    /// PUNKTFUNK_SHOT_ORIENTATION=landscape turns every scene: the iPad set is landscape.
    private var orientation: ShotOrientation {
        ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_ORIENTATION"] == "landscape"
            ? .landscape : scene.orientation
    }
    #endif

    private func announceReady() {
        print("PF_SHOT_READY scene=\(scene.name)")
        #if os(iOS)
        // The window in pixels. A landscape iPad app in a portrait simulator is drawn scaled to
        // fit, and the driver crops the screenshot to it.
        if let window = UIApplication.shared.connectedScenes
            .compactMap({ ($0 as? UIWindowScene)?.keyWindow }).first {
            let scale = window.screen.scale
            print("PF_SHOT_WINDOW_PX \(Int(window.bounds.width * scale)) "
                + "\(Int(window.bounds.height * scale))")
        }
        #endif
        fflush(stdout)
        #if os(macOS)
        MacSelfCapture.captureIfRequested(scene: scene)
        #endif
    }
}

#if os(macOS)
/// Puts the hosting window on the mac canvas and hands its rect to `MacSelfCapture`.
///
/// On a 2× display the canvas (1440×900 pt) is exactly the App Store pixels. A scene is the app's
/// own titled window, its frame on the canvas just below the menu bar, so the display needs that
/// much room under it. A `macFullScreen` scene drops the chrome, as the full-screen stream does,
/// and may sit under the menu bar: the capture takes only this app's windows. Without a 2×
/// display the window floats at 1×.
private struct MacShotWindowConfigurator: NSViewRepresentable {
    let scene: ShotScene

    func makeNSView(context: Context) -> NSView { NSView() }

    func updateNSView(_ view: NSView, context: Context) {
        DispatchQueue.main.async {
            guard let window = view.window, !context.coordinator.configured else { return }
            context.coordinator.configured = true
            // NavigationStack / Form / material chrome follow the appearance, not the SwiftUI
            // colorScheme. App-wide, so the Settings window and sheets a scene opens match.
            NSApp.appearance = NSAppearance(named: scene.colorScheme == .dark ? .darkAqua : .aqua)
            let size = ShotDevice.mac.points(scene.orientation)
            let name = scene.name
            let area = NSScreen.screens.lazy
                .filter { $0.backingScaleFactor == ShotDevice.mac.scale }
                .map { scene.macFullScreen ? $0.frame : $0.visibleFrame }
                .first { $0.width >= size.width && $0.height >= size.height }
            if scene.macFullScreen { window.styleMask = [.borderless] }
            if let area {
                let top = area.maxY.rounded(.down)
                window.setFrame(NSRect(x: area.minX, y: top - size.height,
                                       width: size.width, height: size.height), display: true)
            } else {
                window.setFrame(NSRect(origin: .zero, size: size), display: true)
                window.center()
            }
            window.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
            MacSelfCapture.canvas = Self.globalRect(window.frame)
            Self.announce(window, name, size)
        }
    }

    /// `frame` in the top-left global space that window captures take.
    private static func globalRect(_ frame: NSRect) -> CGRect {
        let top = (NSScreen.screens.first?.frame.height ?? 0) - frame.maxY
        return CGRect(x: frame.minX, y: top, width: frame.width, height: frame.height)
    }

    private static func announce(_ window: NSWindow, _ name: String, _ size: CGSize) {
        print("PF_SHOT_WINDOW=\(window.windowNumber) scene=\(name) "
            + "size=\(Int(size.width))x\(Int(size.height))pt")
        fflush(stdout)
    }

    func makeCoordinator() -> Coordinator { Coordinator() }
    final class Coordinator { var configured = false }
}

/// PUNKTFUNK_SHOT_SELFCAPTURE=<dir>: once the scene is ready the app captures its own windows
/// through ScreenCaptureKit and exits. A process may capture its own windows without a Screen
/// Recording grant (macOS 14.4+), and materials come out as on screen. Only this process's
/// windows go in, front to back over the canvas: sheets and the Settings window land in the
/// shot, the menu bar, the desktop and other apps do not.
enum MacSelfCapture {
    /// The canvas in the top-left global space, set by the window configurator.
    static var canvas: CGRect?

    static func captureIfRequested(scene: ShotScene) {
        guard let dir = ProcessInfo.processInfo.environment["PUNKTFUNK_SHOT_SELFCAPTURE"],
              !dir.isEmpty else { return }
        let outDir = URL(fileURLWithPath: (dir as NSString).expandingTildeInPath, isDirectory: true)
        try? FileManager.default.createDirectory(at: outDir, withIntermediateDirectories: true)
        let url = outDir.appendingPathComponent("\(ShotDevice.mac.id)-\(scene.name).png")
        Task { @MainActor in
            // The front sheet takes key, as a click would: an unkeyed sheet draws grey buttons.
            if let sheet = NSApp.orderedWindows.first(where: { $0.isVisible && $0.sheetParent != nil }) {
                sheet.makeKey()
                try? await Task.sleep(nanoseconds: 300_000_000)
            }
            let (shot, windows) = await captureOwnWindows()
            if let shot, let flat = flatten(shot),
               let dest = CGImageDestinationCreateWithURL(url as CFURL, "public.png" as CFString, 1, nil) {
                CGImageDestinationAddImage(dest, flat, nil)
                CGImageDestinationFinalize(dest)
                print("PF_SHOT_SAVED \(url.path) \(flat.width)x\(flat.height)px")
            } else {
                print("PF_SHOT_CAPTURE_FAILED scene=\(scene.name) windows=\(windows)")
            }
            fflush(stdout)
            exit(0)
        }
    }

    /// This app's visible windows composited back to front over `canvas` (else the front
    /// window): each window's pixels through ScreenCaptureKit, its place from AppKit — the
    /// current-process content needs no screen-recording consent but redacts window frames.
    /// Before macOS 14.4 the whole-system content does, and asks once.
    @MainActor
    private static func captureOwnWindows() async -> (CGImage?, Int) {
        let visible = NSApp.orderedWindows.filter(\.isVisible) // front to back
        guard let front = visible.first else { return (nil, 0) }
        // AppKit frames are bottom-left global; the canvas is top-left global.
        let primaryHeight = NSScreen.screens.first?.frame.height ?? 0
        func topLeft(_ f: NSRect) -> CGRect {
            CGRect(x: f.minX, y: primaryHeight - f.maxY, width: f.width, height: f.height)
        }
        let bounds = canvas ?? topLeft(front.frame)
        let scale = front.backingScaleFactor
        guard let space = CGColorSpace(name: CGColorSpace.sRGB),
              let ctx = CGContext(
                  data: nil, width: Int((bounds.width * scale).rounded()),
                  height: Int((bounds.height * scale).rounded()), bitsPerComponent: 8,
                  bytesPerRow: 0, space: space,
                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
        else { return (nil, visible.count) }
        do {
            let content: SCShareableContent
            if #available(macOS 14.4, *) {
                content = try await SCShareableContent.currentProcess
            } else {
                content = try await SCShareableContent.current
            }
            var drawn = 0
            for window in visible.reversed() {
                // macOS 27 draws an attached sheet into its parent's capture, and a capture of the
                // sheet itself returns the parent shrunk to the sheet's size.
                if #available(macOS 27, *), window.sheetParent != nil { continue }
                let frame = topLeft(window.frame)
                guard frame.intersects(bounds),
                      let scWindow = content.windows.first(where: {
                          $0.windowID == CGWindowID(window.windowNumber)
                      })
                else { continue }
                let config = SCStreamConfiguration()
                config.width = Int((frame.width * scale).rounded())
                config.height = Int((frame.height * scale).rounded())
                config.captureResolution = .best
                config.showsCursor = false
                let image = try await SCScreenshotManager.captureImage(
                    contentFilter: SCContentFilter(desktopIndependentWindow: scWindow),
                    configuration: config)
                // CG draws bottom-up: flip the window's top-left offset within the canvas.
                ctx.draw(image, in: CGRect(
                    x: (frame.minX - bounds.minX) * scale,
                    y: (bounds.maxY - frame.maxY) * scale,
                    width: frame.width * scale, height: frame.height * scale))
                drawn += 1
            }
            return (drawn > 0 ? ctx.makeImage() : nil, drawn)
        } catch {
            print("PF_SHOT_CAPTURE_ERROR \(error.localizedDescription)")
            return (nil, 0)
        }
    }

    /// A window's round corners leave transparent pixels; fill them with the window background
    /// they sit on, so the image is opaque.
    private static func flatten(_ image: CGImage) -> CGImage? {
        guard let space = CGColorSpace(name: CGColorSpace.sRGB),
              let ctx = CGContext(data: nil, width: image.width, height: image.height,
                                  bitsPerComponent: 8, bytesPerRow: 0, space: space,
                                  bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)
        else { return nil }
        var fill = NSColor.black.cgColor
        NSApp.effectiveAppearance.performAsCurrentDrawingAppearance {
            fill = NSColor.windowBackgroundColor.cgColor
        }
        let rect = CGRect(x: 0, y: 0, width: image.width, height: image.height)
        ctx.setFillColor(fill)
        ctx.fill(rect)
        ctx.draw(image, in: rect)
        return ctx.makeImage()
    }
}
#endif

#if os(iOS)
/// Orientation lock for the requested scene (landscape for the stream hero, portrait for chrome).
/// Requires the app to allow those orientations in Info.plist — it does, for both.
private struct IOSOrientationConfigurator: UIViewControllerRepresentable {
    let orientation: ShotOrientation

    func makeUIViewController(context: Context) -> ShotOrientationController {
        ShotOrientationController(mask: mask)
    }

    func updateUIViewController(_ vc: ShotOrientationController, context: Context) {
        vc.mask = mask
        vc.applyGeometry()
    }

    private var mask: UIInterfaceOrientationMask {
        orientation == .landscape ? .landscapeRight : .portrait
    }
}

/// Asks the window scene to rotate, from a place where there IS a window.
///
/// The previous version made the request inside `updateUIViewController`, where `view.window` is
/// still nil: SwiftUI makes exactly one update pass for a representable mounted as a `.background`,
/// before the hierarchy is in a window, so the `guard` fell through and nothing ever asked again.
/// Every scene declared `.landscape` — the stream hero and the trust card — was therefore captured
/// in PORTRAIT at the portrait App Store size. Overriding `supportedInterfaceOrientations` as well
/// keeps the scene from rotating back if the simulator reports a device orientation change.
final class ShotOrientationController: UIViewController {
    var mask: UIInterfaceOrientationMask

    init(mask: UIInterfaceOrientationMask) {
        self.mask = mask
        super.init(nibName: nil, bundle: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not from a nib") }

    override var supportedInterfaceOrientations: UIInterfaceOrientationMask { mask }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        applyGeometry()
    }

    func applyGeometry() {
        // `view.window` once mounted; the connected-scene lookup covers the first update pass,
        // which still runs before this controller is in a window.
        let scene = view.window?.windowScene
            ?? UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }.first
        guard let scene else { return }
        // Report a refusal instead of silently shipping the wrong orientation — that is exactly
        // how every landscape scene went out as a portrait PNG for as long as it did.
        scene.requestGeometryUpdate(.iOS(interfaceOrientations: mask)) { error in
            print("PF_SHOT_ORIENTATION_REFUSED \(error.localizedDescription)")
            fflush(stdout)
        }
        setNeedsUpdateOfSupportedInterfaceOrientations()
    }
}
#endif
#endif
