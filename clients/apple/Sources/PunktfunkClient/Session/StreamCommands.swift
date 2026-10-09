// The app's "Stream" menu (macOS menu bar + iPad hardware-keyboard shortcuts). These live at
// the Scene level so they keep working when the HUD overlay is hidden. The shortcuts are the
// CROSS-CLIENT set every punktfunk client reserves — Ctrl+Alt+Shift+Q (release the captured
// mouse) / +D (disconnect) / +S (stats), plus +A (mute the microphone), the Apple clients'
// addition to it — and the menu is their discoverable surface on macOS
// (the Linux client has its GTK Shortcuts window, Windows its start-of-stream banner). While
// input is CAPTURED these key equivalents never reach the menu (the stream view swallows
// keys); InputCapture's monitor detects the same combos there and performs the same actions —
// the menu covers the released state and discoverability. The stats item cycles the focused
// window's session tier (off → compact → normal → detailed → off).
//
// tvOS has no menu bar / hardware-keyboard command surface (disconnect there is the Siri
// Remote's Menu button, handled by ContentView's `.onExitCommand`), so this whole file is
// non-tvOS only.

#if !os(tvOS)
import PunktfunkKit
import SwiftUI

/// The live session's menu-reachable actions, published by ContentView via
/// `.focusedSceneValue` so the Scene-level commands can drive it.
struct SessionFocus {
    var isStreaming: Bool
    /// The connected host advertises `HOST_CAP_CLIPBOARD` (gates the Share Clipboard item).
    var clipboardAvailable: Bool
    /// Clipboard sync is live (host-acked) — drives the item's Stop/Share title.
    var clipboardOn: Bool
    var toggleClipboard: () -> Void
    /// The session has a mic uplink at all (its resolved `micEnabled`) — gates the mute item, so
    /// it is never an enabled control over a session that sends no microphone.
    var micAvailable: Bool
    /// The user's mic mute is engaged — drives the item's Mute/Unmute title.
    var micMuted: Bool
    var toggleMicMute: () -> Void
    var cycleStats: () -> Void
    var toggleQuickActions: () -> Void
    var disconnect: () -> Void
}

private struct SessionFocusKey: FocusedValueKey {
    typealias Value = SessionFocus
}

extension FocusedValues {
    var sessionFocus: SessionFocus? {
        get { self[SessionFocusKey.self] }
        set { self[SessionFocusKey.self] = newValue }
    }
}

struct StreamCommands: Commands {
    @FocusedValue(\.sessionFocus) private var session

    var body: some Commands {
        CommandMenu("Stream") {
            // From the focused session's own tier: a preset that starts a session on Detailed
            // cycles to Off from here, whatever the global default is.
            Button("Cycle Statistics") { session?.cycleStats() }
            .keyboardShortcut("s", modifiers: [.control, .option, .shift])
            .disabled(session?.isStreaming != true)
            // Reaches the key window's stream view via NotificationCenter — capture is view
            // state the Scene can't touch directly. (Captured, the combo is handled by
            // InputCapture's monitor before menus see it; this item is the released-state
            // path and the shortcut's menu-bar documentation.)
            Button("Release Mouse") {
                NotificationCenter.default.post(name: .punktfunkReleaseCapture, object: nil)
            }
            .keyboardShortcut("q", modifiers: [.control, .option, .shift])
            .disabled(session?.isStreaming != true)
            // Mic mute: local and instant, it gates capture on this device and never asks the host.
            // Per session, so it starts off every time. Greyed when the session sends no microphone
            // (mic off in Settings, or a preset that turns it off). Captured, InputCapture's chord
            // path handles the combo first; this item is the released-state path and documents it.
            Button(session?.micMuted == true ? "Unmute Microphone" : "Mute Microphone") {
                session?.toggleMicMute()
            }
            .keyboardShortcut("a", modifiers: [.control, .option, .shift])
            .disabled(session?.isStreaming != true || session?.micAvailable != true)
            // Mid-session clipboard flip (design/clipboard-and-file-transfer.md §5.3). Greyed
            // when the host doesn't advertise the cap (older host / operator policy off). Captured,
            // InputCapture's chord path handles the combo first; this item is the released path.
            Button(session?.clipboardOn == true ? "Stop Sharing Clipboard" : "Share Clipboard") {
                session?.toggleClipboard()
            }
            .keyboardShortcut("c", modifiers: [.control, .option, .shift])
            .disabled(session?.isStreaming != true || session?.clipboardAvailable != true)
            #if os(macOS)
            // The quick-action ring (design/touch-client-overlay.md §2). A Mac has no two-finger
            // twist, so this menu item and its ⌃⌥⇧O — the desktop clients' own chord for the ring
            // — are how it opens; a pad opens it with Select+A. Captured, InputCapture's monitor
            // catches the combo and toggles the same ring.
            Button("Quick Actions") { session?.toggleQuickActions() }
            .keyboardShortcut("o", modifiers: [.control, .option, .shift])
            .disabled(session?.isStreaming != true)
            // Toggle the window's fullscreen. ⌃⌘F is the macOS-standard fullscreen combo; here it's
            // explicit so it's discoverable. A captured stream view swallows keys, so with Capture
            // system shortcuts off InputCapture's monitor posts the same notification; with it on,
            // ⌃⌘F goes to the host.
            Button("Toggle Fullscreen") {
                NotificationCenter.default.post(name: .punktfunkToggleFullscreen, object: nil)
            }
            .keyboardShortcut("f", modifiers: [.control, .command])
            #endif
            Divider()
            Button("Disconnect") { session?.disconnect() }
                .keyboardShortcut("d", modifiers: [.control, .option, .shift])
                .disabled(session?.isStreaming != true)
        }
    }
}
#endif
