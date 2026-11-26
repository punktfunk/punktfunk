// The keyboard row's editor on iPhone and iPad: the row's buttons as a list in the row's order.
// The list is the system's own: Edit moves and deletes, a swipe deletes, a tap changes what a
// button does. It writes the `row` of the same `overlay_actions` blob the ring's editor holds, so
// a preset that touches it owns both. visionOS has no keyboard row.

#if os(iOS)
import PunktfunkKit
import PunktfunkShared
import SwiftUI

/// The button being picked: its place in the row, or the row's count for a new one.
private struct PickButton: Identifiable {
    let index: Int
    var id: Int { index }
}

struct KeyRowEditor: View {
    /// The `overlay_actions` blob of the layer being edited.
    @Binding var blob: String
    @State private var picking: PickButton?

    private var cfg: OverlayConfig { OverlayConfig.parse(blob) }

    var body: some View {
        let cfg = cfg
        let buttons = cfg.rowButtons
        let faces = buttons.map { face($0, cfg) }
        // One glyph in the list gives every title the glyph's column, as a menu does.
        let glyphs = faces.contains { $0.symbol != nil }
        List {
            Section {
                // By place, not by content: a row may hold the same button twice.
                ForEach(Array(faces.enumerated()), id: \.offset) { i, face in
                    Button {
                        picking = PickButton(index: i)
                    } label: {
                        if glyphs {
                            Label {
                                Text(face.title)
                            } icon: {
                                Image(systemName: face.symbol ?? "keyboard")
                                    .opacity(face.symbol == nil ? 0 : 1)
                                    .accessibilityHidden(true)
                            }
                        } else {
                            Text(face.title)
                        }
                    }
                    .foregroundStyle(.primary)
                }
                .onMove { from, to in write { $0.move(fromOffsets: from, toOffset: to) } }
                .onDelete { offsets in write { $0.remove(atOffsets: offsets) } }
                Button {
                    picking = PickButton(index: buttons.count)
                } label: {
                    Label(L10n.commonAdd, systemImage: "plus")
                }
            } footer: {
                Text(L10n.ringRowHint)
                    .font(.geist(13, relativeTo: .footnote))
            }
            Section {
                // Back to the row nobody edited, which follows the default set.
                Button(L10n.commonReset, role: .destructive) {
                    var c = cfg
                    c.row = nil
                    blob = c.toJSON()
                }
                .disabled(cfg.row == nil)
            }
        }
        .navigationTitle(L10n.ringRowTitle)
        .toolbar { EditButton() }
        .sheet(item: $picking) { p in
            RowButtonPicker(groups: slotGroups(for: cfg).filter { $0.id != "empty" },
                            current: p.index < buttons.count ? buttons[p.index].id : nil) { id in
                if let button = RowSlot.parse(id) {
                    write { row in
                        if p.index < row.count { row[p.index] = button } else { row.append(button) }
                    }
                }
                picking = nil
            }
        }
    }

    /// A button as the list names it: a key by its legend, a quick action by the ring's glyph
    /// and name. The Keyboard action wears the glyph the row draws it with: there it hides
    /// the keyboard.
    private func face(_ button: RowSlot, _ cfg: OverlayConfig) -> (title: String, symbol: String?) {
        switch button {
        case .key(let name):
            return (keyLegend(name), nil)
        case .action(let slot):
            let s = spec(slot, cfg, previewRingActions(cfg))
            return (s.label, slot == .keyboard ? "keyboard.chevron.compact.down" : s.icon)
        }
    }

    /// Writes the blob with the row as `change` leaves it.
    private func write(_ change: (inout [RowSlot]) -> Void) {
        var c = cfg
        var row = c.rowButtons
        change(&row)
        c.row = row
        blob = c.toJSON()
    }
}

/// What one row button does: a single key, picked on a second page, or any quick action.
private struct RowButtonPicker: View {
    let groups: [SlotGroup]
    /// The id of the button being changed; `nil` picks for a new one.
    let current: String?
    let choose: (String) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                Section {
                    NavigationLink {
                        KeyPicker(current: current, choose: choose)
                    } label: {
                        HStack {
                            Text(L10n.ringRowKey)
                            Spacer()
                            if current?.hasPrefix("key:") == true { Image(systemName: "checkmark") }
                        }
                    }
                }
                SlotOptions(groups: groups, current: current ?? "", choose: choose)
            }
            .navigationTitle(L10n.ringPickerTitle)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button(L10n.commonCancel) { dismiss() } }
            }
        }
    }
}

/// Every key a button can be: the four modifiers, then the groups a keyboard has.
private struct KeyPicker: View {
    let current: String?
    let choose: (String) -> Void

    var body: some View {
        Form {
            Section { grid(Chord.modifiers) }
            ForEach(Chord.keyGroups, id: \.id) { group in
                Section(group.title) { grid(group.keys) }
            }
        }
        .navigationTitle(L10n.ringRowKey)
    }

    /// Word keys (Backspace, PgDn, Shift) get wider cells; every cap keeps one line.
    private func grid(_ keys: [String]) -> some View {
        let wide = keys.contains { keyLegend($0).count > 2 }
        return LazyVGrid(columns: [GridItem(.adaptive(minimum: wide ? 80 : 44), spacing: 6)], spacing: 6) {
            ForEach(keys, id: \.self) { key in
                let on = current == "key:\(key)"
                Button {
                    choose("key:\(key)")
                } label: {
                    Text(keyLegend(key))
                        .font(.geistFixed(13, on ? .semibold : .medium))
                        .lineLimit(1)
                        .minimumScaleFactor(0.6)
                        .frame(maxWidth: .infinity)
                        .frame(height: 30)
                }
                .buttonStyle(.bordered)
                .tint(on ? Color.brand : Color.secondary)
                .accessibilityAddTraits(on ? .isSelected : [])
            }
        }
        .padding(.vertical, 4)
    }
}
#endif
