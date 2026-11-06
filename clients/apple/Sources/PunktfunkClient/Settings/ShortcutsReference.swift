// The in-session controls, written down once and read by every surface that shows them.
//
// This replaced the start-of-stream banner (ContentView's `showShortcutHint`): a 6-second pill
// that told you the controls exactly once, while you were busy looking at the thing you had just
// connected to, and then never again. A reference you can OPEN answers the question at the moment
// it is actually asked — which is the second session, not the first.
//
// The catalog is data rather than a view so both About pages render the same words: the touch
// `AboutView` (a Form) and the controller-first `GamepadAboutView` (a console list). The banner
// was macOS/tvOS-only, so deleting it would have cost Mac TOUCH users the one place those keys
// were written down — hence the touch surface gets this too, not just the gamepad UI.
//
// Per-platform by `#if`, because the honest answer really is different: tvOS has no keyboard and
// no menu bar, iOS has a touch gesture nothing else has, and macOS is the only one that has to
// explain mouse capture. A controller's chords are the one section common to all three — they are
// the same buttons on every client (`GamepadCapture.escapeChord` / `.statsChord`), which is the
// whole point of a cross-client chord.

import AVFoundation
import PunktfunkKit
import PunktfunkShared
import SwiftUI

/// One line of the reference: what you press, and what it does.
struct ShortcutItem: Identifiable {
    /// Unique within its group.
    let id: String
    /// The chord itself, rendered monospaced so ⌃⌥⇧-style runs stay legible. A key or button
    /// name is data; a gesture is a message.
    let keys: String
    let text: String

    init(_ id: String, keys: String, text: String) {
        self.id = id
        self.keys = keys
        self.text = text
    }

    init(_ id: String, keys: String, _ text: LocalizedStringResource) {
        self.init(id, keys: keys, text: String(localized: text))
    }
}

struct ShortcutGroup: Identifiable {
    let id: String
    let title: String
    let items: [ShortcutItem]
}

enum ShortcutsCatalog {
    /// Whether a mute key is worth listing when no session is running, for the About page reached
    /// from settings. `SessionModel.micAvailable` is the authority DURING a session — it also
    /// consults the preset the session actually resolved — but a reference page opened between
    /// sessions has no session to ask, so it answers the device-level half of the same question:
    /// a platform with an app-accessible input, the mic setting on, and the OS not refusing.
    /// `.notDetermined` counts, exactly as it does there: the prompt is simply still pending.
    static var micPlausible: Bool {
        #if os(tvOS)
        return false // no app-accessible microphone
        #else
        guard UserDefaults.standard.object(forKey: DefaultsKey.micEnabled) as? Bool
            ?? SettingDefault.micEnabled
        else { return false }
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized, .notDetermined: return true
        default: return false
        }
        #endif
    }

    /// `micAvailable` gates the mute row — a device with no microphone would otherwise be told
    /// about a key that does nothing, which is the failure the old banner already avoided.
    static func groups(micAvailable: Bool) -> [ShortcutGroup] {
        var groups: [ShortcutGroup] = []
        #if os(macOS)
        var keyboard: [ShortcutItem] = [
            .init("capture", keys: String(localized: L10n.controlsGestureClick),
                  L10n.controlsCaptureMouseKeyboard),
            .init("release", keys: L10n.controlsEither(first: "⌃⌥⇧Q", second: "⌘⎋"),
                  L10n.controlsReleaseMouseKeyboard),
            .init("disconnect", keys: "⌃⌥⇧D", L10n.controlsDisconnect),
            .init("stats", keys: "⌃⌥⇧S", L10n.controlsCycleStats),
            .init("dial", keys: "⌃⌥⇧O", L10n.controlsOpenDial),
            .init("mouse_mode", keys: "⌃⌥⇧M", L10n.controlsSwitchMouseMode),
            .init("clipboard", keys: "⌃⌥⇧C", L10n.controlsToggleClipboard),
            .init("fullscreen", keys: "⌃⌘F", text: L10n.controlsToggleFullscreenAfter(keys: "⌘⎋")),
        ]
        if micAvailable {
            keyboard.append(.init("mic", keys: "⌃⌥⇧A", L10n.controlsToggleMic))
        }
        groups.append(.init(id: "keyboard", title: String(localized: L10n.controlsGroupKeyboard),
                            items: keyboard))
        #elseif os(iOS) || os(visionOS)
        // iPad with a hardware keyboard gets the same cross-client set as the Mac (StreamCommands
        // publishes it either way); a phone simply never sees a keyboard to press it on.
        var keyboard: [ShortcutItem] = [
            .init("release", keys: L10n.controlsEither(first: "⌃⌥⇧Q", second: "⌘⎋"),
                  L10n.controlsReleasePointerDevice),
            .init("disconnect", keys: "⌃⌥⇧D", L10n.controlsDisconnect),
            .init("stats", keys: "⌃⌥⇧S", L10n.controlsCycleStats),
            .init("dial", keys: "⌃⌥⇧O", L10n.controlsOpenDial),
            .init("clipboard", keys: "⌃⌥⇧C", L10n.controlsToggleClipboard),
        ]
        if micAvailable {
            keyboard.append(.init("mic", keys: "⌃⌥⇧A", L10n.controlsToggleMic))
        }
        groups.append(.init(id: "keyboard", title: String(localized: L10n.controlsGroupKeyboard),
                            items: keyboard))
        groups.append(.init(id: "touch", title: String(localized: L10n.controlsGroupTouch), items: [
            .init("twist", keys: String(localized: L10n.controlsGestureTwoFingerTwist),
                  L10n.controlsOpenDial),
            .init("tap", keys: String(localized: L10n.controlsGestureThreeFingerTap),
                  L10n.controlsCycleStats),
            .init("swipe_up", keys: String(localized: L10n.controlsGestureThreeFingerSwipeUp),
                  L10n.controlsShowKeyboard),
            .init("swipe_down", keys: String(localized: L10n.controlsGestureThreeFingerSwipeDown),
                  L10n.controlsHideKeyboard),
            .init("side_edge", keys: String(localized: L10n.appleControlsGestureSideEdge),
                  L10n.appleControlsOpenDialPassthrough),
        ]))
        #elseif os(tvOS)
        // The remote section leads on tvOS: it carries the ONLY exits. Menu/B is swallowed during
        // a session (ContentView's `.onExitCommand {}`), so a user who does not know the hold
        // gesture is genuinely stuck — which is why this was the one banner that could not simply
        // be deleted without putting the words somewhere findable first.
        groups.append(.init(id: "remote", title: "Siri Remote", items: [
            .init("back", keys: "Back", L10n.controlsOpenDial),
            .init("back_hold", keys: L10n.controlsHold(keys: "Back"), L10n.controlsDisconnect),
            .init("surface", keys: String(localized: L10n.appleControlsTouchSurface),
                  L10n.appleControlsMovePointer),
            .init("press", keys: String(localized: L10n.appleControlsPress), L10n.appleControlsClick),
            .init("play_pause", keys: "Play/Pause", L10n.appleControlsRightClick),
            .init("play_pause_hold", keys: L10n.controlsHold(keys: "Play/Pause"), L10n.controlsCycleStats),
        ]))
        #endif
        // Every client's controller speaks these two chords — see GamepadCapture.escapeChord and
        // .statsChord, which a test pins against their GameController element lists.
        groups.append(.init(id: "controller", title: String(localized: L10n.controlsGroupPad), items: [
            .init("escape", keys: "L1 + R1 + Start + Select", L10n.controlsHoldDisconnect),
            .init("stats", keys: "Select + X", L10n.controlsCycleStats),
            .init("dial", keys: "Select + A", L10n.controlsOpenDial),
            .init("guide", keys: L10n.controlsHold(keys: "Select"), L10n.controlsGuideHold),
        ]))
        return groups
    }
}

/// The standard-interface reference — a sheet from `AboutView` on iOS/macOS, a pushed page on
/// tvOS — so the keys the start-of-stream banner used to carry are still one press away.
/// (The controller-first surface renders the same catalog itself; see `GamepadAboutView`.)
struct ShortcutsView: View {
    let micAvailable: Bool

    var body: some View {
        #if os(tvOS)
        // No `Form`/`.formStyle(.grouped)` worth using at 10 feet, and the rows are read, not
        // operated — a plain scrolling column at TV sizes says the same thing with less chrome.
        ScrollView {
            VStack(alignment: .leading, spacing: 30) {
                TVScreenTitle(String(localized: L10n.controlsTitle))
                ForEach(ShortcutsCatalog.groups(micAvailable: micAvailable)) { group in
                    VStack(alignment: .leading, spacing: 12) {
                        Text(group.title)
                            .font(.geist(28, .semibold, relativeTo: .headline))
                        ForEach(group.items) { item in
                            HStack(alignment: .firstTextBaseline, spacing: 20) {
                                Text(item.keys)
                                    .font(.geistFixed(22, .medium))
                                    .frame(minWidth: 300, alignment: .leading)
                                    .fixedSize(horizontal: false, vertical: true)
                                Text(item.text)
                                    .font(.geist(22, relativeTo: .caption))
                                    .foregroundStyle(.secondary)
                                    .fixedSize(horizontal: false, vertical: true)
                            }
                        }
                    }
                    // A pushed page with nothing focusable cannot route Menu — the app suspends
                    // instead of popping (see TVFocusable). One stop per group.
                    .modifier(TVFocusable())
                }
            }
            .frame(maxWidth: 1000, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(60)
        }
        #else
        form
        #endif
    }

    #if !os(tvOS)
    private var form: some View {
        Form {
            ForEach(ShortcutsCatalog.groups(micAvailable: micAvailable)) { group in
                Section(group.title) {
                    ForEach(group.items) { item in
                        HStack(alignment: .firstTextBaseline, spacing: 12) {
                            Text(item.keys)
                                .font(.geistFixed(13, .medium))
                                .foregroundStyle(.primary)
                                // A fixed column keeps the descriptions aligned; the chords vary
                                // from "Click" to "L1 + R1 + Start + Select".
                                .frame(minWidth: 132, alignment: .leading)
                                .fixedSize(horizontal: false, vertical: true)
                            Text(item.text)
                                .font(.geist(13, relativeTo: .footnote))
                                .foregroundStyle(.secondary)
                                .fixedSize(horizontal: false, vertical: true)
                        }
                        .padding(.vertical, 2)
                    }
                }
            }
        }
        .formStyle(.grouped)
        .navigationTitle(L10n.controlsTitle)
    }
    #endif
}
