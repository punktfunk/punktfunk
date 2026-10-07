// The dial editors' shared vocabulary: the catalogue of actions by group with each entry's note
// on this platform, the shortcut keys, and the ring's inert preview commands. The touch/desktop
// editor (QuickActionsEditor) and the TV's (TVQuickActionsEditor) both read it.

import PunktfunkKit
import PunktfunkShared
import SwiftUI

struct SlotOption: Identifiable {
    let id: String
    let label: String
    var note: String? = nil
}

struct SlotGroup: Identifiable {
    /// The section title.
    let id: String
    let options: [SlotOption]
}

/// The catalogue by group (design/touch-client-overlay.md §3.3), with each entry's availability
/// note. The preset's own shortcuts and the empty slot are appended per config.
let builtinGroups: [SlotGroup] = [
    .init(id: "Session", options: [
        .init(id: "end_stream", label: "End stream"),
        .init(id: "end_game", label: "End game", note: "Only a game this device launched"),
        .init(id: "disconnect_linger", label: "Disconnect, keep the game running"),
    ]),
    .init(id: "Input", options: [
        .init(id: "touch_mode", label: "Touch mode", note: noTouchNote),
        .init(id: "keyboard", label: "Keyboard", note: keyboardNote),
        .init(id: "pad", label: "Virtual controller",
              note: noTouchNote ?? "Shows or hides the on-screen controller"),
        .init(id: "send_text", label: "Send text", note: "Not on this device yet"),
        .init(id: "guide", label: "Guide button", note: "The host's Xbox / PS / Steam button"),
        .init(id: "qam", label: "Quick access menu",
              note: "Only where the host's pad is Steam-shaped"),
        .init(id: "pad_mouse", label: "Controller mouse",
              note: "Your controller moves the host's pointer"),
        .init(id: "pad_type", label: "Controller type",
              note: "The controller the host emulates, for this stream"),
    ]),
    .init(id: "View", options: [.init(id: "stats", label: "Statistics")]),
    .init(id: "Audio", options: [.init(id: "mic", label: "Microphone", note: micNote)]),
    .init(id: "Host", options: [
        .init(id: "host:power.sleep", label: "Sleep host", note: "Only where the host offers it"),
        .init(id: "host:power.reboot", label: "Restart host", note: "Only where the host offers it"),
        .init(id: "host:power.shutdown", label: "Shut down host", note: "Only where the host offers it"),
    ]),
]

/// The catalogue for one config: the built-in groups, then its shortcuts, then the empty slot.
func slotGroups(for cfg: OverlayConfig) -> [SlotGroup] {
    var groups = builtinGroups
    if !cfg.shortcuts.isEmpty {
        groups.append(SlotGroup(id: "Shortcuts", options: cfg.shortcuts.map {
            SlotOption(id: "shortcut:\($0.id)", label: $0.label.isEmpty ? chordChip($0.keys) : $0.label,
                       note: $0.label.isEmpty ? nil : chordChip($0.keys))
        }))
    }
    groups.append(SlotGroup(id: "Empty", options: [SlotOption(id: "", label: "Empty slot")]))
    return groups
}

// The slots this platform cannot run, said in the catalogue so a dimmed disc is never a surprise
// found after picking it. The preset still syncs to a device that runs them.
#if os(macOS)
private let noTouchNote: String? = "Not on a Mac — no touch screen"
private let keyboardNote: String? = "Not on a Mac — use its own keyboard"
private let micNote: String? = nil
#elseif os(tvOS)
private let noTouchNote: String? = "Not on Apple TV — no touch screen"
private let keyboardNote: String? = "Not on Apple TV"
private let micNote: String? = "Not on Apple TV — no microphone"
#else
private let noTouchNote: String? = nil
private let keyboardNote: String? = nil
private let micNote: String? = nil
#endif

let modifierKeys = ["ctrl", "alt", "shift", "win"]

/// The keys a chord can end on, as the keyboard the editor draws lays them out — every name
/// `keyVk` knows, grouped the way a keyboard groups them.
let keyGroups: [(title: String, keys: [String])] = [
    ("Function", ["escape"] + (1...12).map { "f\($0)" }),
    ("Letters", "qwertyuiopasdfghjklzxcvbnm".map(String.init)),
    ("Numbers", (1...9).map(String.init) + ["0"]),
    ("Editing", ["tab", "space", "enter", "backspace", "delete", "insert"]),
    ("Navigation", ["home", "end", "pageup", "pagedown", "up", "down", "left", "right"]),
    ("Other", ["printscreen", "pause", "capslock"]),
]

/// A shortcut being edited, new or existing.
struct ShortcutDraft: Identifiable, Hashable {
    var id: String
    var label: String
    var keys: [String]
    var isNew: Bool
}

/// The three power actions as the ring would show them on a host that offers all three; an
/// editor has no host on the line, and a dimmed "does not offer it" would lie about the slot.
let previewHosts = [
    HostAction(id: "power.sleep", title: "Sleep"),
    HostAction(id: "power.reboot", title: "Restart", danger: true),
    HostAction(id: "power.shutdown", title: "Shut down", danger: true),
]

/// The ring's commands with nothing behind them: an editor shows, it never fires (§3.3).
var previewRingActions: RingActions {
    #if os(tvOS)
    let micAvailable = false
    #else
    let micAvailable = true
    #endif
    return RingActions(
        endStream: {}, disconnectLinger: {},
        touchMode: { .trackpad }, cycleTouchMode: {},
        keyboard: {},
        stats: { .compact }, cycleStats: {},
        micAvailable: { micAvailable }, micMuted: { false }, toggleMic: {},
        hostActions: { previewHosts }, invokeHost: { _ in },
        sendShortcut: { _ in },
        padAvailable: { true }, padShown: { false }, togglePad: {}, tapPadButton: { _ in },
        pointerGranted: { true }, padMouseTarget: { 1 }, padMouseMode: { .off }, cyclePadMouse: {},
        currentMode: { (1920, 1080, 60) }, requestMode: { _, _, _ in },
        padTypeAvailable: { true })
}
