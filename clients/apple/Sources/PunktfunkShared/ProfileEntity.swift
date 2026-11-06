// A settings preset as an App Intents entity — the parameter type for "Stream <host> with
// <preset>" in Shortcuts (design/client-deep-links.md §6). Lives beside `HostEntity` in the
// shared module for the same reason: an intent (and, later, a configurable widget) executes
// outside the app, so the entity and its query must not need PunktfunkKit.
//
// The query reads the App-Group catalog blob directly — no Rust core, no app process — exactly
// like `HostEntityQuery` reads the saved hosts.

#if canImport(AppIntents)
import AppIntents
import Foundation

/// Keeps its name: Shortcuts stores the type name in every saved action that uses a preset.
public struct ProfileEntity: AppEntity, Identifiable {
    public static let typeDisplayRepresentation = TypeDisplayRepresentation(
        name: LocalizedStringResource("apple_entity_preset", defaultValue: "Settings Preset"))
    public static let defaultQuery = ProfileEntityQuery()

    /// The catalog id — stable across renames, which is why a shortcut keeps working after the
    /// user renames the preset it points at.
    public let id: String
    public let name: String
    /// `#RRGGBB`, when the preset has been given one. Carried so a future display representation
    /// (and the configurable widget) can tint without a second lookup.
    public let accent: String?

    public init(id: String, name: String, accent: String? = nil) {
        self.id = id
        self.name = name
        self.accent = accent
    }

    public init(_ preset: StreamPreset) {
        self.init(id: preset.id, name: preset.name, accent: preset.accent)
    }

    public var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(name)")
    }
}

public struct ProfileEntityQuery: EntityQuery {
    public init() {}

    public func entities(for identifiers: [String]) async throws -> [ProfileEntity] {
        PresetCatalog.load().presets
            .filter { identifiers.contains($0.id) }
            .map(ProfileEntity.init)
    }

    public func suggestedEntities() async throws -> [ProfileEntity] {
        PresetCatalog.load().presets.map(ProfileEntity.init)
    }
}
#endif
