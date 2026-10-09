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
import SwiftUI

/// One line of the reference: what you press, and what it does.
struct ShortcutItem: Identifiable {
    /// Stable within its group — the keys are unique per group by construction.
    var id: String { keys }
    /// The chord itself, rendered monospaced so ⌃⌥⇧-style runs stay legible.
    let keys: String
    let text: String
}

struct ShortcutGroup: Identifiable {
    var id: String { title }
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
            .init(keys: "Click", text: "Capture the mouse and keyboard for the stream"),
            .init(keys: "⌃⌥⇧Q or ⌘⎋", text: "Release the mouse and keyboard back to this Mac"),
            .init(keys: "⌃⌥⇧D", text: "Disconnect"),
            .init(keys: "⌃⌥⇧S", text: "Cycle the statistics overlay"),
            .init(keys: "⌃⌥⇧O", text: "Open the quick actions dial"),
            .init(keys: "⌃⌥⇧M", text: "Switch the mouse mode"),
            .init(keys: "⌃⌥⇧C", text: "Start or stop clipboard sharing"),
            .init(keys: "⌃⌘F", text: "Toggle fullscreen (after ⌘⎋, or with Capture system shortcuts off)"),
        ]
        if micAvailable {
            keyboard.append(.init(keys: "⌃⌥⇧A", text: "Mute or unmute the microphone"))
        }
        groups.append(.init(title: "Keyboard", items: keyboard))
        #elseif os(iOS) || os(visionOS)
        // iPad with a hardware keyboard gets the same cross-client set as the Mac (StreamCommands
        // publishes it either way); a phone simply never sees a keyboard to press it on.
        var keyboard: [ShortcutItem] = [
            .init(keys: "⌃⌥⇧Q or ⌘⎋", text: "Release the pointer back to this device"),
            .init(keys: "⌃⌥⇧D", text: "Disconnect"),
            .init(keys: "⌃⌥⇧S", text: "Cycle the statistics overlay"),
            .init(keys: "⌃⌥⇧O", text: "Open the quick actions dial"),
            .init(keys: "⌃⌥⇧C", text: "Start or stop clipboard sharing"),
        ]
        if micAvailable {
            keyboard.append(.init(keys: "⌃⌥⇧A", text: "Mute or unmute the microphone"))
        }
        groups.append(.init(title: "Hardware keyboard", items: keyboard))
        groups.append(.init(title: "Touch", items: [
            .init(keys: "Two-finger twist", text: "Open the quick actions dial"),
            .init(keys: "Three-finger tap", text: "Cycle the statistics overlay"),
            .init(keys: "Three-finger swipe up", text: "Show the keyboard"),
            .init(keys: "Three-finger swipe down", text: "Hide the keyboard"),
            .init(keys: "Two fingers from a side edge", text: "Open the quick actions dial in Touch passthrough"),
        ]))
        #elseif os(tvOS)
        // The remote section leads on tvOS: it carries the ONLY exits. Menu/B is swallowed during
        // a session (ContentView's `.onExitCommand {}`), so a user who does not know the hold
        // gesture is genuinely stuck — which is why this was the one banner that could not simply
        // be deleted without putting the words somewhere findable first.
        groups.append(.init(title: "Siri Remote", items: [
            .init(keys: "Back", text: "Open the quick actions dial"),
            .init(keys: "Hold Back", text: "Disconnect"),
            .init(keys: "Touch surface", text: "Move the pointer"),
            .init(keys: "Press", text: "Click"),
            .init(keys: "Play/Pause", text: "Right-click"),
            .init(keys: "Hold Play/Pause", text: "Cycle the statistics overlay"),
        ]))
        #endif
        // Every client's controller speaks these two chords — see GamepadCapture.escapeChord and
        // .statsChord, which a test pins against their GameController element lists.
        groups.append(.init(title: "Controller", items: [
            .init(keys: "L1 + R1 + Start + Select", text: "Hold to disconnect"),
            .init(keys: "Select + X", text: "Cycle the statistics overlay"),
            .init(keys: "Select + A", text: "Open the quick actions dial"),
            .init(keys: "Hold Select", text: "Press the host's guide button"),
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
                TVScreenTitle("Shortcuts")
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
        .navigationTitle("Shortcuts")
    }
    #endif
}
