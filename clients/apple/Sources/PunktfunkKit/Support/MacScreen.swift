// What a Mac's screens really measure — read by the settings mode list AND by the fullscreen
// choice, which must agree or the offered below-the-notch mode gets the wrong fullscreen — and
// how they sit, which the screens a session asks for follow.

#if os(macOS)
import AppKit
import PunktfunkCore
import PunktfunkShared

/// One screen of a session on this Mac: the record the host is asked for, and the display it
/// stands for.
struct DisplayScreen {
    let record: PunktfunkScreen
    let display: CGDirectDisplayID
}

extension ScreenAsk {
    /// A host screen per display of this Mac, screen 0 the main one at `mode`; none with one.
    @MainActor
    public static func ofThisMac(mode: (width: UInt32, height: UInt32, hz: UInt32)) -> ScreenAsk {
        guard NSScreen.screens.count > 1, let main = NSScreen.main else { return .none }
        return ScreenAsk(records: NSScreen.sessionScreens(first: main, mode: mode).map(\.record))
    }
}

extension NSScreen {
    /// The display's CoreGraphics id, stable while it stays connected.
    var displayID: CGDirectDisplayID? {
        (deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? NSNumber)
            .map { CGDirectDisplayID($0.uint32Value) }
    }

    /// The session's screens on this Mac: `first` at `mode`, then each other display at its panel
    /// pixels and top refresh, at most `PUNKTFUNK_SCREENS_MAX` in all.
    @MainActor
    static func sessionScreens(
        first: NSScreen, mode: (width: UInt32, height: UInt32, hz: UInt32)
    ) -> [DisplayScreen] {
        guard let firstID = first.displayID else { return [] }
        let primary = NSScreen.screens.first?.displayID
        var head = PunktfunkScreen()
        (head.width, head.height, head.refresh_hz) = (mode.width, mode.height, mode.hz)
        head.primary = firstID == primary ? 1 : 0
        var out = [DisplayScreen(record: head, display: firstID)]
        for screen in NSScreen.screens {
            guard out.count < Int(PUNKTFUNK_SCREENS_MAX), let id = screen.displayID, id != firstID
            else { continue }
            let panel = screen.panelPixelSize
            let at = offset(
                of: screen.frame, size: panel, from: first.frame, mode: (mode.width, mode.height))
            var r = PunktfunkScreen()
            r.index = UInt8(out.count)
            (r.width, r.height) = (UInt32(panel.width), UInt32(panel.height))
            r.refresh_hz = UInt32(max(screen.maximumFramesPerSecond, 1))
            (r.x, r.y) = at
            r.primary = id == primary ? 1 : 0
            out.append(DisplayScreen(record: r, display: id))
        }
        return out
    }

    /// Where `other`'s top-left sits from `first`'s, both AppKit frames (origin bottom-left, y
    /// up), in pixels of `mode` shown on `first` (y down). A display wholly left of or above
    /// `first` hangs off `first`'s edge by its own `size` in pixels, so a denser `first` leaves
    /// no gap.
    static func offset(
        of other: CGRect, size: (width: Int, height: Int), from first: CGRect,
        mode: (width: UInt32, height: UInt32)
    ) -> (x: Int32, y: Int32) {
        let sx = Double(mode.width) / max(Double(first.width), 1)
        let sy = Double(mode.height) / max(Double(first.height), 1)
        let x = other.maxX <= first.minX
            ? ((other.maxX - first.minX) * sx).rounded() - Double(size.width)
            : ((other.minX - first.minX) * sx).rounded()
        let y = other.minY >= first.maxY
            ? ((first.maxY - other.minY) * sy).rounded() - Double(size.height)
            : ((first.maxY - other.maxY) * sy).rounded()
        return (Int32(clamping: Int(x)), Int32(clamping: Int(y)))
    }

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

    /// The panel below the menu bar's band, where native fullscreen stops — the mode a full-screen
    /// stream shows 1:1. Equal to [`panelPixelSize`] on a screen with no housing, which is what
    /// lets callers treat "the two agree" as "there is no notch here".
    public var notchSafePixelSize: (width: Int, height: Int) {
        let panel = panelPixelSize
        let top = SafeDisplay.fullscreenTopInset(
            housing: Double(safeAreaInsets.top), menuBar: Double(frame.maxY - visibleFrame.maxY))
        // Points → panel pixels. NOT backingScaleFactor — see `SafeDisplay.mode(topInsetPoints:)`.
        return SafeDisplay.mode(
            nativeWidth: panel.width, nativeHeight: panel.height,
            topInsetPoints: top, scale: panelScale)
    }
}
#endif
