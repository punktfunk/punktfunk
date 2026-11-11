// Which layer the settings surface edits, what its rows show and where a write goes
// (design/client-settings-profiles.md §5.1). The surface edits the global defaults or one preset's
// overrides with the same rows: a row shows the defaults with the preset's overrides on top, and a
// write lands in `UserDefaults` or in the preset's overlay. Pure; tested in `SettingsScopeTests`.

import Foundation

/// Which layer the settings surface is editing.
public enum SettingsScope: Equatable, Hashable, Sendable {
    /// The global defaults every preset inherits from.
    case defaults
    /// One preset's overrides, by id.
    case preset(String)

    public var presetID: String? {
        if case .preset(let id) = self { return id }
        return nil
    }

    /// The edited preset, while the catalog still holds it.
    public func preset(in catalog: PresetCatalog) -> StreamPreset? {
        presetID.flatMap { catalog.preset(id: $0) }
    }

    /// Where a row's write goes.
    public enum Route: Equatable, Sendable {
        /// The global, under this `UserDefaults` key.
        case defaults(String)
        /// An override in this preset.
        case override(String)
        /// The scope names a preset that left the catalog.
        case gone
    }

    /// What every row displays: `globals` with the edited preset's overrides on top, so a row the
    /// preset does not override reads as the live global.
    public static func resolve(
        globals: EffectiveSettings, scope: SettingsScope, catalog: PresetCatalog
    ) -> EffectiveSettings {
        guard let preset = scope.preset(in: catalog) else { return globals }
        return globals.applying(preset.overrides)
    }

    /// Where a write to `field` goes. A write always records: it never compares the value with
    /// the global, so a value equal to it is a pin the preset keeps when the global moves. A
    /// preset deleted elsewhere is `.gone`: the screen shows the globals, and the caller writes
    /// there and falls back to `.defaults`.
    public static func route<Value>(
        scope: SettingsScope, catalog: PresetCatalog, field: SettingsField<Value>
    ) -> Route {
        guard let id = scope.presetID else { return .defaults(field.key) }
        return catalog.preset(id: id) == nil ? .gone : .override(id)
    }
}

/// One presetable setting's three faces: the `UserDefaults` key its global lives under, the
/// overlay slot an override lives in, and the name a per-row reset carries. Together, so a row
/// can't write an override its reset can't find.
public struct SettingsField<Value> {
    /// The overlay's serialized field name, also the reset key. `resolution` is the one alias,
    /// covering the width/height/match-window tri-state a single control drives.
    public let name: String
    public let key: String
    public let overlay: WritableKeyPath<SettingsOverlay, Value?>
    public let effective: KeyPath<EffectiveSettings, Value>

    public init(
        name: String, key: String, overlay: WritableKeyPath<SettingsOverlay, Value?>,
        effective: KeyPath<EffectiveSettings, Value>
    ) {
        self.name = name
        self.key = key
        self.overlay = overlay
        self.effective = effective
    }
}
