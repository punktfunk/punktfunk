// The dial's editor on a TV: the dial itself, its six discs as focusable buttons in their ring
// places (design/apple-tvos-ui-overhaul.md §2.4). Select changes a slot's action; a long press
// moves or clears it. It edits whichever layer the settings surface is on, as the touch one does.

#if os(tvOS)
import PunktfunkKit
import PunktfunkShared
import SwiftUI

private let dialSize: CGFloat = 560
private let dialRadius: CGFloat = 200

struct TVQuickActionsEditor: View {
    /// The `overlay_actions` blob of the layer being edited; empty is the TV default.
    @Binding var blob: String
    /// The edited preset owns its own dial.
    let overridden: Bool
    /// The Shortcuts page of the same layer, a push away. A new shortcut skips every id its
    /// bindings name.
    let shortcuts: ShortcutsEditor
    /// The pad chord that opens the dial, from the live shortcuts; nil when none does.
    let padOpener: String?
    /// The edited layer holds the Shortcuts page's blob itself, so removing a shortcut drops
    /// its bindings there. False where the layer inherits them: another layer is never written.
    var ownsBindings = false
    /// Back to the TV dial: drops the override in preset scope, clears the global otherwise.
    let reset: () -> Void
    @State private var picking: Int?
    @State private var moving: Int?
    @State private var shortcut: ShortcutDraft?
    @FocusState private var focused: Int?
    #if DEBUG
    /// Shot harness: opens slot 1's list, then goes back, as Back would.
    var shotPushPop = false
    #endif

    private var cfg: OverlayConfig { OverlayConfig.parse(blob, platform: .tv) }

    var body: some View {
        HStack(alignment: .top, spacing: 48) {
            dial
                .frame(width: dialSize, height: dialSize)
                .focusSection()
            Form {
                Section {
                    // Not a row focus lands on, so no platter: what the focused disc holds and
                    // what it needs.
                    VStack(alignment: .leading, spacing: 8) {
                        Text(detailTitle)
                            .font(.geist(30, .semibold, relativeTo: .title3))
                        Text(detailNote)
                            .foregroundStyle(.secondary)
                    }
                    .listRowBackground(Color.clear)
                }
                shortcutsSection
                Section {
                    NavigationLink(String(localized: L10n.playerShortcuts)) { shortcuts }
                } footer: {
                    Text(L10n.playerShortcutsHint)
                }
                Section {
                    Button(L10n.ringReset, role: .destructive, action: reset)
                } footer: {
                    if overridden {
                        Text(L10n.ringEditorPresetOwns)
                    } else if let padOpener {
                        Text(L10n.appleTvRingFooter(remote: "Back", pad: padOpener))
                    }
                }
            }
        }
        .navigationDestination(item: $picking) { k in
            TVSlotPicker(groups: slotGroups(for: cfg), current: cfg.ring[k]?.id ?? "") { id in
                set(k, id)
                picking = nil
            }
        }
        .navigationDestination(item: $shortcut) { draft in
            TVShortcutEditor(draft: draft, save: save) { remove(draft.id) }
        }
        #if DEBUG
        .task {
            guard shotPushPop else { return }
            try? await Task.sleep(for: .seconds(1))
            picking = 0
            try? await Task.sleep(for: .seconds(1.5))
            picking = nil
        }
        #endif
    }

    private var dial: some View {
        ZStack {
            ForEach(0..<OverlayConfig.ringSlots, id: \.self) { k in
                disc(k)
                    .position(position(k))
            }
            Text(focused.map(label) ?? String(localized: L10n.ringEditorTitle))
                .font(.geist(22, .medium, relativeTo: .caption))
                .multilineTextAlignment(.center)
                .frame(width: 220)
                .position(x: dialSize / 2, y: dialSize / 2)
        }
        // Back puts a lifted disc down; otherwise it leaves the pane as usual.
        .onExitCommand(perform: moving == nil ? nil : { moving = nil })
    }

    /// Slot k at 12, 2, 4… o'clock, as the in-stream ring places it.
    private func position(_ k: Int) -> CGPoint {
        let rad = (-90 + 60 * CGFloat(k)) * .pi / 180
        return CGPoint(x: dialSize / 2 + dialRadius * cos(rad), y: dialSize / 2 + dialRadius * sin(rad))
    }

    private func disc(_ k: Int) -> some View {
        let s = cfg.ring[k].map { spec($0, cfg, previewRingActions) }
        return Button {
            if let from = moving {
                swapSlots(from, k)
            } else {
                picking = k
            }
        } label: {
            DiscFace(spec: s, lifted: moving == k)
        }
        .buttonStyle(DiscButtonStyle())
        .focused($focused, equals: k)
        .contextMenu {
            Button(L10n.appleTvRingMove, systemImage: "arrow.left.arrow.right") { moving = k }
            if s != nil {
                Button(L10n.appleTvRingClear, systemImage: "xmark.circle", role: .destructive) { set(k, "") }
            }
        }
        .accessibilityLabel(s?.label ?? String(localized: L10n.ringSlotEmpty))
    }

    private var shortcutsSection: some View {
        Section {
            ForEach(cfg.shortcuts, id: \.id) { sc in
                Button {
                    shortcut = ShortcutDraft(id: sc.id, label: sc.label, keys: sc.keys, isNew: false)
                } label: {
                    HStack {
                        Text(sc.label.isEmpty ? chordChip(sc.keys) : sc.label)
                        Spacer(minLength: 16)
                        Text(chordChip(sc.keys)).foregroundStyle(.secondary)
                    }
                }
            }
            Button {
                shortcut = ShortcutDraft(id: cfg.nextShortcutID(bindings: Shortcuts.parse(shortcuts.blob)),
                                         label: "", keys: [], isNew: true)
            } label: {
                Label(L10n.ringShortcutAdd, systemImage: "plus")
            }
        } header: {
            Text(L10n.ringEditorShortcuts)
        } footer: {
            Text(L10n.ringEditorShortcutsHint)
        }
    }

    private func label(_ k: Int) -> String {
        cfg.ring[k].map { spec($0, cfg, previewRingActions).label } ?? String(localized: L10n.ringSlotEmpty)
    }

    private var detailTitle: String {
        if let moving { return label(moving) }
        return focused.map(label) ?? String(localized: L10n.ringEditorTitle)
    }

    private var detailNote: String {
        if moving != nil { return L10n.appleTvRingMovingHint(back: "Back") }
        guard let k = focused, let slot = cfg.ring[k] else {
            return String(localized: L10n.appleTvRingPickHint)
        }
        let s = spec(slot, cfg, previewRingActions)
        return s.enabled ? String(localized: L10n.appleTvRingSlotHint) : s.reason
    }

    /// Writes the blob as `change` leaves the config.
    private func edit(_ change: (inout OverlayConfig) -> Void) {
        var c = cfg
        change(&c)
        blob = c.toJSON()
    }

    private func set(_ k: Int, _ id: String) {
        edit { $0.assign(k, id: id) }
    }

    /// Puts the lifted disc down on slot `b`; on its own slot it changes nothing.
    private func swapSlots(_ a: Int, _ b: Int) {
        moving = nil
        guard a != b else { return }
        edit { $0.swap(a, b) }
    }

    private func save(_ d: ShortcutDraft) {
        edit { $0.saveShortcut(OverlayShortcut(id: d.id, label: d.label, keys: d.keys)) }
    }

    private func remove(_ id: String) {
        var sent = Shortcuts.parse(shortcuts.blob)
        let before = sent
        edit { $0.removeShortcut(id, bindings: &sent) }
        if ownsBindings, sent != before { shortcuts.blob = sent.toJSON() }
    }
}

/// One disc as the ring draws it, at 10-foot size: brighter and larger while focused, ringed in
/// the brand colour while lifted to move.
private struct DiscFace: View {
    let spec: SlotSpec?
    let lifted: Bool
    @Environment(\.isFocused) private var isFocused

    var body: some View {
        ZStack {
            Circle().fill(Color.white.opacity(isFocused ? 0.28 : 0.12))
            Circle().strokeBorder(
                lifted ? Color.brand : Color.white.opacity(isFocused ? 0.9 : 0.2),
                lineWidth: lifted || isFocused ? 4 : 1.5)
            if let keys = spec?.keys {
                ChordKeycap(keys: keys).scaleEffect(1.8)
            } else if let mark = spec?.mark {
                mark.resizable().scaledToFit().frame(width: 60, height: 60)
            } else {
                Image(systemName: spec?.icon ?? "circle.dashed")
                    .font(.system(size: 44, weight: .semibold))
            }
        }
        .foregroundStyle(spec?.enabled ?? false ? Color.white : Color.white.opacity(0.35))
        .frame(width: 120, height: 120)
        .scaleEffect(isFocused ? 1.15 : 1)
        .animation(.easeOut(duration: 0.15), value: isFocused)
    }
}

/// The disc draws its own focus; the style only dims a press.
private struct DiscButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label.opacity(configuration.isPressed ? 0.8 : 1)
    }
}

/// The catalogue as the TV's pushed list: grouped, each entry's note under it, the current one
/// ticked. Picking one sets the slot and comes back.
struct TVSlotPicker: View {
    let groups: [SlotGroup]
    let current: String
    let choose: (String) -> Void

    var body: some View {
        List {
            TVScreenTitle(String(localized: L10n.ringPickerTitle))
                .listRowBackground(Color.clear)
            ForEach(groups) { group in
                Section(group.title) {
                    ForEach(group.options) { option in
                        Button {
                            choose(option.id)
                        } label: {
                            HStack {
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(option.label)
                                    if let note = option.note {
                                        Text(note)
                                            .font(.geist(22, relativeTo: .caption))
                                            .foregroundStyle(.secondary)
                                    }
                                }
                                Spacer(minLength: 16)
                                if option.id == current { Image(systemName: "checkmark") }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// One shortcut on a TV: the name through the system keyboard, the modifiers as toggles, the key
/// from the grouped keyboard the touch editor draws, and the buttons at the bottom.
private struct TVShortcutEditor: View {
    @State var draft: ShortcutDraft
    let save: (ShortcutDraft) -> Void
    let delete: () -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var editingName = false

    private var chord: Chord { Chord.parse(keys: draft.keys) }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 28) {
                TVScreenTitle(String(localized: draft.isNew ? L10n.ringShortcutNewTitle : L10n.ringShortcutTitle))
                TVFieldRow(
                    label: String(localized: L10n.ringShortcutName), value: draft.label,
                    placeholder: draft.keys.isEmpty ? String(localized: L10n.ringShortcutNamePlaceholder)
                        : chordChip(draft.keys)
                ) { editingName = true }
                heading(String(localized: L10n.ringShortcutModifiers))
                HStack(spacing: 24) {
                    ForEach(Chord.modifiers, id: \.self) { m in
                        chip(keyLegend(m), on: chord.mods.contains(m)) { toggle(m) }
                    }
                }
                .focusSection()
                ForEach(Chord.keyGroups, id: \.id) { group in
                    heading(group.title)
                    let wide = group.keys.contains { keyLegend($0).count > 2 }
                    LazyVGrid(
                        columns: [GridItem(.adaptive(minimum: wide ? 200 : 110), spacing: 20)],
                        alignment: .leading, spacing: 20
                    ) {
                        ForEach(group.keys, id: \.self) { k in
                            chip(keyLegend(k), on: chord.key == k) { pick(k) }
                        }
                    }
                    .focusSection()
                }
                HStack(spacing: 32) {
                    Button(draft.isNew ? L10n.commonAdd : L10n.commonSave) {
                        save(draft)
                        dismiss()
                    }
                    // A modifier alone is a combination: Win opens the host's Start menu.
                    .disabled(draft.keys.isEmpty)
                    if !draft.isNew {
                        Button(L10n.ringShortcutRemove, role: .destructive) {
                            delete()
                            dismiss()
                        }
                    }
                    Button(L10n.commonCancel, role: .cancel) { dismiss() }
                }
                .padding(.top, 12)
            }
            .padding(60)
        }
        .fullScreenCover(isPresented: $editingName) {
            TVTextEntry(title: String(localized: L10n.ringShortcutName), text: draft.label) { typed in
                draft.label = typed.trimmingCharacters(in: .whitespaces)
                editingName = false
            }
        }
    }

    private func heading(_ text: String) -> some View {
        Text(text)
            .font(.geist(24, .semibold, relativeTo: .caption))
            .foregroundStyle(.secondary)
    }

    /// The pick is the prominent button: a tinted bordered one paints its label in the tint.
    @ViewBuilder private func chip(_ title: String, on: Bool, action: @escaping () -> Void) -> some View {
        let button = Button(action: action) {
            Text(title)
                .lineLimit(1)
                .minimumScaleFactor(0.6)
                .frame(minWidth: 60)
        }
        if on {
            button.buttonStyle(.borderedProminent).tint(Color.brand)
                .accessibilityAddTraits(.isSelected)
        } else {
            button.buttonStyle(.bordered)
        }
    }

    private func toggle(_ m: String) {
        var c = chord
        c.toggle(m)
        draft.keys = c.keys
    }

    private func pick(_ k: String) {
        var c = chord
        c.key = k
        draft.keys = c.keys
    }
}
#endif
