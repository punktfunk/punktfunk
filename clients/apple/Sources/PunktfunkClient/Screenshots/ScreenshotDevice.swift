// App Store screenshot harness — device catalog.
//
// The harness captures the REAL running UI (not an offscreen ImageRenderer snapshot, which can't
// rasterize NavigationStack / Form / Liquid-Glass — they come out black). The app is launched in
// "shot mode" (PUNKTFUNK_SHOT_SCENE=<name>, see ScreenshotHost) showing one mock-populated scene
// full-bleed, and the OS screenshots it: `xcrun simctl io booted screenshot` on the iOS/tvOS
// simulators (native pixels = the exact App Store size). On the Mac the app captures its own
// windows (`MacSelfCapture`).
// tools/screenshots.sh drives it. DEBUG-only — none of this ships in Release.
//
// This catalog records the target App Store sizes; on Apple platforms only the mac size is read
// at runtime (to size the capture window) — the simulator IS the device, so iOS/tvOS pixels are
// whatever the booted device is.

#if DEBUG
import CoreGraphics

enum ShotOrientation { case natural, portrait, landscape }

/// A target App Store canvas: a natural-orientation pixel size + backing scale.
struct ShotDevice {
    let id: String
    let naturalWidth: Int
    let naturalHeight: Int
    let scale: CGFloat

    func pixels(_ o: ShotOrientation) -> (w: Int, h: Int) {
        let long = max(naturalWidth, naturalHeight)
        let short = min(naturalWidth, naturalHeight)
        switch o {
        case .natural: return (naturalWidth, naturalHeight)
        case .portrait: return (short, long)
        case .landscape: return (long, short)
        }
    }

    /// Logical point size (pixels / scale) — used to size the mac capture window so that a
    /// `screencapture` on a 2× display yields exactly `pixels(_:)`.
    func points(_ o: ShotOrientation) -> CGSize {
        let (w, h) = pixels(o)
        return CGSize(width: CGFloat(w) / scale, height: CGFloat(h) / scale)
    }

    /// Mac: 2880×1800 (16:10 Retina) — an accepted size; on a 1× display the window capture is
    /// 1440×900, also accepted.
    static let mac = ShotDevice(id: "mac", naturalWidth: 2880, naturalHeight: 1800, scale: 2)

    /// iPhone 6.9" (required) — for reference / the driver script's simulator choice.
    static let iphone69 = ShotDevice(id: "iphone-6.9", naturalWidth: 1320, naturalHeight: 2868,
                                     scale: 3)
    /// iPad 13" (required).
    static let ipad13 = ShotDevice(id: "ipad-13", naturalWidth: 2064, naturalHeight: 2752,
                                   scale: 2)
    /// Apple TV (always landscape).
    static let appleTV = ShotDevice(id: "appletv", naturalWidth: 1920, naturalHeight: 1080,
                                    scale: 1)
}
#endif
