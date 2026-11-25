// This device's display, as `EffectiveSettings.streamMode(native:)` reads a Native setting.

#if os(macOS)
import AppKit
#else
import UIKit
#endif
#if os(tvOS)
import AVFoundation
#endif

@MainActor
public enum NativeDisplay {
    /// Whether this display can present HDR: what a connect advertises, so the host sends an
    /// SDR stream to an SDR display. Each platform's answer is the mode-independent one. macOS
    /// reads the potential headroom (the current one is 1.0 on an idle desktop); tvOS asks the
    /// TV's capability (its headroom follows the current output mode, SDR on the home screen).
    public static var presentsHDR: Bool {
        #if os(macOS)
        return (NSScreen.main?.maximumPotentialExtendedDynamicRangeColorComponentValue ?? 1.0) > 1.0
        #elseif os(tvOS)
        return AVPlayer.eligibleForHDRPlayback
        #elseif os(visionOS)
        return true
        #else
        return screen.potentialEDRHeadroom > 1.0
        #endif
    }

    #if os(iOS) || os(tvOS)
    /// The screen the app's window is on. A foldable has two built-in screens and `UIScreen.main`
    /// stays the cover one, so the key window decides. An attached monitor's scene never counts.
    public static var screen: UIScreen {
        let scenes = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .filter { $0.session.role == .windowApplication }
        let key = scenes.first { $0.windows.contains(where: \.isKeyWindow) }
        return (key ?? scenes.first)?.screen ?? UIScreen.main
    }

    /// `screen` in landscape pixels, at its top refresh.
    public static func mode(on screen: UIScreen) -> (width: Int, height: Int, hz: Int) {
        let bounds = screen.nativeBounds // portrait-oriented pixels (tvOS: the TV mode)
        return (
            Int(max(bounds.width, bounds.height)), Int(min(bounds.width, bounds.height)),
            screen.maximumFramesPerSecond)
    }
    #endif

    #if os(macOS)
    /// `screen`'s panel below its camera housing, at its top refresh.
    public static func mode(on screen: NSScreen) -> (width: Int, height: Int, hz: Int) {
        let safe = screen.notchSafePixelSize
        return (safe.width, safe.height, screen.maximumFramesPerSecond)
    }
    #endif

    /// How long a session keeps the mode of a screen that went away, in seconds. A display that
    /// changes its own mode leaves and comes back inside it, and the host then switches once.
    public static let lossHold: TimeInterval = 3

    /// `mode`'s refresh to the millihertz: 59 940 on a display running at 59.94 Hz, which its
    /// whole hertz round to 60. Only a Mac reads a fraction: the other platforms state whole
    /// hertz.
    public static var refreshMilliHz: Int {
        #if os(macOS)
        return NSScreen.main?.refreshMilliHz ?? 60_000
        #else
        return mode.hz * 1000
        #endif
    }

    /// This app's display in landscape pixels, at its top refresh. On a Mac that is the panel
    /// below the camera housing (`NSScreen.notchSafePixelSize`), never the framebuffer a scaled
    /// mode renders into: native fullscreen stops below the housing, and only that fullscreen
    /// scans out Direct. The whole panel stays a mode of its own, picked by name.
    public static var mode: (width: Int, height: Int, hz: Int) {
        #if os(macOS)
        guard let screen = NSScreen.main else { return (1920, 1080, 60) }
        return mode(on: screen)
        #elseif os(visionOS)
        // No screen to read (UIScreen is unavailable, visionOS 27 SDK): a window can grow past
        // 4K's detail, and every Vision Pro runs 90 Hz. Fixed until visionOS reports a refresh
        // ceiling; M5 panels reach 120.
        return (3840, 2160, 90)
        #else
        return mode(on: screen)
        #endif
    }
}

#if os(macOS)
extension NSScreen {
    /// This screen's rate to the millihertz, off its current display mode. A mode that states
    /// no rate, or one a hertz or more from `maximumFramesPerSecond` (a variable-rate panel
    /// held under its top), reads the whole top rate.
    public var refreshMilliHz: Int {
        let whole = maximumFramesPerSecond * 1000
        guard let number = deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber,
            let mode = CGDisplayCopyDisplayMode(CGDirectDisplayID(number.uint32Value))
        else { return whole }
        let exact = Int((mode.refreshRate * 1000).rounded())
        return abs(exact - whole) < 1000 ? exact : whole
    }
}
#endif
