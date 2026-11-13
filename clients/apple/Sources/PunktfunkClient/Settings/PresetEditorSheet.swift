// Creating, duplicating, renaming, recolouring and deleting a settings preset — one sheet for
// all five (design/client-settings-profiles.md §5.1).
//
// It replaced a menu of four separate actions, three of which raised their own bare text alert
// and the fourth a submenu of colour NAMES with no colour anywhere on it. Making a preset then
// meant: name it in an alert, find it again in the scope menu, open a submenu, and pick "Amber"
// on faith. The two things a preset has — a name and a colour — are decided together here, with
// a live chip showing exactly what the host cards will render.

import PunktfunkKit
import SwiftUI

/// What the sheet was opened to do. Carrying the seed values rather than an id keeps the sheet
/// free of "which store do I read to find out what I'm editing" — the caller already knows.
struct PresetDraft: Identifiable {
    enum Kind: String { case create, edit, duplicate }

    let kind: Kind
    /// nil = creating (a blank preset, or a duplicate); set = editing that preset.
    var editingID: String?
    var name: String
    var accent: String?
    /// What a newly created preset starts with. Empty for a blank one; the source's overrides
    /// for a duplicate, which is the whole point of duplicating.
    var overrides = SettingsOverlay()
    var title: String
    var accept: String

    /// A sheet of each kind is its own sheet: the kind keys a new one, never its title.
    var id: String { editingID ?? "new-\(kind.rawValue)" }

    static func create() -> PresetDraft {
        PresetDraft(
            kind: .create, name: "", accent: nil, title: String(localized: L10n.presetNewTitle),
            accept: String(localized: L10n.commonCreate))
    }

    static func edit(_ preset: StreamPreset) -> PresetDraft {
        PresetDraft(
            kind: .edit, editingID: preset.id, name: preset.name, accent: preset.accent,
            title: String(localized: L10n.presetEditTitle),
            accept: String(localized: L10n.commonSave))
    }

    static func duplicate(_ preset: StreamPreset, name: String) -> PresetDraft {
        PresetDraft(
            kind: .duplicate, name: name, accent: preset.accent, overrides: preset.overrides,
            title: String(localized: L10n.applePresetDuplicateTitle),
            accept: String(localized: L10n.commonDuplicate))
    }
}

struct PresetEditorSheet: View {
    @Environment(\.dismiss) private var dismiss
    @ObservedObject private var presets = PresetStore.shared

    let draft: PresetDraft
    /// Where the settings surface should be pointing afterwards — at the preset that was just
    /// created, so you land in the layer you made rather than back on the defaults.
    let onScope: (SettingsScope) -> Void

    @State private var name: String
    @State private var accent: String?
    #if os(tvOS)
    /// The system keyboard for the name, full screen as tvOS text entry is.
    @State private var editingName = false
    #endif

    init(draft: PresetDraft, onScope: @escaping (SettingsScope) -> Void) {
        self.draft = draft
        self.onScope = onScope
        _name = State(initialValue: draft.name)
        _accent = State(initialValue: draft.accent)
    }

    var body: some View {
        #if os(macOS)
        VStack(spacing: 0) {
            form
            HStack {
                Button(L10n.commonCancel, role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Spacer()
                Button(draft.accept) { commit() }
                    .glassProminentButtonStyle()
                    .keyboardShortcut(.defaultAction)
                    .disabled(!isNameAcceptable)
            }
            .padding(16)
        }
        .frame(width: 420)
        .fixedSize(horizontal: false, vertical: true)
        #elseif os(tvOS)
        tvForm
        #else
        NavigationStack {
            form
                .navigationTitle(draft.title)
                #if os(iOS) || os(visionOS)
                .navigationBarTitleDisplayMode(.inline)
                #endif
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button(L10n.commonCancel) { dismiss() }
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button(draft.accept) { commit() }
                            .disabled(!isNameAcceptable)
                    }
                }
        }
        .presentationDetents([.medium, .large])
        .presentationDragIndicator(.visible)
        #endif
    }

    // MARK: - Form

    private var form: some View {
        Form {
            Section {
                preview
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 8)
                    .listRowInsets(EdgeInsets())
                    .listRowBackground(Color.clear)
            }
            Section {
                TextField(L10n.presetName, text: $name, prompt: Text(L10n.presetNamePlaceholder))
                    #if os(iOS) || os(visionOS)
                    .textInputAutocapitalization(.words)
                    #endif
            } footer: {
                Text(nameFootnote)
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(duplicateName ? Color.red : Color.secondary)
            }
            Section {
                swatches
                    .listRowInsets(EdgeInsets(top: 12, leading: 16, bottom: 12, trailing: 16))
            } header: {
                Text(L10n.presetColor)
            } footer: {
                Text(L10n.presetColorHint)
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        #if os(macOS)
        .frame(minHeight: 420)
        #endif
    }

    /// The preset exactly as a host card will show it — the answer to "what am I choosing?",
    /// which a list of colour names never gave.
    private var preview: some View {
        let shown = name.trimmingCharacters(in: .whitespaces)
        return PresetChip(
            preset: StreamPreset(
                name: shown.isEmpty ? String(localized: L10n.applePresetPreviewName) : shown,
                accent: accent),
            size: Self.chipSize, prominent: true)
            .opacity(shown.isEmpty ? 0.5 : 1)
            .animation(.easeOut(duration: 0.15), value: accent)
    }

    #if os(tvOS)
    private static let chipSize: CGFloat = 28
    #else
    private static let chipSize: CGFloat = 13
    #endif

    /// The palette, as colours. `nil` is the brand default and leads, so "no colour" is a choice
    /// on the same shelf as the rest rather than the absence of one.
    private var swatches: some View {
        LazyVGrid(
            columns: [GridItem(.adaptive(minimum: 44), spacing: 14, alignment: .center)],
            spacing: 14
        ) {
            swatch(nil, label: String(localized: L10n.presetColorNone))
            ForEach(PresetAccent.palette) { option in
                swatch(option.hex, label: option.name)
            }
        }
    }

    private func swatch(_ hex: String?, label: String) -> some View {
        let selected = accent?.caseInsensitiveCompare(hex ?? "") == .orderedSame
            || (accent == nil && hex == nil)
        let color = hex.flatMap { Color(hex: $0) } ?? .brand
        return Button {
            accent = hex
        } label: {
            ZStack {
                Circle()
                    .fill(color)
                    .frame(width: 30, height: 30)
                if selected {
                    Image(systemName: "checkmark")
                        .font(.footnote.weight(.bold))
                        .foregroundStyle(.white)
                    // A ring OUTSIDE the swatch as well as the checkmark: the tick alone washes
                    // out on the pale hues and the ring alone is easy to miss at this size.
                    Circle()
                        .strokeBorder(color, lineWidth: 2)
                        .frame(width: 40, height: 40)
                }
            }
            // 30pt is under the touch minimum; the cell is the target, not the dot.
            .frame(width: 44, height: 44)
            .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(label)
        .accessibilityAddTraits(selected ? [.isButton, .isSelected] : .isButton)
        .help(label)
    }

    #if os(tvOS)
    /// The TV's form: the name through the system keyboard, the colours as focusable swatches,
    /// and the two buttons at the bottom, since a TV sheet has no toolbar to hold them.
    private var tvForm: some View {
        VStack(spacing: 36) {
            Text(draft.title)
                .font(.geist(40, .bold, relativeTo: .title))
            preview
            TVFieldRow(
                label: String(localized: L10n.presetName), value: name,
                placeholder: String(localized: L10n.presetNamePlaceholder)
            ) {
                editingName = true
            }
            Text(nameFootnote)
                .font(.geist(22, relativeTo: .caption))
                .foregroundStyle(duplicateName ? Color.red : Color.secondary)
            HStack(spacing: 24) {
                tvSwatch(nil, label: String(localized: L10n.presetColorNone))
                ForEach(PresetAccent.palette) { option in
                    tvSwatch(option.hex, label: option.name)
                }
            }
            .focusSection()
            HStack(spacing: 32) {
                Button(L10n.commonCancel, role: .cancel) { dismiss() }
                Button(draft.accept) { commit() }
                    .disabled(!isNameAcceptable)
            }
        }
        .frame(maxWidth: 1100)
        .padding(60)
        .fullScreenCover(isPresented: $editingName) {
            TVTextEntry(title: String(localized: L10n.presetName), text: name) { typed in
                name = typed
                editingName = false
            }
        }
    }

    /// One colour, focusable: the platter draws the focus, the tick says which is chosen.
    private func tvSwatch(_ hex: String?, label: String) -> some View {
        let selected = accent?.caseInsensitiveCompare(hex ?? "") == .orderedSame
            || (accent == nil && hex == nil)
        return Button {
            accent = hex
        } label: {
            ZStack {
                Circle()
                    .fill(hex.flatMap { Color(hex: $0) } ?? .brand)
                    .frame(width: 52, height: 52)
                if selected {
                    Image(systemName: "checkmark")
                        .font(.system(size: 24, weight: .bold))
                        .foregroundStyle(.white)
                }
            }
        }
        .buttonStyle(.bordered)
        .buttonBorderShape(.circle)
        .accessibilityLabel(label)
        .accessibilityAddTraits(selected ? [.isButton, .isSelected] : .isButton)
    }
    #endif

    // MARK: - Validation + commit

    private var trimmedName: String { name.trimmingCharacters(in: .whitespaces) }

    /// Case-insensitively unique — two "Work"s make every menu ambiguous, and the deep-link
    /// grammar has to refuse an ambiguous reference rather than guess which one was meant.
    private var duplicateName: Bool {
        !trimmedName.isEmpty && presets.nameTaken(trimmedName, except: draft.editingID)
    }

    private var isNameAcceptable: Bool { !trimmedName.isEmpty && !duplicateName }

    private var nameFootnote: String {
        if duplicateName { return L10n.presetNameTaken(name: trimmedName) }
        return draft.editingID == nil
            ? String(localized: L10n.presetNewHint)
            : String(localized: L10n.applePresetEditHint)
    }

    private func commit() {
        guard isNameAcceptable else { return }
        if let id = draft.editingID {
            presets.rename(id, to: trimmedName)
            presets.setAccent(id, to: accent)
        } else {
            var preset = StreamPreset(name: trimmedName, accent: accent)
            preset.overrides = draft.overrides
            presets.add(preset)
            // Land in the thing that was just made — creating a preset and being left on the
            // defaults is how you end up editing the wrong layer.
            onScope(.preset(preset.id))
        }
        dismiss()
    }
}
