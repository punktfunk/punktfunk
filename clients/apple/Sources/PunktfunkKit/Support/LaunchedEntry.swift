// The shelf tap behind a launch connect. The library remembers it, the connect flow peeks at it,
// and `SessionModel.connect` takes it to raise the launch hold (`LaunchedEntryTests`).

import CoreGraphics

/// The entry behind a `connect(launchID:)`, handed over out of band: the shelf's launch callbacks
/// carry only the id, and the launch hold needs the title, art and tile rect. Keyed by id on the
/// way out, so a stale entry can never dress a different launch.
public enum LaunchedEntry {
    private static var last: (entry: GameEntry, rect: CGRect?)?

    public static func remember(_ entry: GameEntry?, from rect: CGRect?) {
        last = entry.map { ($0, rect) }
    }

    /// The remembered entry for `id`, left for `take`.
    public static func peek(_ id: String) -> GameEntry? {
        last?.entry.id == id ? last?.entry : nil
    }

    /// The remembered entry for `id` and the rect of the tile it was tapped on. A take for
    /// another id leaves the entry for the connect it names.
    public static func take(_ id: String) -> (entry: GameEntry, rect: CGRect?)? {
        guard let taken = last, taken.entry.id == id else { return nil }
        last = nil
        return taken
    }
}
