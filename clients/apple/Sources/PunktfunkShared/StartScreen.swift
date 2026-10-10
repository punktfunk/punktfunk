// Where a bare launch opens: the host list, the default host's library, or its stream.
//
// The Swift half of the cross-client start-screen policy — `crates/client/pf-client-core/src/start.rs`
// is the other, and `clients/shared/start-screen-vectors.json` holds them to the same answers.
// Lives in the dependency-free shared module because a widget or an App Intent that wants to
// act on "your host" needs the same resolution the app boots with.

import Foundation

/// The `punktfunk.startIn` setting. Unknown reads as `.hosts`, the `libraryView` convention:
/// a value a newer client wrote degrades, it never ends a launch.
public enum StartIn: String, CaseIterable, Sendable {
    case hosts
    case library
    case stream

    /// Stored → case, with the unknown fallback. Use this rather than `init(rawValue:)`.
    public static func parse(_ raw: String?) -> StartIn {
        StartIn(rawValue: raw ?? "") ?? .hosts
    }

    public var stored: String { rawValue }

    /// Settings-row label. The value line names the resolved host beside it.
    public var label: String {
        switch self {
        case .hosts: return "Host list"
        case .library: return "Library"
        case .stream: return "Stream"
        }
    }
}

/// Where a bare launch opens, and which host it opens on.
public enum StartScreen: Equatable, Sendable {
    case hosts
    case library(StoredHost)
    /// The library plus one connect. One attempt: a refusal lands on the shelf underneath,
    /// and nothing retries.
    case stream(StoredHost)

    /// Where a resolved default came from, for the one log line a launch prints.
    public enum Source: String, Sendable {
        case explicit
        case derived
        case none
    }

    /// The host a bare launch opens on, and how it was chosen. The explicit id wins when it
    /// names a paired record; else the sole paired record; else nothing. Paired-only, because
    /// a launch cannot pair — an unpaired host would be a dead landing. A dangling id falls
    /// through to the derived rule, which is why forgetting a host needs no write hook.
    ///
    /// The id is compared case-insensitively: `UUID.uuidString` is uppercase here and the Rust
    /// client mints lowercase, and the two write the same key name.
    public static func defaultHost(
        id: String?, hosts: [StoredHost]
    ) -> (host: StoredHost?, source: Source) {
        let paired = hosts.filter { $0.pinnedSHA256 != nil }
        if let want = id?.lowercased(), !want.isEmpty,
            let match = paired.first(where: { $0.id.uuidString.lowercased() == want })
        {
            return (match, .explicit)
        }
        return paired.count == 1 ? (paired[0], .derived) : (nil, .none)
    }

    /// Where a bare launch opens. No default host degrades every `startIn` value to the list.
    public static func resolve(
        startIn: String?, defaultHost id: String?, hosts: [StoredHost]
    ) -> StartScreen {
        guard let host = defaultHost(id: id, hosts: hosts).host else { return .hosts }
        switch StartIn.parse(startIn) {
        case .hosts: return .hosts
        case .library: return .library(host)
        case .stream: return .stream(host)
        }
    }

    /// The host this landing opens on, if any — for the callers that only need the target.
    public var host: StoredHost? {
        switch self {
        case .hosts: return nil
        case .library(let h), .stream(let h): return h
        }
    }

    /// The saved-host store as the app last wrote it, in store order. For the readers that hold
    /// no `HostStore` — the settings footer, an App Intent, the widget. Store order, not recency:
    /// the derived rule counts paired records rather than picking a recent one.
    public static func savedHosts() -> [StoredHost] {
        StoredHost.loadAll(recentFirst: false)
    }
}
