// What a Mac's screen really measures — read by the settings mode list AND by the fullscreen
// choice, which must agree or the offered below-the-notch mode gets the wrong fullscreen.

#if os(macOS)
import AppKit
import PunktfunkShared

extension NSScreen {
    /// IOKit's flag for the mode that drives the panel 1:1 (`kDisplayModeNativeFlag`).
    private static let displayModeNativeFlag: UInt32 = 0x0200_0000

    /// The PANEL's pixels, which are not the framebuffer's. A scaled mode ("More Space") renders
    /// into a buffer LARGER than the panel and the window server shrinks it, so
    /// `frame × backingScaleFactor` reads 3420×2224 on a 2560×1664 MacBook Air — a mode the host
    /// would really drive, at a third more pixels than the screen can show. The mode list carries
    /// the panel size on the modes IOKit marks native.
    ///
    /// Falls back to the framebuffer for a display that publishes no native mode at all (Sidecar,
    /// screen sharing, some virtual outputs), which is the best guess available there.
    public var panelPixelSize: (width: Int, height: Int) {
        let fallback = (Int(frame.width * backingScaleFactor), Int(frame.height * backingScaleFactor))
        guard let number = deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber,
            let modes = CGDisplayCopyAllDisplayModes(
                CGDirectDisplayID(number.uint32Value), nil) as? [CGDisplayMode],
            let panel = modes
                .filter({ $0.ioFlags & Self.displayModeNativeFlag != 0 })
                .max(by: { $0.pixelWidth * $0.pixelHeight < $1.pixelWidth * $1.pixelHeight })
        else { return fallback }
        return (panel.pixelWidth, panel.pixelHeight)
    }

    /// Panel pixels per point: what turns a view's bounds into pixels the screen really shows.
    /// Below `backingScaleFactor` in a scaled mode (about 1.5 on a "More Space" Air, not 2).
    public var panelScale: Double {
        Double(panelPixelSize.height) / max(Double(frame.height), 1)
    }

    /// The panel shortened to clear the camera housing — the mode a full-screen stream shows whole.
    /// Equal to [`panelPixelSize`] on a screen with no housing, which is what lets callers treat
    /// "the two agree" as "there is no notch here".
    public var notchSafePixelSize: (width: Int, height: Int) {
        let panel = panelPixelSize
        // Points → panel pixels. NOT backingScaleFactor — see `SafeDisplay.mode(topInsetPoints:)`.
        return SafeDisplay.mode(
            nativeWidth: panel.width, nativeHeight: panel.height,
            topInsetPoints: Double(safeAreaInsets.top),
            scale: panelScale)
    }
}
#endif
