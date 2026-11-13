// On-disk cache for a host's library CATALOG — the list of titles, not their art.
//
// Cover art has been cached since `ArtCache`, but the catalog behind it never was: every visit to a
// library refetched `GET /api/v1/library` and showed nothing until that call returned. A host that
// is asleep, or simply not reachable yet, therefore had an EMPTY library — which is the opposite of
// what a player wants from the screen they use to decide what to play, and it makes waking a host
// on library entry pointless: there would be nothing to look at while it boots.
//
// So the catalog is cached per host and rendered immediately, marked stale, and reconciled when the
// host answers. Field request, 2026-08-16: "it would add a nice seamless experience if offline
// library would be added".
//
// Caches directory, like `ArtCache`, for the same reason: every byte is re-derivable from the host,
// so the system is welcome to evict it. Unlike art, a catalog is small (a few hundred KB for a big
// library), so there is no size budget here — one file per host and per profile it was fetched
// as, replaced wholesale.
//
// Deliberately free of any Network.framework / PunktfunkCore dependency, so it can be unit-tested
// against a temporary directory.

import CryptoKit
import Foundation

/// A host's library as last seen, with when that was.
public struct CachedLibrary: Codable, Sendable {
    public var games: [GameEntry]
    public var fetchedAt: Date

    public init(games: [GameEntry], fetchedAt: Date) {
        self.games = games
        self.fetchedAt = fetchedAt
    }

    /// How old this snapshot is. The UI uses it to word the staleness note, never to decide whether
    /// to show the catalog: a year-old library is still a far better answer than an empty one, and
    /// the live fetch is always in flight behind it anyway.
    public var age: TimeInterval { Date().timeIntervalSince(fetchedAt) }
}

/// Per-host catalog storage. An actor so disk work stays off the SwiftUI thread and a read can
/// never race the write that follows a fetch.
public actor LibraryCache {
    private let directory: URL
    private let fileManager = FileManager.default

    public init(directory: URL) {
        self.directory = directory
    }

    /// The app's standard location, or nil if the caches directory is unavailable — in which case
    /// callers simply run without a cache rather than failing.
    public static func standard() -> LibraryCache? {
        guard let caches = FileManager.default.urls(
            for: .cachesDirectory, in: .userDomainMask).first
        else { return nil }
        return LibraryCache(
            directory: caches.appendingPathComponent("PunktfunkLibrary", isDirectory: true))
    }

    /// The shared instance, or nil where there is no caches directory.
    public static let shared = standard()

    /// The catalog fetched as `profile` (nil: the box's), so a shelf never shows another
    /// profile's titles.
    public func load(hostID: String, profile: String?) -> CachedLibrary? {
        guard let data = try? Data(contentsOf: path(for: hostID, profile: profile)) else {
            return nil
        }
        // A catalog written by an older build whose `GameEntry` had different fields decodes to
        // nothing rather than failing: a miss costs one fetch, which is what would have happened
        // anyway. Never surfaced as an error.
        return try? JSONDecoder().decode(CachedLibrary.self, from: data)
    }

    public func store(_ games: [GameEntry], hostID: String, profile: String?) {
        // An empty catalog is not worth remembering: it is indistinguishable from "never fetched"
        // when read back, and caching it would pin a blank library over a host that has titles.
        guard !games.isEmpty else { return }
        write(CachedLibrary(games: games, fetchedAt: Date()), to: path(for: hostID, profile: profile))
    }

    /// The catalog cached for `profile` once title `id` left it (`back` nil) or came back as
    /// `back`, keeping its age. No cache, nothing written; the last title gone, no cache.
    public func noteHidden(_ id: String, back: GameEntry?, hostID: String, profile: String?) {
        guard var cached = load(hostID: hostID, profile: profile) else { return }
        cached.games.removeAll { $0.id == id }
        if let back { cached.games.append(back) }
        let file = path(for: hostID, profile: profile)
        guard !cached.games.isEmpty else {
            try? fileManager.removeItem(at: file)
            return
        }
        write(cached, to: file)
    }

    private func write(_ snapshot: CachedLibrary, to file: URL) {
        guard let data = try? JSONEncoder().encode(snapshot) else { return }
        do {
            try fileManager.createDirectory(
                at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
            try data.write(to: file, options: .atomic)
        } catch {
            return // a cache that can't write is a slower app, not a broken one
        }
    }

    /// Drop a host's catalogs, every profile's — used when its identity is forgotten, so a
    /// removed host leaves no list of what somebody plays behind on disk.
    public func forget(hostID: String) {
        try? fileManager.removeItem(at: path(for: hostID, profile: nil))
        try? fileManager.removeItem(at: directory.appendingPathComponent(Self.hashed(hostID)))
    }

    /// `<host>.json` for the box, the name a catalog had before profiles, else
    /// `<host>/<profile>.json`. Hashed rather than used verbatim: a host id is user-controlled
    /// text and must never be able to reach out of this directory (`../`) or exceed a filename
    /// length limit.
    func path(for hostID: String, profile: String?) -> URL {
        let host = Self.hashed(hostID)
        guard let profile else { return directory.appendingPathComponent("\(host).json") }
        return directory.appendingPathComponent(host, isDirectory: true)
            .appendingPathComponent("\(Self.hashed(profile)).json")
    }

    private static func hashed(_ text: String) -> String {
        SHA256.hash(data: Data(text.utf8)).map { String(format: "%02x", $0) }.joined()
    }
}
