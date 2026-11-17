// "Copy Link" on a host or pinned card puts a `punktfunk://` URL on the clipboard — the same
// self-emitted form a shortcut or a Playnite entry would carry (design/client-deep-links.md §5):
// stable id first, with the address and pin alongside so it still resolves after the record is
// re-addressed or the store is wiped.
//
// The pasteboard is the one part of that with no cross-platform API, hence this two-line shim.
// tvOS has no clipboard (UIPasteboard is unavailable there), so the affordance isn't offered.

#if os(macOS)
import AppKit
#elseif os(iOS) || os(visionOS)
import UIKit
#endif

enum LinkClipboard {
    /// True where a link can actually be copied — the card menus hide the item elsewhere rather
    /// than offering an action that silently does nothing.
    static var isAvailable: Bool {
        #if os(macOS) || os(iOS) || os(visionOS)
        return true
        #else
        return false
        #endif
    }

    static func copy(_ text: String) {
        #if os(macOS)
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        #elseif os(iOS) || os(visionOS)
        UIPasteboard.general.string = text
        #endif
    }
}
