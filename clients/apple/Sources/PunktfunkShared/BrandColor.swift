// The brand color, in the dependency-free foundation so EVERY process can use it — the widget
// extension links PunktfunkShared alone (never PunktfunkKit's Rust staticlib).

import SwiftUI

public extension Color {
    /// The Punktfunk brand purple, the app family's accent. Light: #6656F2, Dark: #8678F5 (the
    /// lighter violet reads better on dark surfaces; pf-console-ui's dark accent). Defined
    /// explicitly, not as `Color.accentColor`, which on macOS follows the system accent unless the
    /// user picks Multicolor, and the brand mark must never drift.
    static let brand: Color = {
        #if canImport(UIKit)
        return Color(UIColor { traits in
            traits.userInterfaceStyle == .dark
                ? UIColor(red: 0x86 / 255, green: 0x78 / 255, blue: 0xF5 / 255, alpha: 1)
                : UIColor(red: 0x66 / 255, green: 0x56 / 255, blue: 0xF2 / 255, alpha: 1)
        })
        #elseif canImport(AppKit)
        return Color(NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                ? NSColor(red: 0x86 / 255, green: 0x78 / 255, blue: 0xF5 / 255, alpha: 1)
                : NSColor(red: 0x66 / 255, green: 0x56 / 255, blue: 0xF2 / 255, alpha: 1)
        })
        #else
        // Non-Apple fallback: the light brand value, so all branches agree on a canonical color.
        return Color(red: 0x66 / 255, green: 0x56 / 255, blue: 0xF2 / 255)
        #endif
    }()
}
