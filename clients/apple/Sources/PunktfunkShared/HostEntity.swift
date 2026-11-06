// The saved-host as an App Intents entity — the parameter type for the Connect/Wake intents and
// the configurable single-host widget. Lives in the shared module (not the app) because widget
// *configuration* intents execute in the EXTENSION process, so the entity can't be app-only.
//
// AppIntents is genuinely available on macOS (13+), so this is gated on `canImport(AppIntents)`
// (unlike ActivityKit, whose macOS types are unavailable) — it compiles on every platform and the
// entity query reads the same shared App-Group store the widget does.

#if canImport(AppIntents)
import AppIntents
import Foundation

public struct HostEntity: AppEntity, Identifiable {
    public static let typeDisplayRepresentation = TypeDisplayRepresentation(
        name: LocalizedStringResource("apple_entity_host", defaultValue: "Host"))
    public static let defaultQuery = HostEntityQuery()

    public let id: UUID
    public let name: String

    public init(id: UUID, name: String) {
        self.id = id
        self.name = name
    }

    public init(_ host: StoredHost) {
        self.id = host.id
        self.name = host.displayName
    }

    public var displayRepresentation: DisplayRepresentation {
        DisplayRepresentation(title: "\(name)")
    }
}

public struct HostEntityQuery: EntityQuery {
    public init() {}

    public func entities(for identifiers: [UUID]) async throws -> [HostEntity] {
        Self.loadHosts().filter { identifiers.contains($0.id) }.map(HostEntity.init)
    }

    /// Sorted most-recent first — Siri/Shortcuts and the widget config picker suggest recent hosts.
    public func suggestedEntities() async throws -> [HostEntity] {
        Self.loadHosts().map(HostEntity.init)
    }

    static func loadHosts() -> [StoredHost] { StoredHost.loadAll(recentFirst: true) }
}
#endif
