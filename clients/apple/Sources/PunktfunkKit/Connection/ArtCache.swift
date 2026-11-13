// On-disk cache for library cover art.
//
// Posters are the bulk of what the library screen transfers and they essentially never change, so
// re-fetching them on every visit is pure waste — the Windows client has cached them on disk for
// this reason and Apple did not. It matters more now that host art rides `MgmtTransport`: a cache
// hit costs no connection at all.
//
// Lives in the CACHES directory on purpose: every byte here is re-derivable from the host, so the
// system is welcome to evict it under storage pressure. Entries are keyed by a caller-chosen
// string hashed to a filename — the loader keys host art by pin+path (a re-addressed host keeps
// its cache) and CDN art by URL.
//
// Deliberately free of any Network.framework / PunktfunkCore dependency, so it can be unit-tested
// against a temporary directory.

import CryptoKit
import Foundation

/// A size- and age-bounded blob cache. An actor so disk work stays off whichever thread the
/// SwiftUI poster view happens to be on, and so pruning can never race a write.
actor ArtCache {
    private let directory: URL
    private let maxBytes: Int
    private let maxAge: TimeInterval
    private let fileManager = FileManager.default
    /// Bytes written since the last prune. Pruning walks the whole directory and stats every
    /// entry, so doing it per store made a cold 500-title library quadratic — and every poster
    /// waits behind it on this actor.
    private var writtenSincePrune = 0

    /// `directory` is created on demand. Defaults: 128 MB — a 200-title library of 600×900
    /// capsules lands far under that — and 30 days, which only matters for art a host later
    /// replaces.
    init(directory: URL, maxBytes: Int = 128 * 1024 * 1024, maxAge: TimeInterval = 30 * 24 * 3600) {
        self.directory = directory
        self.maxBytes = maxBytes
        self.maxAge = maxAge
    }

    /// The one cache every loader shares. One instance per directory keeps the prune budget
    /// counting across library visits, and keeps prune from racing another instance's write.
    static let shared = standard()

    /// The app's standard location, or nil if the caches directory is unavailable (in which case
    /// callers simply run without a cache rather than failing).
    static func standard() -> ArtCache? {
        guard let caches = FileManager.default.urls(
            for: .cachesDirectory, in: .userDomainMask).first
        else { return nil }
        return ArtCache(directory: caches.appendingPathComponent("PunktfunkArt", isDirectory: true))
    }

    func data(forKey key: String) -> Data? {
        let file = path(forKey: key)
        // Stat before reading: an expired entry is deleted unseen rather than paying a full
        // read just to throw it away.
        guard let modified = modificationDate(of: file) else { return nil }
        // Age out stale art rather than serving it forever.
        if Date().timeIntervalSince(modified) > maxAge {
            try? fileManager.removeItem(at: file)
            return nil
        }
        guard let data = try? Data(contentsOf: file) else { return nil }
        // Touch, so eviction can order by last USE rather than last write.
        try? fileManager.setAttributes([.modificationDate: Date()], ofItemAtPath: file.path)
        return data
    }

    func store(_ data: Data, forKey key: String) {
        // An empty body is not art.
        guard !data.isEmpty else { return }
        do {
            try fileManager.createDirectory(at: directory, withIntermediateDirectories: true)
            try data.write(to: path(forKey: key), options: .atomic)
        } catch {
            return // a cache that can't write is a slow cache, not a broken app
        }
        // Amortised: prune once per `pruneInterval` of new bytes. The budget is a ceiling on
        // steady-state size, not a per-write invariant, so overshooting it between prunes by at
        // most one pruneInterval is the whole cost.
        writtenSincePrune += data.count
        if writtenSincePrune >= pruneInterval {
            writtenSincePrune = 0
            prune()
        }
    }

    /// New bytes that earn a full directory walk. An eighth of the budget: eight walks to turn
    /// the cache over, against one per poster.
    private var pruneInterval: Int { max(1, maxBytes / 8) }

    /// Drop the oldest entries until the directory fits the budget. Also removes anything past
    /// `maxAge` so a cache that is under budget still doesn't hoard stale art forever.
    func prune() {
        let keys: [URLResourceKey] = [.contentModificationDateKey, .fileSizeKey]
        guard let entries = try? fileManager.contentsOfDirectory(
            at: directory, includingPropertiesForKeys: keys, options: .skipsHiddenFiles)
        else { return }

        var files: [(url: URL, date: Date, size: Int)] = []
        var total = 0
        let now = Date()
        for entry in entries {
            let values = try? entry.resourceValues(forKeys: Set(keys))
            let date = values?.contentModificationDate ?? .distantPast
            let size = values?.fileSize ?? 0
            if now.timeIntervalSince(date) > maxAge {
                try? fileManager.removeItem(at: entry)
                continue
            }
            files.append((entry, date, size))
            total += size
        }
        guard total > maxBytes else { return }

        // Oldest first — `data(forKey:)` touches on read, so this is least-recently-USED.
        for file in files.sorted(by: { $0.date < $1.date }) {
            guard total > maxBytes else { break }
            try? fileManager.removeItem(at: file.url)
            total -= file.size
        }
    }

    private func path(forKey key: String) -> URL {
        let digest = SHA256.hash(data: Data(key.utf8))
        let name = digest.map { String(format: "%02x", $0) }.joined()
        return directory.appendingPathComponent(name, isDirectory: false)
    }

    private func modificationDate(of file: URL) -> Date? {
        (try? file.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate
    }
}
