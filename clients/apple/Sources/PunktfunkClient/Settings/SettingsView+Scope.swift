// The settings surface edits SETTINGS PRESETS too (design/client-settings-profiles.md §5.1): a
// scope switcher swaps the whole screen between the global defaults and one preset's overrides.
//
// It is deliberately not a second editor. A parallel preset editor would drift from this one
// field by field, and the caption/curation work of the 2026-07 revamp would have to be done twice.
// So the section builders stay the single definition of every row and simply change which LAYER
// their binding reads and writes.
//
// Three rules make it a preset editor rather than a settings copier:
//   • every row shows the EFFECTIVE value — the inherited global until this preset overrides it;
//   • touching a control records the override, always, even when the new value happens to equal
//     today's global (that is a PIN: it keeps its value when the global later moves). Overrides
//     are never inferred by diffing at save time;
//   • the only way back to inheriting is an explicit per-row reset — which is why the marker and
//     its Reset action below are not optional garnish: without them a preset is a one-way door.
//
// Tier-G/H rows (this device's endpoints and hardware, properties of a host) simply don't render
// in preset scope — see §3's curation and `isPresetable` at each call site.

import PunktfunkKit
import SwiftUI

/// Which layer the settings surface is editing.
enum SettingsScope: Equatable, Hashable {
    /// The global defaults every preset inherits from — the only scope before this feature.
    case defaults
    /// One preset's overrides, by id.
    case preset(String)

    var presetID: String? {
        if case .preset(let id) = self { return id }
        return nil
    }
}

/// One presetable setting, in the one place that knows all three of its faces: the
/// `UserDefaults` key its global lives under, the overlay slot an override lives in, and the
/// serialized name a per-row reset carries. Keeping them together is what stops a row from
/// writing an override the reset button can't find.
struct SettingsField<Value> {
    /// The overlay's serialized field name — also the reset key. `resolution` is the one alias,
    /// covering the width/height/match-window tri-state a single control drives.
    let name: String
    let key: String
    let overlay: WritableKeyPath<SettingsOverlay, Value?>
    let effective: KeyPath<EffectiveSettings, Value>
}

/// The catalog of presetable rows. A plain namespace rather than statics on the generic
/// `SettingsField` itself: members of a generic type can't have their parameter inferred from the
/// member alone, so every call site would need to spell the type out anyway.
enum SettingsFields {
    static var refreshHz: SettingsField<Int> {
        .init(name: "refresh_hz", key: DefaultsKey.streamHz,
              overlay: \.refreshHz, effective: \.refreshHz)
    }
    static var matchWindow: SettingsField<Bool> {
        // Part of the resolution tri-state, so its RESET name is the shared alias.
        .init(name: OverlayField.resolution, key: DefaultsKey.matchWindow,
              overlay: \.matchWindow, effective: \.matchWindow)
    }
    static var width: SettingsField<Int> {
        .init(name: OverlayField.resolution, key: DefaultsKey.streamWidth,
              overlay: \.width, effective: \.width)
    }
    static var height: SettingsField<Int> {
        .init(name: OverlayField.resolution, key: DefaultsKey.streamHeight,
              overlay: \.height, effective: \.height)
    }
    static var renderScale: SettingsField<Double> {
        .init(name: "render_scale", key: DefaultsKey.renderScale,
              overlay: \.renderScale, effective: \.renderScale)
    }
    static var videoFit: SettingsField<String> {
        .init(name: "video_fit", key: DefaultsKey.videoFit,
              overlay: \.videoFit, effective: \.videoFit)
    }
    static var bitrateKbps: SettingsField<Int> {
        .init(name: "bitrate_kbps", key: DefaultsKey.bitrateKbps,
              overlay: \.bitrateKbps, effective: \.bitrateKbps)
    }
    static var pyrowaveBpp: SettingsField<Double> {
        .init(name: "pyrowave_bpp", key: DefaultsKey.pyrowaveBpp,
              overlay: \.pyrowaveBpp, effective: \.pyrowaveBpp)
    }
    static var codec: SettingsField<String> {
        .init(name: "codec", key: DefaultsKey.codec, overlay: \.codec, effective: \.codec)
    }
    static var hdrEnabled: SettingsField<Bool> {
        .init(name: "hdr_enabled", key: DefaultsKey.hdrEnabled,
              overlay: \.hdrEnabled, effective: \.hdrEnabled)
    }
    static var enable444: SettingsField<Bool> {
        .init(name: "enable_444", key: DefaultsKey.enable444,
              overlay: \.enable444, effective: \.enable444)
    }
    static var tenBitSdr: SettingsField<Bool> {
        .init(name: "ten_bit_sdr", key: DefaultsKey.tenBitSdr,
              overlay: \.tenBitSdr, effective: \.tenBitSdr)
    }
    static var compositor: SettingsField<Int> {
        .init(name: "compositor", key: DefaultsKey.compositor,
              overlay: \.compositor, effective: \.compositor)
    }
    static var audioChannels: SettingsField<Int> {
        .init(name: "audio_channels", key: DefaultsKey.audioChannels,
              overlay: \.audioChannels, effective: \.audioChannels)
    }
    static var audioFormat: SettingsField<String> {
        .init(name: "audio_format", key: DefaultsKey.audioFormat,
              overlay: \.audioFormat, effective: \.audioFormat)
    }
    static var micEnabled: SettingsField<Bool> {
        .init(name: "mic_enabled", key: DefaultsKey.micEnabled,
              overlay: \.micEnabled, effective: \.micEnabled)
    }
    static var echoCancel: SettingsField<Bool> {
        .init(name: "echo_cancel", key: DefaultsKey.echoCancel,
              overlay: \.echoCancel, effective: \.echoCancel)
    }
    static var keepHostAudio: SettingsField<Bool> {
        .init(name: "keep_host_audio", key: DefaultsKey.keepHostAudio,
              overlay: \.keepHostAudio, effective: \.keepHostAudio)
    }
    static var touchMode: SettingsField<String> {
        .init(name: "touch_mode", key: DefaultsKey.touchMode,
              overlay: \.touchMode, effective: \.touchMode)
    }
    static var mouseMode: SettingsField<String> {
        .init(name: "mouse_mode", key: DefaultsKey.mouseMode,
              overlay: \.mouseMode, effective: \.mouseMode)
    }
    static var invertScroll: SettingsField<Bool> {
        .init(name: "invert_scroll", key: DefaultsKey.invertScroll,
              overlay: \.invertScroll, effective: \.invertScroll)
    }
    static var overlayActions: SettingsField<String> {
        .init(name: "overlay_actions", key: DefaultsKey.overlayActions,
              overlay: \.overlayActions, effective: \.overlayActions)
    }
    static var inhibitShortcuts: SettingsField<Bool> {
        .init(name: "inhibit_shortcuts", key: DefaultsKey.inhibitShortcuts,
              overlay: \.inhibitShortcuts, effective: \.inhibitShortcuts)
    }
    static var modifierLayout: SettingsField<String> {
        .init(name: "modifier_layout", key: DefaultsKey.modifierLayout,
              overlay: \.modifierLayout, effective: \.modifierLayout)
    }
    static var gamepadType: SettingsField<Int> {
        .init(name: "gamepad", key: DefaultsKey.gamepadType,
              overlay: \.gamepadType, effective: \.gamepadType)
    }
    static var gamepadForwarding: SettingsField<Bool> {
        .init(name: "gamepad_forwarding", key: DefaultsKey.gamepadForwarding,
              overlay: \.gamepadForwarding, effective: \.gamepadForwarding)
    }
    static var systemButtons: SettingsField<String> {
        .init(name: "system_buttons", key: DefaultsKey.systemButtons,
              overlay: \.systemButtons, effective: \.systemButtons)
    }
    static var guideGesture: SettingsField<String> {
        .init(name: "guide_gesture", key: DefaultsKey.guideGesture,
              overlay: \.guideGesture, effective: \.guideGesture)
    }
    static var statsVerbosity: SettingsField<String> {
        .init(name: "stats_verbosity", key: DefaultsKey.statsVerbosity,
              overlay: \.statsVerbosity, effective: \.statsVerbosity)
    }
    static var fullscreenWhileStreaming: SettingsField<Bool> {
        .init(name: "fullscreen_on_stream", key: DefaultsKey.fullscreenWhileStreaming,
              overlay: \.fullscreenWhileStreaming, effective: \.fullscreenWhileStreaming)
    }
    static var presentPriority: SettingsField<String> {
        .init(name: "present_priority", key: DefaultsKey.presentPriority,
              overlay: \.presentPriority, effective: \.presentPriority)
    }
    static var smoothBuffer: SettingsField<Int> {
        .init(name: "smooth_buffer", key: DefaultsKey.smoothBuffer,
              overlay: \.smoothBuffer, effective: \.smoothBuffer)
    }
    static var vsync: SettingsField<Bool> {
        .init(name: "vsync", key: DefaultsKey.vsync, overlay: \.vsync, effective: \.vsync)
    }
    static var allowVRR: SettingsField<Bool> {
        .init(name: "allow_vrr", key: DefaultsKey.allowVRR,
              overlay: \.allowVRR, effective: \.allowVRR)
    }
}

extension SettingsView {
    // MARK: - The layer being edited

    var activePreset: StreamPreset? {
        scope.presetID.flatMap { presets.preset(id: $0) }
    }

    /// True while a preset is being edited — the gate every tier-G/H row is hidden behind.
    var inPresetScope: Bool { activePreset != nil }

    /// What every row displays: the globals as this view's `@AppStorage` sees them (so SwiftUI
    /// tracks each of them), with the edited preset's overrides on top. A row the preset doesn't
    /// override therefore reads as the LIVE global, which is what inherit-by-default has to look
    /// like.
    var effective: EffectiveSettings {
        var base = EffectiveSettings()
        base.width = width
        base.height = height
        base.refreshHz = hz
        base.matchWindow = matchWindow
        base.bitrateKbps = bitrateKbps
        base.pyrowaveBpp = pyrowaveBpp
        base.renderScale = renderScale
        base.videoFit = videoFit
        base.codec = codec
        base.hdrEnabled = hdrEnabled
        base.enable444 = enable444
        base.tenBitSdr = tenBitSdr
        base.compositor = compositor
        base.audioChannels = audioChannels
        base.audioFormat = audioFormat
        base.micEnabled = micEnabled
        base.echoCancel = echoCancel
        base.keepHostAudio = keepHostAudio
        base.gamepadType = gamepadType
        base.gamepadForwarding = gamepadForwarding
        base.systemButtons = systemButtons
        base.guideGesture = guideGesture
        base.statsVerbosity = statsVerbosityRaw
        base.fullscreenWhileStreaming = fullscreenWhileStreaming
        base.presentPriority = presentPriority
        base.smoothBuffer = smoothBuffer
        base.overlayActions = overlayActions
        #if !os(tvOS)
        base.invertScroll = invertScroll
        base.modifierLayout = modifierLayout
        base.allowVRR = allowVRR
        #endif
        #if os(macOS)
        base.mouseMode = mouseMode
        base.inhibitShortcuts = inhibitShortcuts
        base.vsync = vsync
        #endif
        #if os(iOS) || os(visionOS)
        base.touchMode = touchMode
        #endif
        guard let preset = activePreset else { return base }
        return base.applying(preset.overrides)
    }

    // MARK: - Scoped bindings

    /// A control's binding for whichever layer is being edited: `UserDefaults` in defaults scope,
    /// the preset's overlay in preset scope.
    ///
    /// The setter always WRITES on touch. It never compares the new value against the global to
    /// decide whether to record an override — that comparison is exactly the "copy the settings"
    /// behaviour the design rejects, and it would silently drop a deliberate pin.
    func scoped<Value>(_ field: SettingsField<Value>) -> Binding<Value> {
        Binding(
            get: { effective[keyPath: field.effective] },
            set: { newValue in
                guard let id = scope.presetID else {
                    // @AppStorage observes this key, so the write re-renders the whole surface.
                    UserDefaults.standard.set(newValue, forKey: field.key)
                    return
                }
                presets.setOverride(id, field.overlay, newValue)
            })
    }

    /// The resolution row's tri-state, written as one act: a single control drives width, height
    /// and (where it exists) match-window, so all of them move together and one reset clears all
    /// three.
    func setResolution(width newWidth: Int? = nil, height newHeight: Int? = nil,
                       matchWindow newMatch: Bool? = nil) {
        if let newWidth { scoped(SettingsFields.width).wrappedValue = newWidth }
        if let newHeight { scoped(SettingsFields.height).wrappedValue = newHeight }
        if let newMatch { scoped(SettingsFields.matchWindow).wrappedValue = newMatch }
    }

    // MARK: - Override markers + per-row reset

    /// Does the edited preset override this row? False in defaults scope, where there is nothing
    /// to inherit from.
    func isOverridden(_ name: String) -> Bool {
        guard let preset = activePreset else { return false }
        return OverlayField.isOverridden(name, in: preset.overrides)
    }

    /// Put a row back to inheriting. The ONLY way an override is removed — the model never infers
    /// "not overridden" from a value comparison.
    func resetOverride(_ name: String) {
        guard let id = scope.presetID else { return }
        presets.clearOverride(id, field: name)
    }

    /// The accent dot + Reset affordance a row carries while it overrides the defaults. Rendered
    /// inside `described`'s caption line, so it sits with the row it belongs to instead of in some
    /// separate list of exceptions.
    ///
    /// Reset is a BORDERED control pushed to the far edge, not tinted text next to the label: as a
    /// plain button it read as a third word of the notice, and the one action that can undo an
    /// override must not look like prose.
    @ViewBuilder
    func overrideMarker(_ name: String) -> some View {
        if isOverridden(name) {
            HStack(spacing: 5) {
                Circle()
                    .fill(Color.brand)
                    .frame(width: 6, height: 6)
                    // The adjacent text says the state; the dot is the at-a-glance version.
                    .accessibilityHidden(true)
                Text("Overrides Default settings")
                    .font(.geist(12, .medium, relativeTo: .caption2))
                    .foregroundStyle(Color.brand)
                Spacer(minLength: 12)
                Button {
                    resetOverride(name)
                } label: {
                    Label("Reset", systemImage: "arrow.uturn.backward")
                        .font(.geist(11, .medium, relativeTo: .caption2))
                }
                .buttonStyle(.bordered)
                .controlSize(.small)
                .accessibilityLabel("Reset to Default settings")
                .help("Stop overriding this — follow Default settings again")
            }
            // The marker carries a bordered button, which needs more air under the control or
            // caption above it than a line of text would. Here rather than at each call site, so
            // every row that shows it is spaced the same.
            .padding(.top, 4)
        }
    }

    // MARK: - The scope switcher

    /// The menu itself — which layer to edit, plus the edited preset's own management actions.
    /// ONE definition, two chromes: the macOS preferences window heads itself with a button menu
    /// (`scopeSwitcher`), while iOS puts a value row at the top of the settings list
    /// (`scopeRow`) — a form sheet has no window header to hang a button off, and a bordered
    /// button dropped into a `List` renders as neither a row nor a control.
    ///
    /// A TV shows the same choices and acts as rows in its Editing pane (`tvPresetManager`): a
    /// remote does menus inside menus badly.
    @ViewBuilder
    var scopeMenuContent: some View {
        // A Picker rather than hand-rolled buttons: the platform owns the selection checkmark
        // and draws it in its OWN column, which leaves each row's icon free to be the preset's
        // colour chip. Rolling the selection by hand would have cost one or the other.
        Picker("Editing", selection: scopeSelection) {
            Label("Default settings", systemImage: "gearshape")
                .tag(SettingsScope.defaults)
            ForEach(presets.presets) { preset in
                Label {
                    Text(preset.name)
                } icon: {
                    // Rasterised, not tinted — see MenuIcon: a tinted symbol in a menu is a
                    // stencil, and the colour never arrives.
                    if let chip = MenuIcon.swatch(preset.accentColor, scheme: colorScheme) {
                        chip
                    } else {
                        Image(systemName: "circle.fill")
                    }
                }
                .tag(SettingsScope.preset(preset.id))
            }
        }
        .pickerStyle(.inline)
        Divider()
        // Three actions, one sheet. A preset is a name and a colour; deciding them in separate
        // menu items — each raising its own bare alert — is how "make a Work preset" became four
        // trips through this menu.
        Button {
            presetDraft = .create()
        } label: {
            Label("New Preset…", systemImage: "plus")
        }
        if let active = activePreset {
            Button {
                presetDraft = .edit(active)
            } label: {
                Label("Edit “\(active.name)”…", systemImage: "pencil")
            }
            Button {
                presetDraft = .duplicate(active, name: Self.copyName(of: active.name, in: presets))
            } label: {
                Label("Duplicate “\(active.name)”…", systemImage: "plus.square.on.square")
            }
            Divider()
            Button(role: .destructive) {
                presetPendingDelete = active
            } label: {
                Label {
                    Text("Delete “\(active.name)”…")
                } icon: {
                    // The destructive ROLE colours the row on iOS and nothing at all on macOS,
                    // and either way it leaves the icon as a stencil — so the red is rendered in.
                    if let trash = MenuIcon.symbol("trash", color: .red, scheme: colorScheme) {
                        trash
                    } else {
                        Image(systemName: "trash")
                    }
                }
            }
        }
    }

    /// The name of the layer being edited, and one line on what editing it means. Shared by both
    /// chromes — the caption is a stacked line on macOS and the list section's footer on iOS.
    var scopeName: String { activePreset?.name ?? "Default settings" }

    /// The edited layer as a glyph: a preset's own colour, or the settings gear for the
    /// defaults. Colour is only worth choosing if it shows up where you chose it.
    @ViewBuilder
    var scopeDot: some View {
        if let preset = activePreset {
            Circle()
                .fill(preset.accentColor)
                .frame(width: Self.scopeDotSize, height: Self.scopeDotSize)
                .accessibilityHidden(true) // the name is right beside it
        } else {
            Image(systemName: "gearshape")
                .foregroundStyle(.secondary)
        }
    }

    #if os(tvOS)
    static let scopeDotSize: CGFloat = 16
    #else
    static let scopeDotSize: CGFloat = 9
    #endif

    var scopeCaption: String {
        activePreset == nil
            ? "The settings every preset inherits from."
            : "Overrides Default settings for hosts that use this preset. Untouched rows keep "
                + "following the defaults."
    }

    #if os(macOS)
    /// The control that heads the preferences window.
    var scopeSwitcher: some View {
        presetPrompts(
            VStack(alignment: .leading, spacing: 4) {
                Menu { scopeMenuContent } label: {
                    HStack(spacing: 6) {
                        scopeDot
                        Text(scopeName)
                    }
                }
                .menuStyle(.button)
                .fixedSize()
                // Same reason as the host grid's sort menu: a Menu draws its label — and the
                // icons of everything inside it — in the ACCENT colour, which put a purple wash
                // over a control whose only meaningful colour is the preset chips. Those are
                // rendered bitmaps (see MenuIcon), so they keep their colour; the decoration
                // loses its. macOS only — iOS menus already draw their icons in the label colour.
                .tint(.primary)
                Text(scopeCaption)
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            })
    }
    #endif

    #if os(iOS) || os(visionOS)
    /// A standard value row at the head of the settings list: label on the left, current layer on
    /// the right, the system's up/down chevron. Its caption belongs to the section FOOTER, not to
    /// the row — stacked inside the row it wrapped to four lines and made the first thing on the
    /// screen a block of text.
    var scopeRow: some View {
        presetPrompts(
            Menu { scopeMenuContent } label: {
                HStack(spacing: 8) {
                    Text("Editing")
                        .foregroundStyle(.primary)
                    Spacer(minLength: 8)
                    scopeDot
                    Text(scopeName)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    Image(systemName: "chevron.up.chevron.down")
                        .font(.footnote.weight(.semibold))
                        .foregroundStyle(.tertiary)
                        .accessibilityHidden(true) // the menu itself announces the action
                }
                // The whole row opens the menu, not just the glyphs.
                .contentShape(Rectangle())
            })
    }
    #endif

    /// The editor and the delete confirmation, attached wherever the menu or the TV's Editing
    /// pane lives.
    func presetPrompts<Content: View>(_ content: Content) -> some View {
        content
            .sheet(item: $presetDraft) { draft in
                PresetEditorSheet(draft: draft) { scope = $0 }
            }
            // Deleting warns with what it changes (§6): bound hosts fall back to Default settings
            // and pinned cards disappear. Neither is an error, but neither should be a surprise —
            // so the counts are in the question, not discovered afterwards.
            .alert(
                "Delete “\(presetPendingDelete?.name ?? "")”?",
                isPresented: presetDeletePresented,
                presenting: presetPendingDelete
            ) { preset in
                Button("Cancel", role: .cancel) { presetPendingDelete = nil }
                Button("Delete", role: .destructive) {
                    presets.delete(preset.id)
                    scope = .defaults
                    presetPendingDelete = nil
                }
            } message: { preset in
                Text(deleteWarning(for: preset))
            }
    }

    private var presetDeletePresented: Binding<Bool> {
        Binding(
            get: { presetPendingDelete != nil },
            set: { if !$0 { presetPendingDelete = nil } })
    }

    private func deleteWarning(for preset: StreamPreset) -> String {
        let (bound, pinned) = presets.usage(of: preset.id)
        var parts: [String] = []
        if bound > 0 {
            parts.append("\(bound) host\(bound == 1 ? "" : "s") will fall back to Default settings")
        }
        if pinned > 0 {
            parts.append("\(pinned) pinned card\(pinned == 1 ? "" : "s") will disappear")
        }
        guard !parts.isEmpty else { return "Nothing uses this preset yet." }
        return parts.joined(separator: ", ") + "."
    }

    /// Switching scope is a plain state change — the rows are built by one code path and simply
    /// read the other layer, so there is nothing to commit on the way out.
    private var scopeSelection: Binding<SettingsScope> {
        Binding(get: { scope }, set: { scope = $0 })
    }

    /// "Game copy", "Game copy 2", … — the first name that isn't taken, so Duplicate never opens
    /// with a name the accept button refuses.
    static func copyName(of name: String, in store: PresetStore) -> String {
        let base = "\(name) copy"
        if !store.nameTaken(base) { return base }
        for n in 2...99 where !store.nameTaken("\(base) \(n)") { return "\(base) \(n)" }
        return base
    }

}
