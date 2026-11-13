// The app's "Stream" menu: the menu bar on macOS, the app menu on iPad and Vision Pro. It lives
// at the Scene level so it keeps working while the HUD is hidden. The stats item cycles the
// focused window's session tier (off → compact → normal → detailed → off).
//
// InputCapture's shortcut engine runs the keyboard shortcuts in both capture states, from the
// player's own bindings. On macOS its key monitor sees each key before any menu, and the key
// equivalents here name the default chords. On iPadOS and visionOS GCKeyboard is the key path and
// a menu key equivalent is a second dispatch of the same press, so the items there carry none.
//
// tvOS has no menu bar / hardware-keyboard command surface (disconnect there is the Siri
// Remote's Menu button, handled by ContentView's `.onExitCommand`), so this whole file is
// non-tvOS only.

#if !os(tvOS)
import PunktfunkKit
import PunktfunkShared
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

    /// The ⌃⌥⇧ key equivalent of an item: macOS only (see the file header).
    private func chord(_ key: KeyEquivalent) -> KeyboardShortcut? {
        #if os(macOS)
        KeyboardShortcut(key, modifiers: [.control, .option, .shift])
        #else
        nil
        #endif
    }

    var body: some Commands {
        CommandMenu(L10n.appleMenuStream) {
            // From the focused session's own tier: a preset that starts a session on Detailed
            // cycles to Off from here, whatever the global default is.
            Button(L10n.appleMenuCycleStatistics) { session?.cycleStats() }
            .keyboardShortcut(chord("s"))
            .disabled(session?.isStreaming != true)
            // Reaches the key window's stream view via NotificationCenter — capture is view
            // state the Scene can't touch directly.
            Button(L10n.appleMenuReleaseMouse) {
                NotificationCenter.default.post(name: .punktfunkReleaseCapture, object: nil)
            }
            .keyboardShortcut(chord("q"))
            .disabled(session?.isStreaming != true)
            // Mic mute: local and instant, it gates capture on this device and never asks the host.
            // Per session, so it starts off every time. Greyed when the session sends no microphone
            // (mic off in Settings, or a preset that turns it off).
            Button(session?.micMuted == true ? L10n.appleMenuUnmuteMicrophone : L10n.appleMenuMuteMicrophone) {
                session?.toggleMicMute()
            }
            .keyboardShortcut(chord("a"))
            .disabled(session?.isStreaming != true || session?.micAvailable != true)
            // Mid-session clipboard flip (design/clipboard-and-file-transfer.md §5.3). Greyed
            // when the host doesn't advertise the cap (older host / operator policy off).
            Button(session?.clipboardOn == true ? L10n.appleMenuStopSharingClipboard
                : L10n.appleMenuShareClipboard) {
                session?.toggleClipboard()
            }
            .keyboardShortcut(chord("c"))
            .disabled(session?.isStreaming != true || session?.clipboardAvailable != true)
            #if os(macOS)
            // The quick-action ring (design/touch-client-overlay.md §2). A Mac has no two-finger
            // twist, so this menu item and its ⌃⌥⇧O — the desktop clients' own chord for the ring
            // — are how it opens; a pad opens it with Select+A. InputCapture's monitor catches the
            // combo first and toggles the same ring.
            Button(L10n.appleMenuQuickActions) { session?.toggleQuickActions() }
            .keyboardShortcut(chord("o"))
            .disabled(session?.isStreaming != true)
            // Toggle the window's fullscreen. ⌃⌘F is the macOS-standard fullscreen combo; here it's
            // explicit so it's discoverable. A captured stream view swallows keys, so with Capture
            // system shortcuts off InputCapture's monitor posts the same notification; with it on,
            // ⌃⌘F goes to the host.
            Button(L10n.appleMenuToggleFullscreen) {
                NotificationCenter.default.post(name: .punktfunkToggleFullscreen, object: nil)
            }
            .keyboardShortcut("f", modifiers: [.control, .command])
            #endif
            Divider()
            Button(L10n.appleMenuDisconnect) { session?.disconnect() }
                .keyboardShortcut(chord("d"))
                .disabled(session?.isStreaming != true)
        }
    }
}
#endif
