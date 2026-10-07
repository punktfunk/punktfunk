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
                    Button("Reset to Default", role: .destructive, action: reset)
                } footer: {
                    Text(overridden
                        ? "This preset has its own quick actions; the default dial no longer reaches it."
                        : "A short press of Back opens the dial mid-stream, and so does Select + A on a "
                            + "controller.")
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
            Text(focused.map(label) ?? "Quick Actions")
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
            Button("Move", systemImage: "arrow.left.arrow.right") { moving = k }
            if s != nil {
                Button("Clear", systemImage: "xmark.circle", role: .destructive) { set(k, "") }
            }
        }
        .accessibilityLabel(s?.label ?? "Empty slot")
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
                shortcut = ShortcutDraft(id: cfg.nextShortcutID, label: "", keys: [], isNew: true)
            } label: {
                Label("Add Shortcut", systemImage: "plus")
            }
        } header: {
            Text("Shortcuts")
        } footer: {
            Text("A new shortcut takes the first empty slot.")
        }
    }

    private func label(_ k: Int) -> String {
        cfg.ring[k].map { spec($0, cfg, previewRingActions).label } ?? "Empty slot"
    }

    private var detailTitle: String {
        if let moving { return "Moving \(label(moving))" }
        return focused.map(label) ?? "Quick Actions"
    }

    private var detailNote: String {
        if moving != nil { return "Select another button to swap the two. Back puts it down." }
        guard let k = focused, let slot = cfg.ring[k] else {
            return "Select a button to choose its action; hold it to move or clear it."
        }
        let s = spec(slot, cfg, previewRingActions)
        return s.enabled ? "Select to change it; hold to move or clear it." : s.reason
    }

    private func set(_ k: Int, _ id: String) {
        var c = cfg
        c.ring[k] = SlotId.parse(id)
        blob = c.toJSON()
    }

    private func swapSlots(_ a: Int, _ b: Int) {
        moving = nil
        guard a != b else { return }
        var c = cfg
        c.ring.swapAt(a, b)
        blob = c.toJSON()
    }

    private func save(_ d: ShortcutDraft) {
        var c = cfg
        c.saveShortcut(OverlayShortcut(id: d.id, label: d.label, keys: d.keys))
        blob = c.toJSON()
    }

    private func remove(_ id: String) {
        var c = cfg
        c.removeShortcut(id)
        blob = c.toJSON()
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
private struct TVSlotPicker: View {
    let groups: [SlotGroup]
    let current: String
    let choose: (String) -> Void

    var body: some View {
        List {
            TVScreenTitle("Slot Action")
                .listRowBackground(Color.clear)
            ForEach(groups) { group in
                Section(group.id) {
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

    private var mods: [String] { draft.keys.filter { modifierKeys.contains($0) } }
    private var key: String? { draft.keys.first { !modifierKeys.contains($0) } }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 28) {
                TVScreenTitle(draft.isNew ? "New Shortcut" : "Shortcut")
                TVFieldRow(
                    label: "Name", value: draft.label,
                    placeholder: key == nil ? "Optional" : chordChip(draft.keys)
                ) { editingName = true }
                heading("Hold")
                HStack(spacing: 24) {
                    ForEach(modifierKeys, id: \.self) { m in
                        chip(keyLegend(m), on: mods.contains(m)) { toggle(m) }
                    }
                }
                .focusSection()
                ForEach(keyGroups, id: \.title) { group in
                    heading(group.title)
                    let wide = group.keys.contains { keyLegend($0).count > 2 }
                    LazyVGrid(
                        columns: [GridItem(.adaptive(minimum: wide ? 200 : 110), spacing: 20)],
                        alignment: .leading, spacing: 20
                    ) {
                        ForEach(group.keys, id: \.self) { k in
                            chip(keyLegend(k), on: key == k) { pick(k) }
                        }
                    }
                    .focusSection()
                }
                HStack(spacing: 32) {
                    Button(draft.isNew ? "Add" : "Save") {
                        save(draft)
                        dismiss()
                    }
                    .disabled(key == nil)
                    if !draft.isNew {
                        Button("Remove Shortcut", role: .destructive) {
                            delete()
                            dismiss()
                        }
                    }
                    Button("Cancel", role: .cancel) { dismiss() }
                }
                .padding(.top, 12)
            }
            .padding(60)
        }
        .fullScreenCover(isPresented: $editingName) {
            TVTextEntry(title: "Name", text: draft.label) { typed in
                draft.label = typed
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

    /// Modifiers first in keyboard order, then the key — the order the chord is sent.
    private func rebuild(mods: [String], key: String?) {
        draft.keys = modifierKeys.filter { mods.contains($0) } + (key.map { [$0] } ?? [])
    }

    private func toggle(_ m: String) {
        var next = mods
        if let i = next.firstIndex(of: m) { next.remove(at: i) } else { next.append(m) }
        rebuild(mods: next, key: key)
    }

    private func pick(_ k: String) {
        rebuild(mods: mods, key: k)
    }
}
#endif
