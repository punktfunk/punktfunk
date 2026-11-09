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
    let id: String
    /// The section title.
    let title: String
    let options: [SlotOption]
}

/// The catalogue by group (design/touch-client-overlay.md §3.3), with each entry's availability
/// note. The preset's own shortcuts and the empty slot are appended per config.
let builtinGroups: [SlotGroup] = [
    .init(id: "session", title: String(localized: L10n.ringGroupSession), options: [
        .init(id: "end_stream", label: String(localized: L10n.ringEndStream)),
        .init(id: "end_game", label: String(localized: L10n.ringEndGame),
              note: String(localized: L10n.ringEndGameNote)),
        .init(id: "disconnect_linger", label: String(localized: L10n.ringDisconnectLinger)),
    ]),
    .init(id: "input", title: String(localized: L10n.ringGroupInput), options: [
        .init(id: "touch_mode", label: String(localized: L10n.ringTouchMode), note: noTouchNote),
        .init(id: "keyboard", label: String(localized: L10n.ringKeyboard), note: keyboardNote),
        .init(id: "pad", label: String(localized: L10n.ringPad),
              note: noTouchNote ?? String(localized: L10n.ringPadNote)),
        .init(id: "send_text", label: String(localized: L10n.ringSendText),
              note: String(localized: L10n.ringSendTextNote)),
        .init(id: "guide", label: String(localized: L10n.ringGuide),
              note: String(localized: L10n.ringGuideNote)),
        .init(id: "qam", label: String(localized: L10n.ringQam),
              note: String(localized: L10n.ringQamNote)),
        .init(id: "pad_mouse", label: String(localized: L10n.ringPadMouse),
              note: String(localized: L10n.ringPadMouseNote)),
        .init(id: "pad_type", label: String(localized: L10n.ringPadType),
              note: String(localized: L10n.ringPadTypeNote)),
        .init(id: "release_input", label: String(localized: L10n.ringReleaseInput),
              note: SlotId.releaseInput.runsHere ? nil : String(localized: L10n.ringNoPointerCapture)),
        .init(id: "mouse_mode", label: String(localized: L10n.ringMouseMode), note: macOnlyNote),
        .init(id: "clipboard", label: String(localized: L10n.ringClipboard),
              note: SlotId.clipboard.runsHere ? nil : String(localized: L10n.ringClipboardUnavailable)),
    ]),
    .init(id: "view", title: String(localized: L10n.ringGroupView), options: [
        .init(id: "stats", label: String(localized: L10n.ringStats)),
        .init(id: "fullscreen", label: String(localized: L10n.ringFullscreen), note: macOnlyNote),
        .init(id: "minimize", label: String(localized: L10n.ringMinimize), note: macOnlyNote),
    ]),
    .init(id: "audio", title: String(localized: L10n.ringGroupAudio), options: [
        .init(id: "mic", label: String(localized: L10n.ringMic), note: micNote),
    ]),
    .init(id: "host", title: String(localized: L10n.ringGroupHost), options: [
        .init(id: "host:power.sleep", label: String(localized: L10n.hostPowerSleep),
              note: String(localized: L10n.ringHostNote)),
        .init(id: "host:power.reboot", label: String(localized: L10n.hostPowerReboot),
              note: String(localized: L10n.ringHostNote)),
        .init(id: "host:power.shutdown", label: String(localized: L10n.hostPowerShutdown),
              note: String(localized: L10n.ringHostNote)),
    ]),
]

/// The catalogue for one config: the built-in groups, then its shortcuts, then the empty slot.
func slotGroups(for cfg: OverlayConfig) -> [SlotGroup] {
    var groups = builtinGroups
    if !cfg.shortcuts.isEmpty {
        groups.append(SlotGroup(id: "shortcuts", title: String(localized: L10n.ringGroupShortcuts),
                                options: cfg.shortcuts.map {
            SlotOption(id: "shortcut:\($0.id)", label: $0.label.isEmpty ? chordChip($0.keys) : $0.label,
                       note: $0.label.isEmpty ? nil : chordChip($0.keys))
        }))
    }
    groups.append(SlotGroup(id: "empty", title: String(localized: L10n.ringGroupEmpty),
                            options: [SlotOption(id: "", label: String(localized: L10n.ringEmptySlot))]))
    return groups
}

// The slots this platform cannot run, said in the catalogue so a dimmed disc is never a surprise
// found after picking it. The preset still syncs to a device that runs them.
/// The window and mouse-model actions run only on a Mac.
private let macOnlyNote: String? = SlotId.fullscreen.runsHere ? nil : String(localized: L10n.appleRingMacOnly)

#if os(macOS)
private let noTouchNote: String? = String(localized: L10n.appleRingNoteNoTouchMac)
private let keyboardNote: String? = String(localized: L10n.appleRingNoteKeyboardMac)
private let micNote: String? = nil
#elseif os(tvOS)
private let noTouchNote: String? = String(localized: L10n.appleRingNoteNoTouchTv)
private let keyboardNote: String? = String(localized: L10n.appleRingNoteKeyboardTv)
private let micNote: String? = String(localized: L10n.appleRingNoteMicTv)
#else
private let noTouchNote: String? = nil
private let keyboardNote: String? = nil
private let micNote: String? = nil
#endif

let modifierKeys = ["ctrl", "alt", "shift", "win"]

/// The keys a chord can end on, as the keyboard the editor draws lays them out — every name
/// `keyVk` knows, grouped the way a keyboard groups them.
let keyGroups: [(id: String, title: String, keys: [String])] = [
    ("function", String(localized: L10n.ringKeyGroupFunction), ["escape"] + (1...12).map { "f\($0)" }),
    ("letters", String(localized: L10n.ringKeyGroupLetters), "qwertyuiopasdfghjklzxcvbnm".map(String.init)),
    ("numbers", String(localized: L10n.ringKeyGroupNumbers), (1...9).map(String.init) + ["0"]),
    ("editing", String(localized: L10n.ringKeyGroupEditing),
     ["tab", "space", "enter", "backspace", "delete", "insert"]),
    ("navigation", String(localized: L10n.ringKeyGroupNavigation),
     ["home", "end", "pageup", "pagedown", "up", "down", "left", "right"]),
    ("other", String(localized: L10n.ringKeyGroupOther), ["printscreen", "pause", "capslock"]),
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
        padAvailable: { true }, padShown: { false }, togglePad: {}, tapPadButton: { _, _ in },
        pointerGranted: { true }, padMouseTarget: { 1 }, padMouseMode: { .off }, cyclePadMouse: {},
        currentMode: { (1920, 1080, 60) }, requestMode: { _, _, _ in },
        padTypeAvailable: { true })
}
