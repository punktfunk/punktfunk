// Settings → Input → Shortcuts (design/client-shortcuts.md §5): what the controller and the
// keyboard do while a stream runs, one row per shortcut from `Shortcuts.rows`, each openable.
// A row's sheet picks the action from the dial's catalogue, records the trigger by pressing it
// (or chooses it from a grid) and sets Press or Hold. It edits whichever layer the settings
// surface is on: the binding comes from `scoped(SettingsFields.shortcuts)`. On tvOS the page
// sits in the Quick actions category and lists no keys.

import GameController
import PunktfunkKit
import PunktfunkShared
import SwiftUI

#if os(macOS)
import AppKit
let shortcutClient = Shortcuts.Client.mac
#else
let shortcutClient = Shortcuts.Client.apple
#endif

#if os(tvOS)
/// A TV lists the Siri Remote instead of a keyboard and touch gestures.
private let touchscreen = false
#elseif os(macOS)
private let touchscreen = false
#else
private let touchscreen = true
#endif

/// One binding being edited; `index` is nil for a new one.
private struct BindingDraft: Identifiable {
    let id = UUID()
    let index: Int?
}

struct ShortcutsEditor: View {
    /// The `shortcuts` blob of the layer being edited; empty is this platform's defaults.
    @Binding var blob: String
    /// The retired *Hold Select for guide* value the defaults read.
    let legacyGuideGesture: String
    /// The layer's ring blob, which names the key combinations a shortcut can send.
    let overlayActions: String
    /// The edited preset owns its own shortcuts.
    let overridden: Bool
    /// Back to the defaults: drops the override in preset scope, clears the global otherwise.
    let reset: () -> Void
    @State private var editing: BindingDraft?

    private var shortcuts: Shortcuts {
        Shortcuts.resolve(blob, client: shortcutClient, legacyGuideGesture: legacyGuideGesture)
    }

    private var overlay: OverlayConfig { OverlayConfig.parse(overlayActions) }

    var body: some View {
        let current = shortcuts
        Form {
            Section {
                Text(L10n.shortcutsLead).foregroundStyle(.secondary)
                if overridden {
                    Text(L10n.ringEditorPresetOwns).foregroundStyle(.secondary)
                }
            }
            ForEach(current.rows(client: shortcutClient, touchscreen: touchscreen, overlay: overlay)) { group in
                Section {
                    ForEach(group.rows) { row in rowView(row, current) }
                } header: {
                    Text(group.title)
                } footer: {
                    if let note = group.note { Text(note).font(.geist(13, relativeTo: .footnote)) }
                }
            }
            Section {
                Button {
                    editing = BindingDraft(index: nil)
                } label: {
                    Label(L10n.shortcutsAdd, systemImage: "plus")
                }
                Button(L10n.shortcutsReset, role: .destructive, action: reset)
            }
        }
        #if os(macOS)
        .formStyle(.grouped)
        #endif
        .navigationTitle(L10n.playerShortcuts)
        .sheet(item: $editing) { draft in
            ShortcutSheet(shortcuts: current, index: draft.index, overlay: overlay) { blob = $0.toJSON() }
                #if os(macOS)
                .frame(width: 460, height: 560)
                #endif
        }
    }

    @ViewBuilder private func rowView(_ row: Shortcuts.Row, _ current: Shortcuts) -> some View {
        if let i = row.binding {
            let action = current.bindings[i].action
            let missing = action.dangling(overlay) || !Self.runsHere(action)
            Button {
                editing = BindingDraft(index: i)
            } label: {
                line(row.text, keys: row.keys, note: missing ? String(localized: L10n.shortcutsNotOnDevice) : nil)
                    .opacity(missing ? 0.5 : 1)
                    .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
        } else {
            line(row.text, keys: row.keys, note: String(localized: L10n.shortcutsFixed))
                .foregroundStyle(.secondary)
        }
    }

    private func line(_ text: String, keys: String, note: String?) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text(text)
                if let note { Text(note).font(.footnote).foregroundStyle(.secondary) }
            }
            Spacer(minLength: 8)
            Text(keys)
                .font(.geistFixed(13, .medium))
                .multilineTextAlignment(.trailing)
        }
    }

    static func runsHere(_ action: Shortcuts.Action) -> Bool {
        if case .slot(let slot) = action { return slot.runsHere }
        return true
    }
}

/// One shortcut: what it does, what to press, and whether a press or a hold fires it.
private struct ShortcutSheet: View {
    let shortcuts: Shortcuts
    let index: Int?
    let overlay: OverlayConfig
    let save: (Shortcuts) -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var action: Shortcuts.Action?
    @State private var trigger: Shortcuts.Trigger?
    @State private var picking = false
    @State private var choosing = false
    @State private var replacing: Int?
    @StateObject private var recorder = ShortcutRecorder()

    init(shortcuts: Shortcuts, index: Int?, overlay: OverlayConfig, save: @escaping (Shortcuts) -> Void) {
        self.shortcuts = shortcuts
        self.index = index
        self.overlay = overlay
        self.save = save
        let b = index.map { shortcuts.bindings[$0] }
        _action = State(initialValue: b?.action)
        _trigger = State(initialValue: b?.trigger)
    }

    /// What Save does with the trigger as it stands; nil before there is one.
    private var decision: Shortcuts.SaveDecision? {
        trigger.map { shortcuts.saveDecision($0, action: action ?? .ring, editing: index) }
    }

    var body: some View {
        NavigationStack {
            Form {
                Section(String(localized: L10n.shortcutsDoes)) {
                    Button {
                        picking = true
                    } label: {
                        HStack {
                            Text(action?.label(overlay) ?? String(localized: L10n.ringEmptySlot))
                            Spacer(minLength: 8)
                            Image(systemName: "chevron.right")
                                .font(.footnote.weight(.semibold))
                                .foregroundStyle(.tertiary)
                                .accessibilityHidden(true)
                        }
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
                pressSection
                if case .pad(let buttons, let hold) = trigger {
                    Section {
                        Picker(String(localized: L10n.shortcutsFiresOn), selection: Binding(
                            get: { hold || buttons.count < 2 },
                            set: { trigger = .pad(buttons: buttons, hold: $0) })
                        ) {
                            Text(L10n.shortcutsFiresPress).tag(false)
                            Text(L10n.shortcutsFiresHold).tag(true)
                        }
                        .pickerStyle(.segmented)
                        .disabled(buttons.count < 2)
                    } header: {
                        Text(L10n.shortcutsFiresOn)
                    } footer: {
                        if buttons.count < 2 { Text(L10n.shortcutsOneButtonHold) }
                    }
                }
                if let index {
                    Section {
                        Button(L10n.shortcutsRemove, role: .destructive) {
                            save(shortcuts.without(index))
                            dismiss()
                        }
                    }
                }
            }
            #if os(macOS)
            .formStyle(.grouped)
            #endif
            .navigationTitle(action?.label(overlay) ?? String(localized: L10n.shortcutsAdd))
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button(L10n.commonCancel) { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button(index == nil ? L10n.commonAdd : L10n.commonSave) {
                        if case .replace(let i)? = decision { replacing = i } else { commit() }
                    }
                    .disabled(action == nil || trigger == nil || blocking != nil)
                }
            }
            .alert(replacePrompt, isPresented: Binding(
                get: { replacing != nil }, set: { if !$0 { replacing = nil } })
            ) {
                Button(L10n.shortcutsReplace, role: .destructive) { commit(replacing: replacing) }
                Button(L10n.commonCancel, role: .cancel) {}
            }
            .sheet(isPresented: $picking) { actionPicker }
            .sheet(isPresented: $choosing) {
                ChooseGrid(trigger: $trigger)
                    #if os(macOS)
                    .frame(width: 460, height: 560)
                    #endif
            }
            .onDisappear { recorder.stop() }
        }
    }

    @ViewBuilder private var pressSection: some View {
        Section {
            HStack {
                Text(recorder.active ? recorder.preview : (trigger?.label(client: shortcutClient) ?? "—"))
                    .font(.geistFixed(15, .medium))
                Spacer(minLength: 8)
                Button(L10n.shortcutsRecord) {
                    recorder.start { got in
                        if case .pad(let buttons, _) = got, case .pad(_, let wasHold)? = trigger {
                            trigger = .pad(buttons: buttons, hold: wasHold || buttons.count < 2)
                        } else if case .pad(let buttons, _) = got {
                            trigger = .pad(buttons: buttons, hold: buttons.count < 2)
                        } else {
                            trigger = got
                        }
                    }
                }
                .disabled(recorder.active)
                Button(L10n.shortcutsChoose) { choosing = true }
            }
            #if os(iOS) || os(visionOS)
            if recorder.active { KeyCatcher { recorder.key(modifiers: $0, vk: $1) }.frame(height: 0) }
            #endif
        } header: {
            Text(L10n.shortcutsPress)
        } footer: {
            if recorder.active {
                Text(L10n.shortcutsRecordPrompt)
            } else if let blocking {
                Text(blocking).foregroundStyle(.red)
            }
        }
    }

    /// Why Save is off, in the player's words; nil when it may save (an identical trigger asks
    /// to replace instead).
    private var blocking: String? {
        guard case .blocked(let conflict)? = decision, let trigger else { return nil }
        switch conflict {
        case .identical: return nil
        case .pressSubset(let i):
            let other = shortcuts.without(index).bindings[i].trigger
            let (first, second) = trigger.ids.count < other.ids.count ? (trigger, other) : (other, trigger)
            return L10n.shortcutsSubset(first: first.chord(client: shortcutClient),
                                        second: second.chord(client: shortcutClient))
        case .singlePress: return String(localized: L10n.shortcutsOneButtonHold)
        case .noModifier: return String(localized: L10n.shortcutsNeedsModifier)
        case .noKey: return String(localized: L10n.ringShortcutPickKeyFirst)
        }
    }

    private var replacePrompt: String {
        guard let replacing else { return "" }
        let b = shortcuts.without(index).bindings[replacing]
        return L10n.shortcutsIdenticalReplace(keys: b.trigger.label(client: shortcutClient),
                                              action: b.action.label(overlay))
    }

    /// Store the edit, less the binding at `replacing` (an index of the others) it takes over from.
    private func commit(replacing: Int? = nil) {
        guard let action, let trigger else { return }
        save(shortcuts.applying(binding: .init(trigger: trigger, action: action), at: index, replacing: replacing))
        dismiss()
    }

    /// The dial's catalogue, with opening the dial itself first.
    @ViewBuilder private var actionPicker: some View {
        let groups: [SlotGroup] = slotGroups(for: overlay).filter { $0.id != "empty" }.map { g in
            guard g.id == "session" else { return g }
            let ring = SlotOption(id: "ring", label: String(localized: L10n.ringOpenRing))
            return SlotGroup(id: g.id, title: g.title, options: [ring] + g.options)
        }
        let choose: (String) -> Void = { id in
            action = Shortcuts.Action(id: id)
            picking = false
        }
        #if os(tvOS)
        NavigationStack { TVSlotPicker(groups: groups, current: action?.id ?? "", choose: choose) }
        #else
        SlotPicker(groups: groups, current: action?.id ?? "", choose: choose)
            #if os(macOS)
            .frame(width: 420, height: 520)
            #endif
        #endif
    }
}

/// Records a trigger by pressing it: a controller's buttons, taken when the first one lifts, or
/// a key with the modifiers held. Pads are polled, so a stream in another window keeps its pad
/// handlers.
@MainActor
private final class ShortcutRecorder: ObservableObject {
    @Published private(set) var active = false
    /// The buttons pressed so far, in press order.
    @Published private(set) var held: [UInt32] = []
    private var take: ((Shortcuts.Trigger) -> Void)?
    private var poll: Timer?
    /// Each pad's buttons at the last poll.
    private var last: [ObjectIdentifier: UInt32] = [:]
    #if os(macOS)
    private var monitor: Any?
    #endif

    var preview: String {
        held.isEmpty ? String(localized: L10n.shortcutsRecordPrompt)
            : held.map { Shortcuts.buttonLabel($0) }.joined(separator: " + ")
    }

    func start(_ take: @escaping (Shortcuts.Trigger) -> Void) {
        stop()
        self.take = take
        active = true
        let timer = Timer(timeInterval: 1.0 / 60.0, repeats: true) { [weak self] timer in
            guard let self else { return timer.invalidate() }
            MainActor.assumeIsolated { self.pollPads() }
        }
        RunLoop.main.add(timer, forMode: .common)
        poll = timer
        #if os(macOS)
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            guard let self, let vk = InputCapture.vk(keyCode: event.keyCode) else { return event }
            let flags = event.modifierFlags
            let mods = [(NSEvent.ModifierFlags.control, "ctrl"), (.option, "alt"), (.shift, "shift")]
                .filter { flags.contains($0.0) }.map(\.1)
            self.key(modifiers: mods, vk: vk)
            return nil
        }
        #endif
    }

    /// Safe to call again: the second call finds nothing to undo.
    func stop() {
        poll?.invalidate()
        poll = nil
        last = [:]
        held = []
        active = false
        take = nil
        #if os(macOS)
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
        #endif
    }

    private func pollPads() {
        for controller in GCController.controllers() where active {
            if let g = controller.extendedGamepad { pad(g, id: ObjectIdentifier(controller)) }
        }
    }

    /// A pad's first poll only notes what it holds: the press that started recording is no part
    /// of the trigger.
    private func pad(_ g: GCExtendedGamepad, id: ObjectIdentifier) {
        let now = GamepadCapture.heldButtons(g)
        guard let was = last[id] else {
            last[id] = now
            return
        }
        last[id] = now
        for bit in GamepadWire.allButtons where now & ~was & bit != 0 && held.count < 4 && !held.contains(bit) {
            held.append(bit)
        }
        if was & ~now != 0, !held.isEmpty {
            finish(.pad(buttons: held, hold: false))
        }
    }

    /// A key went down with these modifiers held; modifiers alone are no key.
    func key(modifiers: [String], vk: UInt32) {
        guard let name = Self.keyName(vk) else { return }
        finish(.key(modifiers: Shortcuts.modifiers.filter(modifiers.contains), key: name))
    }

    private func finish(_ trigger: Shortcuts.Trigger) {
        let take = take
        stop()
        take?(trigger)
    }

    /// The stored name of a key's VK, from the names the key grid knows.
    static func keyName(_ vk: UInt32) -> String? {
        Chord.keyGroups.flatMap(\.keys).first { keyVk($0) == vk }
    }
}

#if os(iOS) || os(visionOS)
/// Takes the keyboard while the recorder runs: UIKit hands a first responder every press.
private struct KeyCatcher: UIViewRepresentable {
    let onKey: ([String], UInt32) -> Void

    func makeUIView(context: Context) -> CatcherView {
        let view = CatcherView()
        view.onKey = onKey
        return view
    }

    func updateUIView(_ view: CatcherView, context: Context) {
        view.onKey = onKey
    }

    final class CatcherView: UIView {
        var onKey: (([String], UInt32) -> Void)?
        override var canBecomeFirstResponder: Bool { true }

        override func didMoveToWindow() {
            super.didMoveToWindow()
            if window != nil { becomeFirstResponder() }
        }

        override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
            var rest = Set<UIPress>()
            for press in presses {
                guard let key = press.key, let vk = InputCapture.vk(hidUsage: key.keyCode.rawValue),
                      !(0xE0...0xE7).contains(key.keyCode.rawValue)
                else {
                    rest.insert(press)
                    continue
                }
                let flags = key.modifierFlags
                let mods = [(UIKeyModifierFlags.control, "ctrl"), (.alternate, "alt"), (.shift, "shift")]
                    .filter { flags.contains($0.0) }.map(\.1)
                onKey?(mods, vk)
            }
            if !rest.isEmpty { super.pressesBegan(rest, with: event) }
        }
    }
}
#endif

/// The trigger chosen from a grid: the connected controller's buttons in its own glyphs, or
/// (not on tvOS) a key with Ctrl, Alt and Shift.
private struct ChooseGrid: View {
    @Binding var trigger: Shortcuts.Trigger?
    @Environment(\.dismiss) private var dismiss
    @State private var buttons: [UInt32] = []
    @State private var hold = false
    @State private var mods: [String] = []
    @State private var key: String?
    @State private var keyboard = false

    /// The buttons a shortcut offers, in the rows' order.
    private static let pad: [UInt32] = [
        "a", "b", "x", "y", "lb", "rb", "ls", "rs", "start", "select", "guide",
        "up", "down", "left", "right", "touchpad", "misc1",
    ].compactMap(Shortcuts.buttonBit)

    var body: some View {
        NavigationStack {
            Form {
                #if !os(tvOS)
                Picker(String(localized: L10n.shortcutsPress), selection: $keyboard) {
                    Text(L10n.controlsGroupPad).tag(false)
                    Text(L10n.controlsGroupKeyboard).tag(true)
                }
                .pickerStyle(.segmented)
                #endif
                if keyboard {
                    keyGrid
                } else {
                    Section {
                        LazyVGrid(columns: [GridItem(.adaptive(minimum: 64), spacing: 8)], spacing: 8) {
                            ForEach(Self.pad, id: \.self) { bit in
                                let on = buttons.contains(bit)
                                Button {
                                    if let i = buttons.firstIndex(of: bit) {
                                        buttons.remove(at: i)
                                    } else if buttons.count < 4 {
                                        buttons.append(bit)
                                    }
                                } label: {
                                    glyph(bit).frame(maxWidth: .infinity).frame(height: 30)
                                }
                                .buttonStyle(.bordered)
                                .tint(on ? Color.brand : Color.secondary)
                                .accessibilityLabel(Shortcuts.buttonLabel(bit))
                                .accessibilityAddTraits(on ? .isSelected : [])
                            }
                        }
                    } footer: {
                        Text(buttons.isEmpty ? "—" : buttons.map { Shortcuts.buttonLabel($0) }.joined(separator: " + "))
                    }
                }
            }
            #if os(macOS)
            .formStyle(.grouped)
            #endif
            .navigationTitle(L10n.shortcutsChoose)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button(L10n.commonCancel) { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button(L10n.commonDone) {
                        if keyboard, let key {
                            trigger = .key(modifiers: mods, key: key)
                        } else if !keyboard, !buttons.isEmpty {
                            trigger = .pad(buttons: buttons, hold: hold || buttons.count < 2)
                        }
                        dismiss()
                    }
                    .disabled(keyboard ? key == nil : buttons.isEmpty)
                }
            }
            .onAppear {
                switch trigger {
                case .pad(let b, let h)?: (buttons, hold) = (b, h)
                case .key(let m, let k)?: (mods, key, keyboard) = (m, k, true)
                case nil: break
                }
            }
        }
    }

    /// The connected pad's own symbol for a button, or its name.
    @ViewBuilder private func glyph(_ bit: UInt32) -> some View {
        if let symbol = GamepadCapture.symbol(for: bit, on: GCController.current ?? GCController.controllers().first) {
            Image(systemName: symbol)
        } else {
            Text(Shortcuts.buttonLabel(bit)).font(.geistFixed(13, .medium))
        }
    }

    @ViewBuilder private var keyGrid: some View {
        Section {
            HStack(spacing: 8) {
                ForEach(Shortcuts.modifiers, id: \.self) { m in
                    let on = mods.contains(m)
                    Button(keyLegend(m)) {
                        if on { mods.removeAll { $0 == m } } else { mods = Shortcuts.modifiers.filter { mods.contains($0) || $0 == m } }
                    }
                    .buttonStyle(.bordered)
                    .tint(on ? Color.brand : Color.secondary)
                    .accessibilityAddTraits(on ? .isSelected : [])
                }
            }
        }
        ForEach(Chord.keyGroups, id: \.id) { group in
            Section(group.title) {
                let wide = group.keys.contains { keyLegend($0).count > 2 }
                LazyVGrid(columns: [GridItem(.adaptive(minimum: wide ? 80 : 44), spacing: 6)], spacing: 6) {
                    ForEach(group.keys, id: \.self) { k in
                        Button {
                            key = k
                        } label: {
                            Text(keyLegend(k))
                                .font(.geistFixed(13, key == k ? .semibold : .medium))
                                .lineLimit(1)
                                .minimumScaleFactor(0.6)
                                .frame(maxWidth: .infinity)
                                .frame(height: 30)
                        }
                        .buttonStyle(.bordered)
                        .tint(key == k ? Color.brand : Color.secondary)
                        .accessibilityAddTraits(key == k ? .isSelected : [])
                    }
                }
            }
        }
    }
}
