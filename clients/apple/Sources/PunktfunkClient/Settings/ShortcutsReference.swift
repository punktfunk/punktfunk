// The in-session controls as a read-only reference on the About pages: the rows of
// `Shortcuts.rows` for this device's live shortcuts, which the Shortcuts page edits
// (SettingsView+Sections, `shortcutsRow`).

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

    /// This device's shortcuts as the Shortcuts page lists them, read-only. `micAvailable` gates
    /// the mute row: a device with no microphone is not told about a key that does nothing.
    static func groups(micAvailable: Bool) -> [ShortcutGroup] {
        let shortcuts = EffectiveSettings(defaults: .standard).resolvedShortcuts
        #if os(iOS) || os(visionOS)
        let touchscreen = true
        #else
        let touchscreen = false
        #endif
        let overlay = OverlayConfig.parse(UserDefaults.standard.string(forKey: DefaultsKey.overlayActions))
        return shortcuts.rows(client: shortcutClient, touchscreen: touchscreen, overlay: overlay).map { g in
            let items = g.rows.filter { row in
                micAvailable || row.binding.map { shortcuts.bindings[$0].action != .slot(.mic) } ?? true
            }
            return ShortcutGroup(
                id: g.id, title: g.note.map { "\(g.title) · \($0)" } ?? g.title,
                items: items.map { ShortcutItem($0.id, keys: $0.keys, text: $0.text) })
        }
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
