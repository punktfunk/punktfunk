// The quick-action ring's editor (design/touch-client-overlay.md §3.3): the editor IS the ring —
// the in-stream `RingOverlay`, the same type, full size over a backdrop that runs the real twist
// (`DialCatcher`). Tap a slot to pick its action from the catalogue, drag a disc onto another to
// swap, tap the centre to see depth two; the shortcuts list and the reset sit under it. A
// shortcut is edited on its own sheet: a name, the modifiers as chips, the key on a keyboard
// you tap, and the disc as it will look. It edits whichever layer the settings surface is on —
// the binding comes from `scoped(SettingsFields.overlayActions)` — so a preset that touches it
// owns the whole ring (D10).
//
// On the Mac the same editor, minus the twist: the backdrop is inert and the ring simply sits
// open on it, a mouse drags the discs where a finger did, and there is no on-screen controller
// to configure. A shortcut is removed from its own sheet — a macOS Form has no swipe.

#if os(iOS) || os(visionOS) || os(macOS)
import PunktfunkKit
import PunktfunkShared
import SwiftUI

#if os(macOS)
/// The instruction names the pointer the reader holds: a click on a Mac, a tap elsewhere.
private var pickHint: String { String(localized: L10n.appleRingEditorHintClick) }
#else
private var pickHint: String { String(localized: L10n.ringEditorHintTap) }
#endif

private struct PickSlot: Identifiable {
    let k: Int
    var id: Int { k }
}

struct QuickActionsEditor: View {
    /// The `overlay_actions` blob of the layer being edited; empty is the platform default.
    @Binding var blob: String
    /// The edited preset owns its own ring (the row's override marker says so; this says it
    /// under the ring).
    let overridden: Bool
    /// Back to the platform ring: drops the override in preset scope, clears the global
    /// otherwise.
    let reset: () -> Void
    @StateObject private var ring = RingState()
    @State private var picking: PickSlot?
    @State private var editingShortcut: ShortcutDraft?
    #if os(iOS) || os(visionOS)
    @State private var editingLayout = false
    #endif
    /// The backdrop's middle, where the ring opens and re-opens.
    @State private var centre = CGPoint.zero

    private var cfg: OverlayConfig {
        #if os(macOS)
        OverlayConfig.parse(blob, platform: .desktop)
        #else
        OverlayConfig.parse(blob)
        #endif
    }

    var body: some View {
        Form {
            Section {
                GeometryReader { geo in
                    ZStack {
                        // The Form's own cell colour, resolved dark (the scheme below), so the
                        // backdrop reads as one more field rather than a stage.
                        #if os(macOS)
                        Color(nsColor: .controlBackgroundColor)
                            // No twist on a Mac, so the backdrop carries only the sheet dismiss
                            // the DialCatcher's tap carries on iOS.
                            .onTapGesture { ring.sheet = false }
                        #else
                        Color(.secondarySystemGroupedBackground)
                        // The backdrop runs the real twist. Its tap only dismisses the preview
                        // sheet: UIKit hands a disc tap to this view as well as to the SwiftUI
                        // button above it, so closing the ring here closed it on every pick.
                        DialCatcher(onDial: { ring.handle($0) }) { ring.sheet = false }
                        #endif
                        RingOverlay(state: ring, cfg: cfg, actions: previewRingActions,
                                    editing: RingEditing(pick: { picking = PickSlot(k: $0) }, swap: swap))
                    }
                    .environment(\.colorScheme, .dark)
                    .onAppear {
                        centre = CGPoint(x: geo.size.width / 2, y: geo.size.height / 2)
                        ring.openAt(centre)
                    }
                    // Whatever closes it — a twist wound back, a preview row that ends the
                    // stream — the editor's ring springs back once the wind-in has played, so
                    // there is never a dead editor with nothing to tap.
                    .onChange(of: ring.closing) { _, closing in
                        if !closing, !ring.committed, ring.progress == 0 { ring.openAt(centre) }
                    }
                }
                .frame(height: 400)
                .listRowInsets(EdgeInsets())
            } footer: {
                // Footers wear the described-row caption font (SettingsView+Support): the
                // system footer style renders larger than every other settings caption.
                Text(overridden ? pickHint + " " + String(localized: L10n.ringEditorPresetOwns) : pickHint)
                    .font(.geist(13, relativeTo: .footnote))
            }
            #if os(iOS) || os(visionOS)
            // The virtual controller's preset and look (§4.3), written to the blob's `pad`
            // through the same binding the ring uses. Absent on the Mac: there is no touch screen
            // to draw it on, and a `pad` block written from here would configure nothing.
            Section {
                Picker(L10n.padVirtualLayout, selection: Binding(get: { cfg.pad.layout }, set: { l in setPad { $0.layout = l } })) {
                    Text(L10n.padVirtualLayoutFull).tag("full")
                    Text(L10n.padVirtualLayoutSticks).tag("sticks")
                    Text(L10n.padVirtualLayoutDpad).tag("dpad")
                }
                PadSlider(label: L10n.padVirtualOpacity, value: cfg.pad.opacity, range: VirtualPad.opacityRange) { v in
                    setPad { $0.opacity = v }
                }
                PadSlider(label: L10n.padVirtualScale, value: cfg.pad.scale, range: VirtualPad.scaleRange) { v in
                    setPad { $0.scale = v }
                }
                Button(L10n.padVirtualEditLayout) { editingLayout = true }
            } header: {
                Text(L10n.padVirtualTitle)
            } footer: {
                Text(L10n.padVirtualHint)
                    .font(.geist(13, relativeTo: .footnote))
            }
            #endif
            Section {
                ForEach(cfg.shortcuts, id: \.id) { sc in
                    Button {
                        editingShortcut = ShortcutDraft(id: sc.id, label: sc.label, keys: sc.keys, isNew: false)
                    } label: {
                        HStack(spacing: 12) {
                            KeycapDisc(keys: sc.keys, size: 40)
                            VStack(alignment: .leading, spacing: 2) {
                                Text(sc.label.isEmpty ? chordChip(sc.keys) : sc.label)
                                if !sc.label.isEmpty {
                                    Text(chordChip(sc.keys)).font(.footnote).foregroundStyle(.secondary)
                                }
                            }
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
                #if os(iOS) || os(visionOS)
                .onDelete { offsets in
                    for id in offsets.map({ cfg.shortcuts[$0].id }) { remove(id) }
                }
                #endif
                Button {
                    editingShortcut = ShortcutDraft(id: cfg.nextShortcutID, label: "", keys: [], isNew: true)
                } label: {
                    Label(L10n.ringShortcutAdd, systemImage: "plus")
                }
            } header: {
                Text(L10n.ringEditorShortcuts)
            } footer: {
                Text(L10n.ringEditorShortcutsHint)
                    .font(.geist(13, relativeTo: .footnote))
            }
            Section {
                Button(L10n.ringReset, role: .destructive, action: reset)
            }
        }
        #if os(macOS)
        // The settings tabs' own style, so the editor reads as one more preferences page rather
        // than the plain two-column form a bare macOS Form draws.
        .formStyle(.grouped)
        #endif
        .navigationTitle(L10n.ringEditorTitle)
        .sheet(item: $picking) { p in
            SlotPicker(groups: groups, current: cfg.ring[p.k]?.id ?? "") { id in
                set(p.k, id)
                picking = nil
            }
            // A macOS sheet sizes to its content, and a List's content is no size at all.
            #if os(macOS)
            .frame(width: 420, height: 520)
            #endif
        }
        .sheet(item: $editingShortcut) { draft in
            ShortcutEditor(draft: draft, save: save, delete: { remove(draft.id) })
                #if os(macOS)
                .frame(width: 460, height: 600)
                #endif
        }
        #if os(iOS) || os(visionOS)
        // Full screen deliberately, not a sheet: settings live in a sheet, and on an iPad a
        // sheet is a card — its geometry (and even the wide/narrow layout class) would lie
        // about the stream the layout is for.
        .fullScreenCover(isPresented: $editingLayout) {
            PadLayoutEditor(pad: cfg.pad) { p in setPad { $0 = p } }
        }
        #endif
    }

    private var groups: [SlotGroup] { slotGroups(for: cfg) }

    private func set(_ k: Int, _ id: String) {
        var c = cfg
        c.ring[k] = SlotId.parse(id)
        blob = c.toJSON()
    }

    private func setPad(_ change: (inout PadConfig) -> Void) {
        var c = cfg
        change(&c.pad)
        blob = c.toJSON()
    }

    private func swap(_ a: Int, _ b: Int) {
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

/// A slider that names its value as a percentage and writes it when the finger lifts, not per frame.
private struct PadSlider: View {
    let label: LocalizedStringResource
    let value: Float
    let range: ClosedRange<Float>
    let commit: (Float) -> Void
    @State private var live: Float = 0

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(String(localized: label) + " · "
                 + L10n.commonPercent(value: String(Int((live * 100).rounded()))))
            Slider(value: $live, in: range) { editing in
                if !editing { commit(live) }
            }
        }
        .onAppear { live = value }
        .onChange(of: value) { _, v in live = v }
    }
}

/// A chord on a disc the size the ring draws it, for lists and the editing sheet.
private struct KeycapDisc: View {
    let keys: [String]
    var size: CGFloat = 56

    var body: some View {
        ZStack {
            Circle().fill(Color(white: 0.22))
            Circle().strokeBorder(Color.white.opacity(0.18), lineWidth: 1)
            if keys.isEmpty {
                Image(systemName: "questionmark").font(.system(size: size * 0.35, weight: .semibold))
            } else {
                ChordKeycap(keys: keys).scaleEffect(size / 56)
            }
        }
        .foregroundStyle(.white)
        .frame(width: size, height: size)
    }
}

/// The catalogue by group; the current pick is ticked.
struct SlotPicker: View {
    let groups: [SlotGroup]
    let current: String
    let choose: (String) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            List {
                ForEach(groups) { g in
                    Section(g.title) {
                        ForEach(g.options) { o in
                            Button {
                                choose(o.id)
                            } label: {
                                HStack {
                                    VStack(alignment: .leading, spacing: 2) {
                                        Text(o.label)
                                        if let note = o.note {
                                            Text(note).font(.footnote).foregroundStyle(.secondary)
                                        }
                                    }
                                    Spacer()
                                    if o.id == current { Image(systemName: "checkmark") }
                                }
                            }
                            .foregroundStyle(.primary)
                        }
                    }
                }
            }
            .navigationTitle(L10n.ringPickerTitle)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button(L10n.commonCancel) { dismiss() } }
            }
        }
    }
}

/// One shortcut: a name, the modifiers held as chips, the key it ends on picked from a keyboard,
/// and the disc as the ring will draw it.
private struct ShortcutEditor: View {
    @State var draft: ShortcutDraft
    let save: (ShortcutDraft) -> Void
    let delete: () -> Void
    @Environment(\.dismiss) private var dismiss

    private var mods: [String] { draft.keys.filter { modifierKeys.contains($0) } }
    private var key: String? { draft.keys.first { !modifierKeys.contains($0) } }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    HStack(spacing: 14) {
                        KeycapDisc(keys: draft.keys)
                        VStack(alignment: .leading, spacing: 3) {
                            Text(draft.label.isEmpty
                                 ? (draft.keys.isEmpty ? String(localized: L10n.ringShortcutNoKey) : chordChip(draft.keys))
                                 : draft.label)
                                .font(.geist(15, .medium))
                            Text(draft.keys.isEmpty ? String(localized: L10n.ringShortcutPreview) : chordChip(draft.keys))
                                .font(.footnote)
                                .foregroundStyle(.secondary)
                        }
                    }
                    .padding(.vertical, 4)
                    TextField(String(localized: L10n.ringShortcutNameOptional), text: $draft.label)
                }
                Section(String(localized: L10n.ringShortcutModifiers)) {
                    HStack(spacing: 8) {
                        ForEach(modifierKeys, id: \.self) { m in
                            let on = mods.contains(m)
                            Button(keyLegend(m)) { toggle(m) }
                                .buttonStyle(.bordered)
                                .tint(on ? Color.brand : Color.secondary)
                                .fontWeight(on ? .semibold : .regular)
                                .accessibilityAddTraits(on ? .isSelected : [])
                        }
                    }
                }
                ForEach(keyGroups, id: \.id) { group in
                    Section(group.title) {
                        // Word keys (Backspace, PgDn, PrtSc) get wider cells; every cap keeps
                        // one line at one height and shrinks its text rather than growing.
                        let wide = group.keys.contains { keyLegend($0).count > 2 }
                        LazyVGrid(columns: [GridItem(.adaptive(minimum: wide ? 80 : 44), spacing: 6)], spacing: 6) {
                            ForEach(group.keys, id: \.self) { k in
                                let on = key == k
                                Button {
                                    pick(k)
                                } label: {
                                    Text(keyLegend(k))
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
                if !draft.isNew {
                    Section {
                        Button(L10n.ringShortcutRemove, role: .destructive) {
                            delete()
                            dismiss()
                        }
                    }
                }
            }
            #if os(macOS)
            .formStyle(.grouped)
            #endif
            .navigationTitle(draft.isNew ? L10n.ringShortcutNewTitle : L10n.ringShortcutTitle)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button(L10n.commonCancel) { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button(draft.isNew ? L10n.commonAdd : L10n.commonSave) {
                        // A name of spaces is no name: the row then shows the chord.
                        var named = draft
                        named.label = draft.label.trimmingCharacters(in: .whitespaces)
                        save(named)
                        dismiss()
                    }
                    // A modifier alone is a combination: Win opens the host's Start menu.
                    .disabled(draft.keys.isEmpty)
                }
            }
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
