// Titles marked as favorites, per host, on this device (design/apple-touch-ui-overhaul.md §2.5).
// Favorites belong to a person, and no host profile stores them, so this device is the nearest
// thing to one. One UserDefaults key per host, the `LibraryScrollMemory` pattern; removing a host
// forgets them.

import Foundation

@MainActor
final class LibraryFavorites: ObservableObject {
    static let shared = LibraryFavorites()

    /// Lists changed on this run. A host never toggled here reads straight from defaults, so a
    /// view's body never has to publish to learn one.
    @Published private var changed: [String: [String]] = [:]

    private static func key(_ hostID: String) -> String {
        "punktfunk.library.favorites.\(hostID)"
    }

    /// Favorite title ids on `hostID`'s library, in the order they were marked.
    func ids(for hostID: String) -> [String] {
        changed[hostID] ?? UserDefaults.standard.stringArray(forKey: Self.key(hostID)) ?? []
    }

    func contains(_ gameID: String, host hostID: String) -> Bool {
        ids(for: hostID).contains(gameID)
    }

    func toggle(_ gameID: String, host hostID: String) {
        var list = ids(for: hostID)
        if let at = list.firstIndex(of: gameID) {
            list.remove(at: at)
        } else {
            list.append(gameID)
        }
        changed[hostID] = list
        UserDefaults.standard.set(list, forKey: Self.key(hostID))
    }

    /// Replace `hostID`'s list whole: the console's settings document carries it.
    func set(_ ids: [String], host hostID: String) {
        guard ids != self.ids(for: hostID) else { return }
        changed[hostID] = ids
        UserDefaults.standard.set(ids, forKey: Self.key(hostID))
    }

    /// Part of removing a host: nothing it held stays behind on the device.
    func forget(hostID: String) {
        changed[hostID] = nil
        UserDefaults.standard.removeObject(forKey: Self.key(hostID))
    }
}
