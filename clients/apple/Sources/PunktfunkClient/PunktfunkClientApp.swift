// PunktfunkClient — the macOS client app (also runs unbundled via swift run).
// Hosts grid → trust-on-first-use → StreamView (AVSampleBufferDisplayLayer HEVC) + input.

#if os(macOS)
import AppKit
#elseif os(iOS) || os(visionOS)
import UIKit
#endif
import PunktfunkKit
import SwiftUI

@main
struct PunktfunkClientApp: App {
    /// The main window's scene, for a host window that finds none open.
    static let mainSceneID = "main"

    #if os(macOS)
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    #elseif os(iOS)
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    #endif

    init() {
        // Before anything touches the core, so its first lines (identity load, the first connect's
        // transport setup) land in the log ring "Send logs to host" uploads.
        CoreLog.install()
        #if os(iOS) || os(visionOS)
        // Put Geist on the navigation titles before any bar is built.
        BrandTheme.apply()
        #endif
        Self.warmAtLaunch()
    }

    /// The identity's first load is blocking Keychain work, and most of its callers sit on the
    /// main actor. Loaded here, off it, they find both halves cached. The decoder probes follow:
    /// each is a one-off decode or shader compile that would otherwise sit on the first connect.
    private static func warmAtLaunch() {
        #if DEBUG
        if ScreenshotMode.isActive { return }
        #endif
        DispatchQueue.global(qos: .utility).async {
            if let identity = (try? ClientIdentityStore.shared.load())?.identity {
                LibraryClient.warmIdentity(identity)
            }
            _ = MetalWaveletDecoder.supported
            _ = AV1.hardwareDecodeSupported
            _ = Stage444Probe.hwDecode444_8bit
            _ = Stage444Probe.hwDecode444_10bit
        }
    }

    var body: some Scene {
        WindowGroup("Punktfunk", id: Self.mainSceneID) {
            // Pin the whole app's tint to the brand purple explicitly — the asset-catalog accent
            // resolution is environment/timing-sensitive and can fall back to system blue. Wraps the
            // screenshot harness too, so captured screens are on-brand.
            Group {
                #if DEBUG
                // PUNKTFUNK_SHOT_SCENE=<name> → show that single mock-populated screen full-bleed for
                // the App Store screenshot capture (tools/screenshots.sh). Normal launch otherwise;
                // the whole path is absent from Release builds.
                if let scene = ScreenshotMode.requestedScene {
                    ScreenshotHostView(scene: scene)
                } else {
                    ContentView()
                }
                #else
                ContentView()
                #endif
            }
            // NOT on tvOS: under the tvOS 26 glass button style a tinted UNFOCUSED control fills
            // AND labels itself in the tint — every plain Button/TextField renders as a blank
            // brand-violet pill until focused. Untinted, tvOS keeps the system glass look
            // (visible labels, white focus lift); brand color stays on explicit Color.brand uses.
            #if !os(tvOS)
            .tint(.brand)
            #endif
            // Geist Sans at each platform's own body size: the default for unstyled text, form
            // rows and fields. The phone's 17 pt shrank every tvOS control (29 pt there) and
            // bloated every Mac one (13 pt); views with an explicit size use `.geist(…)`.
            #if os(tvOS)
            .font(.geist(29, relativeTo: .body))
            #elseif os(macOS)
            .font(.geist(13, relativeTo: .body))
            #else
            .font(.geist(17, relativeTo: .body))
            #endif
            #if os(visionOS)
            .volumeBaseplateVisibility(.hidden)
            .supportedVolumeViewpoints(.front)
            #endif
        }
        // The Stream menu (Release Mouse ⌃⌥⇧Q, Disconnect ⌃⌥⇧D, Show/Hide Statistics ⌃⌥⇧S —
        // the cross-client Ctrl+Alt+Shift set) — a real menu bar on macOS, hardware-keyboard
        // shortcuts on iPad. tvOS has neither.
        #if os(macOS)
        .commands {
            StreamCommands()
            MacNavigationCommands()
        }
        #elseif !os(tvOS)
        .commands { StreamCommands() }
        #endif
        #if os(visionOS)
        // A volume: the Home with its own glass, a stream's picture a mesh on the front face, flat
        // or curved. It sizes to its content, depth included: thin for the Home and a flat
        // picture, as deep as a curve needs. Dynamic scaling grows it with distance.
        .windowStyle(.volumetric)
        .defaultSize(width: 1280, height: 720, depth: 40)
        .defaultWorldScaling(.dynamic)
        .windowResizability(.contentSize)
        #endif
        #if os(macOS)
        // A host's page, one window per host.
        WindowGroup("Host", id: MacHostWindow.sceneID, for: StoredHost.ID.self) { $hostID in
            if let hostID {
                MacHostWindow(hostID: hostID, store: .shared)
                    .tint(.brand)
            }
        }
        .defaultSize(width: 720, height: 540)
        Settings {
            // A separate scene — `.tint` does not cross scene boundaries, so re-apply the brand
            // tint here or the Preferences window falls back to the (unreliable) asset accent.
            SettingsView()
                .tint(.brand)
        }
        #endif
    }
}

#if os(macOS)
final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) {
        // `swift run` launches an unbundled binary; promote it to a regular app so the
        // window fronts and receives keyboard/mouse focus (GameController needs focus).
        NSApp.setActivationPolicy(.regular)
        NSApp.activate(ignoringOtherApps: true)
        // A second window opened over a fullscreen one would join it as a tab: hidden behind
        // the first stream, and AppKit throws when that tab enters fullscreen on its own.
        NSWindow.allowsAutomaticWindowTabbing = false
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    /// Without this a quit reads to the host as a dropped link, and it lingers the display.
    func applicationWillTerminate(_ notification: Notification) {
        SessionModel.quitAll()
        PresetStore.shared.flush()
    }
}
#elseif os(iOS)
final class AppDelegate: NSObject, UIApplicationDelegate {
    /// Before iOS 27 an attached monitor's scene arrives here and goes to
    /// `ExternalDisplaySceneDelegate`, so a stream can fill it instead of the letterboxed mirror.
    /// iOS 27 offers it only to the stream's scene accessory. Every other scene stays SwiftUI's.
    func application(
        _ application: UIApplication,
        configurationForConnecting session: UISceneSession,
        options: UIScene.ConnectionOptions
    ) -> UISceneConfiguration {
        let config = UISceneConfiguration(name: nil, sessionRole: session.role)
        if session.role == .windowExternalDisplayNonInteractive {
            config.delegateClass = ExternalDisplaySceneDelegate.self
        }
        return config
    }

    func application(
        _ application: UIApplication, supportedInterfaceOrientationsFor window: UIWindow?
    ) -> UIInterfaceOrientationMask {
        LandscapeLock.mask(for: window)
    }
}
#endif
