// Game library client. Fetches the host's unified game library from the management REST API
// (`GET /api/v1/library`) — the same payload the web console's /library page renders — and what it
// currently has running (`GET /api/v1/status`), so a title the player can return to can be marked
// as such. Offered for a PAIRED host only: the fetch authenticates with the pinned identity.
//
// The management API serves HTTPS on a port distinct from the punktfunk/1 data plane (default
// 47990, also advertised in the host's mDNS `mgmt` TXT). A paired client is authorized for the
// read-only library route by its **mTLS certificate** — no bearer token. The host binds this read
// surface to the LAN by DEFAULT (the bearer-gated admin surface stays loopback-only), so a paired
// client browses a host's library with no operator step. This mirrors the GameEntry/Artwork/
// LaunchSpec schema in `crates/punktfunk-host/src/library.rs`.

import Foundation
// `punktfunkDefaultMgmtPort` (and StoredHost/DefaultsKey) now live in PunktfunkShared so the
// dependency-free widget extension can share them; PunktfunkKit re-exports the module.
import PunktfunkShared

/// Cover art URLs (the public Steam CDN for Steam titles, user-supplied for custom entries).
public struct Artwork: Codable, Hashable, Sendable {
    public var portrait: String?
    public var hero: String?
    public var logo: String?
    public var header: String?

    /// Preferred order for a poster grid: the 600×900 capsule, falling back to the header
    /// (which is near-universal — many older titles lack a portrait capsule).
    public var posterCandidates: [URL] {
        [portrait, header, hero].compactMap { $0 }.compactMap { URL(string: $0) }
    }
}

/// How the host would launch a title (carried for a later step; the client only displays it).
public struct LaunchSpec: Codable, Hashable, Sendable {
    public var kind: String // "steam_appid" | "command"
    public var value: String
}

/// One title's play numbers as the host keeps them (`GameEntry.stats`).
public struct GameStats: Codable, Hashable, Sendable {
    /// Unix ms of the last launch.
    public var lastPlayedUnixMs: UInt64
    /// Every run added up, ms.
    public var playTimeMs: UInt64
    /// The run that started at `lastPlayedUnixMs`, ms. Still growing while it runs.
    public var lastRunMs: UInt64
    public var launchCount: UInt32

    private enum CodingKeys: String, CodingKey {
        case lastPlayedUnixMs = "last_played_unix_ms"
        case playTimeMs = "play_time_ms"
        case lastRunMs = "last_run_ms"
        case launchCount = "launch_count"
    }

    /// A missing or mistyped number reads as zero: numbers are never worth an empty library.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        lastPlayedUnixMs = (try? c.decodeIfPresent(UInt64.self, forKey: .lastPlayedUnixMs)) ?? 0
        playTimeMs = (try? c.decodeIfPresent(UInt64.self, forKey: .playTimeMs)) ?? 0
        lastRunMs = (try? c.decodeIfPresent(UInt64.self, forKey: .lastRunMs)) ?? 0
        launchCount = (try? c.decodeIfPresent(UInt32.self, forKey: .launchCount)) ?? 0
    }
}

/// One title in the unified library. `id` is store-qualified: `steam:<appid>` / `custom:<id>`.
public struct GameEntry: Codable, Hashable, Identifiable, Sendable {
    public var id: String
    public var store: String // "steam" | "custom" | "lutris" | "heroic" | "epic" | "gog" | "xbox"
    public var title: String
    public var art: Artwork
    public var launch: LaunchSpec?
    /// `"game"` (the default, and what an older host omits) or `"launcher"` — an entry that opens
    /// the launcher itself (Steam Big Picture, Heroic) rather than a title. Deliberately a plain
    /// optional String: the host owns the vocabulary, and an unknown future value must never fail
    /// the whole library decode. Anything that isn't `"launcher"` is a game (design D4).
    public var role: String?
    /// The token for this entry's brand mark (`"steam"`, `"heroic"`) — never art, never a URL.
    /// `nil` on every older host and on every ordinary title. See `launcherIconImage`.
    public var icon: String?
    /// The platform the host filed this title under ("PC", "PS3", "SNES" — free-form, the host's
    /// `GameMeta.platform`, populated by the rom-manager plugin and left `nil` by the store
    /// scanners). What the library's collections group by; a `nil` buckets under the STORE, never
    /// under "Unknown" (see `LibraryCollation.bucket`). Also on the detail band as
    /// `STORE · PLATFORM`.
    public var platform: String?
    /// The rest of the host's `GameMeta`, as far as anything here shows it. The host has sent
    /// these since the library API existed and no client ever read them; the launch hold is the
    /// first screen with room to say more than a title, so it decodes what it can use.
    ///
    /// Optional rather than defaulted: a synthesized `Decodable` throws on a missing key for a
    /// non-optional property, and the host omits every one of these when it has nothing.
    public var developer: String?
    public var publisher: String?
    public var releaseYear: Int?
    public var genres: [String]?
    /// A short blurb, plain text.
    public var description: String?
    public var tags: [String]?
    /// Maximum simultaneous local players.
    public var players: Int?
    /// Host play stats; `nil` until the host has launched the title once.
    public var stats: GameStats?
    /// A title a plugin installs; `nil` is installed, as every other title is.
    public var install: TitleInstall?

    private enum CodingKeys: String, CodingKey {
        case id, store, title, art, launch, role, icon, platform
        case developer, publisher, genres, description, tags, players, stats, install
        case releaseYear = "release_year"
    }

    public var isCustom: Bool { store == "custom" }

    /// Whether this entry opens a launcher rather than a game.
    public var isLauncher: Bool { role == "launcher" }

    /// The brand-icon token, re-validated rather than taken on trust.
    ///
    /// The host checks the shape on the way in, so this can only fire for a host older than that
    /// check or one that isn't ours. The value reaches `Image(named:)`, and "the peer promised" is
    /// not the standard a name lookup deserves.
    public var iconToken: String? {
        guard let t = icon, !t.isEmpty, t.count <= 32,
              let first = t.first, first.isASCII, first.isLowercase,
              t.allSatisfy({ $0.isASCII && ($0.isLowercase || $0.isNumber || $0 == "-") })
        else { return nil }
        return t
    }

    /// Display name for the store badge — the same table the Rust clients use
    /// (`pf-console-ui::library::store_label`). Before this existed the badge said "Steam" for
    /// every non-custom entry, which a Lutris or GOG title made a lie.
    public var storeLabel: String {
        switch store {
        case "steam": return "Steam"
        case "custom": return "Custom"
        case "heroic": return "Heroic"
        case "lutris": return "Lutris"
        case "epic": return "Epic"
        case "gog": return "GOG"
        case "xbox": return "Xbox"
        default: return "Game"
        }
    }
}

/// A title's files on the host, from the library entry's `install`.
public struct TitleInstall: Codable, Hashable, Sendable {
    /// `installed` or `missing`; a word this build doesn't know reads as installed.
    public var state: String
    /// The download size while missing, the size on disk once installed.
    public var sizeBytes: UInt64?
    /// Free space where the download goes.
    public var freeBytes: UInt64?

    public init(state: String, sizeBytes: UInt64? = nil, freeBytes: UInt64? = nil) {
        self.state = state
        self.sizeBytes = sizeBytes
        self.freeBytes = freeBytes
    }

    private enum CodingKeys: String, CodingKey {
        case state
        case sizeBytes = "size_bytes"
        case freeBytes = "free_bytes"
    }

    public var missing: Bool { state == "missing" }
}

/// What a title menu offers for a title's files; at most one applies. The rules and words are the
/// Rust client's (`pf_client_core::library::InstallAction`).
public enum InstallAction: String, Sendable {
    case install = "Install", pause = "Pause", resume = "Resume", remove = "Remove"

    /// The one row, if any. `nil` grants (an older host) allow starting a download only.
    public static func forTitle(
        _ install: TitleInstall?, download: HostDownload?, grants: UInt32?
    ) -> InstallAction? {
        guard let install else { return nil }
        let launch = grants.map { $0 & PunktfunkConnection.grantLaunch != 0 } ?? true
        let manage = grants.map { $0 & PunktfunkConnection.grantManageGames != 0 } ?? false
        let action: InstallAction
        switch download?.state {
        case "queued", "downloading": action = .pause
        case "installing": return nil
        case "paused": action = .resume
        case "done": action = .remove
        default: action = install.missing ? .install : .remove
        }
        let allowed = action == .install || action == .resume ? launch : manage
        return allowed ? action : nil
    }

    /// The menu row: `Install · 26 GB (212 GB free)`, `Remove download · 26 GB`.
    public func label(_ install: TitleInstall?) -> String {
        let size = install?.sizeBytes.map(HostDownload.bytes)
        switch self {
        case .install:
            switch (size, install?.freeBytes.map(HostDownload.bytes)) {
            case let (size?, free?): return "Install \u{b7} \(size) (\(free) free)"
            case let (size?, nil): return "Install \u{b7} \(size)"
            default: return "Install"
            }
        case .pause: return "Pause download"
        case .resume: return "Resume download"
        case .remove: return size.map { "Remove download \u{b7} \($0)" } ?? "Remove download"
        }
    }

    var verb: String { rawValue.lowercased() }
}

/// A tile's mark for a title's files; an installed title has none. Same words as the Rust tiles.
public struct TileBadge: Equatable, Sendable {
    /// `download`, `pause` or `alert`.
    public var icon: String
    /// `42 %`, `26 GB`, `Not installed`, `Queued`.
    public var text: String

    public static func forTitle(_ install: TitleInstall?, download: HostDownload?) -> TileBadge? {
        let pct = { (d: HostDownload) in
            d.fraction.map { "\(Int(($0 * 100).rounded(.down))) %" } ?? "Queued"
        }
        if let d = download, d.live { return TileBadge(icon: "download", text: pct(d)) }
        if let d = download, d.state == "paused" { return TileBadge(icon: "pause", text: pct(d)) }
        if download?.state == "failed" { return TileBadge(icon: "alert", text: "Not installed") }
        guard let install, install.missing else { return nil }
        return TileBadge(
            icon: "download", text: install.sizeBytes.map(HostDownload.bytes) ?? "Not installed")
    }
}

/// What asking the host to change a title's files came to. The Rust client's `InstallOutcome`.
public enum InstallOutcome: Equatable, Sendable {
    case done
    /// 403/404/409 with the host's own sentence.
    case refused(String)
    /// A host without the route.
    case unsupported
    case failed(String)

    /// An answer: the host's sentence when it sent one, else the route is missing.
    public static func from(status: Int, message: String?) -> InstallOutcome {
        if (200..<300).contains(status) { return .done }
        if let message, !message.isEmpty { return .refused(message) }
        if [401, 404, 405].contains(status) { return .unsupported }
        return .failed("the host refused it (\(status))")
    }

    /// The player-facing line.
    public func notice(_ action: InstallAction, title: String) -> String {
        switch self {
        case .done:
            switch action {
            case .install, .resume: return "Downloading \(title)."
            case .pause: return "Paused \(title)'s download."
            case .remove: return "Removed \(title). Saves stay on the host."
            }
        case .refused(let why): return why
        case .unsupported: return "This host needs an update to manage games from here."
        case .failed(let why): return "Couldn't \(action.verb) \(title) \u{2014} \(why)"
        }
    }
}

public extension Array where Element == GameEntry {
    /// Design D4: launcher entries lead the shelf, and the host's title order survives within each
    /// group. Applied once where the library is fetched, so no individual view has to remember
    /// the rule — and a library without launcher entries comes back untouched.
    var launchersFirst: [GameEntry] {
        let launchers = filter(\.isLauncher)
        return launchers.isEmpty ? self : launchers + filter { !$0.isLauncher }
    }
}

/// Errors surfaced to the UI so it can guide setup (the common case is "not paired yet").
public enum LibraryError: LocalizedError {
    case unauthorized
    /// The host's certificate didn't hash to the fingerprint pinned at pairing — an impostor, or
    /// a host reinstalled/re-keyed since. Distinct from `unreachable` because the remedy is
    /// completely different: re-pair, don't go hunting the network.
    case pinMismatch
    case http(Int)
    case unreachable(String)
    /// A library entry's art URL is not something we will load (only http/https, or inline base64).
    case badArtURL

    /// A phrase, never a sentence. Every caller supplies the frame — the library
    /// screen's "Couldn't load the library — ", `SendLogs`' "Couldn't send logs — ",
    /// `HostPower`'s "\(label) failed — ". A sentence here reads as a second headline.
    public var errorDescription: String? {
        switch self {
        case .unauthorized:
            return "the host doesn't recognize this device — pair with it first"
        case .pinMismatch:
            return "the host's certificate isn't the one you paired with — pair again"
        case .http(let code):
            return "the host refused it (\(code))"
        case .badArtURL:
            return "that title's artwork isn't a web address or an inline image"
        case .unreachable(let why):
            return "couldn't reach the host — \(why)"
        }
    }
}

/// One game the host currently has launched, from `/api/v1/status`.
///
/// A deliberately partial mirror of the host's `ActiveGame`: only the fields a client can act on.
/// The console's own view of this payload carries more (which session, which plane, the grace
/// countdown), and none of that is a player's business from the library screen.
/// One title's download, from `GET /api/v1/status` `downloads[]`. Its words match the Rust and
/// Kotlin shells', so a report quotes one line whichever client it came from.
public struct HostDownload: Codable, Hashable, Sendable {
    public var appID: String
    /// `queued` | `downloading` | `paused` | `installing` | `done` | `failed` | `cancelled`.
    public var state: String
    public var doneBytes: UInt64
    public var totalBytes: UInt64?
    public var rateBps: UInt64?
    public var etaS: UInt64?
    /// The plugin's words: `File 2 of 3`, `Verifying`.
    public var phase: String?
    public var error: String?

    public init(
        appID: String, state: String, doneBytes: UInt64 = 0, totalBytes: UInt64? = nil,
        rateBps: UInt64? = nil, etaS: UInt64? = nil, phase: String? = nil, error: String? = nil
    ) {
        self.appID = appID
        self.state = state
        self.doneBytes = doneBytes
        self.totalBytes = totalBytes
        self.rateBps = rateBps
        self.etaS = etaS
        self.phase = phase
        self.error = error
    }

    private enum CodingKeys: String, CodingKey {
        case appID = "app_id"
        case state
        case doneBytes = "done_bytes"
        case totalBytes = "total_bytes"
        case rateBps = "rate_bps"
        case etaS = "eta_s"
        case phase, error
    }

    /// Making progress, or expected to: a launch waits on it.
    public var live: Bool { state == "queued" || state == "downloading" || state == "installing" }

    /// 0–1 of the total, when the total is known.
    public var fraction: Double? {
        guard let total = totalBytes, total > 0 else { return nil }
        return min(1, Double(doneBytes) / Double(total))
    }

    /// `12.3 GB of 26 GB · 48 MB/s · about 4 min left`, `Installing…`.
    public var line: String {
        switch state {
        case "queued": return "Waiting for its turn to download\u{2026}"
        case "installing": return phase ?? "Installing\u{2026}"
        default: break
        }
        var parts = [
            totalBytes.flatMap { $0 > 0 ? $0 : nil }.map {
                "\(HostDownload.bytes(doneBytes)) of \(HostDownload.bytes($0))"
            } ?? "\(HostDownload.bytes(doneBytes)) so far",
        ]
        if state == "downloading" {
            if let rate = rateBps { parts.append("\(HostDownload.bytes(rate))/s") }
            if let eta = etaS {
                parts.append(
                    eta < 60 ? "less than a minute left"
                        : eta < 3600 ? "about \((eta + 30) / 60) min left"
                        : "about \(eta / 3600) h \((eta % 3600) / 60) min left")
            }
        }
        return parts.joined(separator: " \u{b7} ")
    }

    /// Why a launch that waited on it didn't start the title; nil while it may still.
    public func stopped(title: String) -> String? {
        switch state {
        case "failed": return error.map { "\(title) didn't download \u{2014} \($0)" } ?? "\(title) didn't download."
        case "cancelled": return "\(title)'s download was cancelled."
        case "paused": return "\(title)'s download was paused. Start it again to resume."
        default: return nil
        }
    }

    /// Decimal units, as stores count: `12.3 GB`, `48 MB`, `512 kB`.
    public static func bytes(_ n: UInt64) -> String {
        let v = Double(n)
        let (value, unit) = v >= 1e9 ? (v / 1e9, "GB") : v >= 1e6 ? (v / 1e6, "MB") : (v / 1e3, "kB")
        let rounded = (value * 10).rounded() / 10
        if value >= 100 || rounded.truncatingRemainder(dividingBy: 1) == 0 {
            return "\(Int(value.rounded())) \(unit)"
        }
        return String(format: "%.1f %@", rounded, unit)
    }
}

public struct RunningGame: Codable, Hashable, Sendable {
    /// Store-qualified library id (`steam:570`) — the key that lines this up with a `GameEntry`.
    /// Absent for an operator-typed GameStream command, which has no catalog entry behind it.
    public var appID: String?
    public var title: String
    /// `launching` | `running` | `window` | `exited` | `untracked` | `grace` | `detached`. A plain
    /// String on purpose: the host owns the vocabulary and adds to it, so an unknown value must
    /// never fail the decode of the whole list.
    public var state: String
    /// `running`, and the host will report `window` once the game's window is up. Absent from a
    /// host that cannot see windows, or predates them.
    public var awaitingWindow: Bool?
    /// The live session streaming it; absent for a game nobody streams.
    public var sessionID: UInt64?
    /// This device may end it (``LibraryClient/endGame(appID:address:port:certPEM:keyPEM:hostFingerprint:)``):
    /// a game it launched. Absent from a host that predates the field.
    public var endable: Bool?

    private enum CodingKeys: String, CodingKey {
        case appID = "app_id"
        case title
        case state
        case awaitingWindow = "awaiting_window"
        case sessionID = "session_id"
        case endable
    }

    /// A game this device launched that a live session streams: what an in-stream End Game ends.
    public var streamedHere: Bool { endable == true && sessionID != nil && appID != nil }

    /// Is this title *up on the host right now* — i.e. would picking it take the player back into
    /// it rather than start it?
    ///
    /// `untracked` counts: the host cannot follow that process, but it did launch it and has no
    /// evidence it stopped. `grace` counts too — its session is gone but the game is still running,
    /// which is precisely the case where getting back in promptly matters most. Only a confirmed
    /// `exited` does not.
    public var isUp: Bool { state != "exited" }
}

/// What asking the host to end a game came to (`POST /api/v1/game/end`). The Rust client's
/// `GameEnd`, words included.
public enum GameEndOutcome: Equatable, Sendable {
    case ended
    /// 409: the host had nothing of this title left to end.
    case notRunning
    /// 401/404: a host that predates ending games from a device.
    case unsupported
    /// 403: this device's access to the host expired.
    case expired
    case failed(String)

    public static func from(status: Int) -> GameEndOutcome {
        switch status {
        case 200..<300: return .ended
        case 409: return .notRunning
        case 401, 404: return .unsupported
        case 403: return .expired
        default: return .failed("the host refused it (\(status))")
        }
    }

    /// The game is gone, so a stream that was playing it can end.
    public var gameGone: Bool { self == .ended || self == .notRunning }

    /// The player-facing line. The Rust, Kotlin and web clients use the same words.
    public func notice(title: String) -> String {
        switch self {
        case .ended: return "Ended \(title)."
        case .notRunning: return "\(title) isn't running any more."
        case .unsupported: return "This host needs an update to end games from here."
        case .expired: return "This device's access to the host has expired."
        case .failed(let why): return "Couldn't end \(title) \u{2014} \(why)"
        }
    }
}

/// One action a host offers THIS device, from `/api/v1/actions` (`design/host-actions.md` §3.2)
/// — v1: sleep, restart, shut down.
///
/// Unknown ids are expected and fine: the client renders ``title`` verbatim for anything it has
/// no local wording for, which is what lets a later host add actions with no client release.
public struct HostAction: Codable, Hashable, Sendable, Identifiable {
    /// Stable id: `power.sleep`, `power.reboot`, `power.shutdown` today.
    public var id: String
    /// The host's own display title — the fallback label for an id this client doesn't know.
    public var title: String
    /// Action group (`power` for the built-ins).
    public var group: String
    /// Confirm before running: the action loses whatever is on that machine (restart, shut down).
    public var danger: Bool
    /// Whether the host can run it right now (a machine that cannot suspend, a foreign inhibitor).
    public var available: Bool
    /// Why not, when it can't — shown rather than hidden, so "greyed out" always has a reason.
    public var unavailableReason: String?
    /// Whether THIS device's access covers it (the host's Host-power grant). The client only ever
    /// keeps the permitted ones.
    public var permitted: Bool

    /// A hand-made action — the quick-actions editor previews the three power slots without a
    /// host on the line.
    public init(id: String, title: String, group: String = "power", danger: Bool = false,
                available: Bool = true, unavailableReason: String? = nil, permitted: Bool = true) {
        self.id = id
        self.title = title
        self.group = group
        self.danger = danger
        self.available = available
        self.unavailableReason = unavailableReason
        self.permitted = permitted
    }

    private enum CodingKeys: String, CodingKey {
        case id, title, group, danger, available, permitted
        case unavailableReason = "unavailable_reason"
    }

    /// This client's wording for a known id, else the host's own title — so a familiar action is
    /// worded the way the rest of this app words it, without hiding an unfamiliar one.
    public var label: String {
        switch id {
        case "power.sleep": return "Sleep Host"
        case "power.reboot": return "Restart Host"
        case "power.shutdown": return "Shut Down Host"
        default: return title
        }
    }
}

/// Stateless fetcher for a host's library.
public enum LibraryClient {
    /// One answer of `GET /api/v1/library/page`. `total` and `platforms` stay undecoded: the
    /// library screens collate the whole catalog themselves.
    struct LibraryPage: Decodable {
        var items: [GameEntry]
        var nextCursor: String?

        private enum CodingKeys: String, CodingKey {
            case items
            case nextCursor = "next_cursor"
        }
    }

    /// Titles a request: the host's ceiling for one page.
    static let pageLimit = 200

    /// 500 pages of 200 is 100 000 titles. A host whose cursor never runs out stops here.
    static let maxPages = 500

    /// The request path of one page. The cursor is the host's own text, so it is encoded.
    static func pagePath(cursor: String?) -> String {
        let plain = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-._~"))
        let next = cursor?.addingPercentEncoding(withAllowedCharacters: plain)
        return "/api/v1/library/page?limit=\(pageLimit)" + (next.map { "&cursor=\($0)" } ?? "")
    }

    /// The whole catalog, a page at a time, so no answer grows with the library. `get` takes
    /// the cursor of the page before and answers one page's body. Any page failing fails the
    /// walk: half a catalog is not one.
    static func walkPages(_ get: (String?) async throws -> Data) async throws -> [GameEntry] {
        var games: [GameEntry] = []
        var cursor: String?
        for _ in 0..<maxPages {
            let page = try JSONDecoder().decode(LibraryPage.self, from: try await get(cursor))
            games += page.items
            // A cursor that does not move would ask for the same page forever.
            guard let next = page.nextCursor, !next.isEmpty, next != cursor else { break }
            cursor = next
        }
        return games
    }

    /// The host's catalog, walked by `GET /api/v1/library/page` and authenticated by **mTLS**:
    /// the client presents its paired cert/key PEM and the host's self-signed cert is pinned by
    /// `hostFingerprint` (SHA-256 of its DER). A host older than the paged route refuses it on
    /// this lane, so `GET /api/v1/library` answers whole instead.
    /// `hostFingerprint == nil` throws `unauthorized`: an unpaired host is never trusted.
    public static func fetch(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data?
    ) async throws -> [GameEntry] {
        guard let base = URL(string: "\(baseURL(address: address, port: port))/api/v1/library")
        else { throw LibraryError.unreachable("invalid host address") }
        let identity = try clientIdentity(certPEM: certPEM, keyPEM: keyPEM)
        let body: (String) async throws -> Data = { path in
            let response = try await send(
                path: path, address: address, port: port,
                identity: identity, hostFingerprint: hostFingerprint)
            switch response.status {
            case 200:
                return response.body
            // Both are the host declining this certificate, with the same remedy.
            case 401, 403:
                throw LibraryError.unauthorized
            default:
                throw LibraryError.http(response.status)
            }
        }
        var games: [GameEntry]
        do {
            games = try await walkPages { cursor in try await body(pagePath(cursor: cursor)) }
        } catch LibraryError.unauthorized, LibraryError.http(404) {
            games = try JSONDecoder().decode(
                [GameEntry].self, from: try await body("/api/v1/library"))
        }
        // Art the host serves arrives as host-relative proxy paths (`/api/v1/library/art/...`).
        // Resolve them against THIS host, so every consumer sees absolute URLs.
        for i in games.indices {
            games[i].art = games[i].art.resolved(against: base)
        }
        return games
    }

    /// What the host currently has running, from `GET /api/v1/status`.
    ///
    /// Same lane, same identity, no new host work: `/status` is already on the paired-certificate
    /// allowlist (the host's `mgmt::auth::cert_may_access`) alongside `/library`, and has carried a
    /// `games[]` array since the session⇄game lifetime work. The client simply never read it — so a
    /// player had no way to see, from the device they browse on, that something was already up.
    ///
    /// Best-effort by contract: an older host, an unreachable one, or a shape we don't recognize
    /// yields an empty list rather than an error. Nothing here is worth failing a library screen
    /// over — the worst case is a Resume badge that doesn't appear.
    public static func running(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data?
    ) async -> [RunningGame] {
        await status(
            address: address, port: port, certPEM: certPEM, keyPEM: keyPEM,
            hostFingerprint: hostFingerprint
        ).games
    }

    /// `GET /api/v1/status`: the launched titles and the host's downloads, kept apart because a
    /// launch the host declined over its download has no game row left to carry it. Best-effort,
    /// as ``running(address:port:certPEM:keyPEM:hostFingerprint:)``.
    public static func status(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data?
    ) async -> (games: [RunningGame], downloads: [HostDownload], grants: UInt32?) {
        guard let identity = try? clientIdentity(certPEM: certPEM, keyPEM: keyPEM),
              let response = try? await send(
                  path: "/api/v1/status", address: address, port: port,
                  identity: identity, hostFingerprint: hostFingerprint),
              response.status == 200,
              let status = try? JSONDecoder().decode(HostStatus.self, from: response.body)
        else { return ([], [], nil) }
        return (status.games ?? [], status.downloads ?? [], status.grants)
    }

    /// Start, resume, pause or remove a title's download (`/api/v1/library/install/{id}`). Never
    /// throws: every outcome is something to tell the player.
    public static func changeInstall(
        appID: String,
        action: InstallAction,
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data
    ) async -> InstallOutcome {
        var allowed = CharacterSet.urlPathAllowed
        allowed.remove("/")
        let id = appID.addingPercentEncoding(withAllowedCharacters: allowed) ?? appID
        let path = "/api/v1/library/install/\(id)"
        do {
            let identity = try clientIdentity(certPEM: certPEM, keyPEM: keyPEM)
            let response: HTTPResponse
            switch action {
            case .install, .resume:
                response = try await send(
                    path: path, address: address, port: port, identity: identity,
                    hostFingerprint: hostFingerprint, body: (Data(), "application/json"))
            case .pause:
                response = try await send(
                    path: path + "/pause", address: address, port: port, identity: identity,
                    hostFingerprint: hostFingerprint, body: (Data(), "application/json"))
            case .remove:
                response = try await send(
                    path: path, address: address, port: port, identity: identity,
                    hostFingerprint: hostFingerprint, delete: true)
            }
            let json = try? JSONSerialization.jsonObject(with: response.body) as? [String: Any]
            return .from(status: response.status, message: json?["message"] as? String)
        } catch {
            return .failed((error as? LocalizedError)?.errorDescription ?? error.localizedDescription)
        }
    }

    /// Upload this client's recent log (`ClientLogRing`) to the host — `POST /api/v1/client-logs`,
    /// the one WRITE a paired certificate may make (the host's `mgmt/client_logs.rs`). Same lane
    /// and identity as the library; the host files the bundle under this device and shows it on
    /// its web console's Logs page next to its own log. Returns the stored bundle id (empty for a
    /// host that predates the id in the reply).
    ///
    /// Why it exists: on an Apple TV (or a phone, for anyone who is not a developer) there is no
    /// way to get the client's log off the device, so every fault report arrived with only the
    /// host's half of the story. `hostFingerprint` is required, not optional: this is an outbound
    /// write carrying the device's diagnostics, and it goes to the host the user paired with.
    public static func sendLogs(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data
    ) async throws -> String {
        let identity = try clientIdentity(certPEM: certPEM, keyPEM: keyPEM)
        let body = Data(ClientLogRing.render(header: ClientLogRing.header()).utf8)
        let response = try await send(
            path: "/api/v1/client-logs", address: address, port: port,
            identity: identity, hostFingerprint: hostFingerprint,
            body: (body, "text/plain; charset=utf-8"))
        switch response.status {
        case 200, 201:
            let json = try? JSONSerialization.jsonObject(with: response.body) as? [String: Any]
            return json?["id"] as? String ?? ""
        case 401, 403:
            throw LibraryError.unauthorized
        default:
            throw LibraryError.http(response.status)
        }
    }

    /// What this host lets THIS device do to it — sleep, restart, shut it down
    /// (`design/host-actions.md` §7) — from `GET /api/v1/actions`.
    ///
    /// Only the PERMITTED rows come back: the host is the only judge of whether this device's
    /// access carries the Host-power grant, and a row it would refuse is not this client's to
    /// render. Best-effort by contract, like ``running(address:port:certPEM:keyPEM:hostFingerprint:)``
    /// — an older host (no such route), an unreachable one, or a shape we don't recognise yields
    /// an empty list. A missing menu row costs a menu row; a thrown error would cost the screen.
    public static func actions(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data?
    ) async -> [HostAction] {
        guard let identity = try? clientIdentity(certPEM: certPEM, keyPEM: keyPEM),
              let response = try? await send(
                  path: "/api/v1/actions", address: address, port: port,
                  identity: identity, hostFingerprint: hostFingerprint),
              response.status == 200,
              let list = try? JSONDecoder().decode(HostActionList.self, from: response.body)
        else { return [] }
        return list.actions.filter(\.permitted)
    }

    /// End one title on the host, live session included (`POST /api/v1/game/end`). The host ends
    /// it only if this device launched it. Never throws: every outcome is something to tell the
    /// player.
    public static func endGame(
        appID: String,
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data
    ) async -> GameEndOutcome {
        let body = (try? JSONSerialization.data(
            withJSONObject: ["app_id": appID, "streaming": true])) ?? Data()
        do {
            let identity = try clientIdentity(certPEM: certPEM, keyPEM: keyPEM)
            let response = try await send(
                path: "/api/v1/game/end", address: address, port: port,
                identity: identity, hostFingerprint: hostFingerprint,
                body: (body, "application/json"))
            return .from(status: response.status)
        } catch {
            return .failed((error as? LocalizedError)?.errorDescription ?? error.localizedDescription)
        }
    }

    /// Invoke one host action by id (`POST /api/v1/actions/{id}`, empty body).
    ///
    /// Returning normally means the host ACCEPTED it (202) — it now ends every session and acts
    /// about a second later, so this is the last word the client will get. A refusal throws
    /// with the host's own sentence ("another device is streaming from this host right now"),
    /// which tells a person what to do where a bare status code would not.
    ///
    /// The body stays empty by design: the id is the whole request, and no request field ever
    /// reaches the host's privileged path.
    public static func invokeAction(
        id: String,
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data
    ) async throws {
        let identity = try clientIdentity(certPEM: certPEM, keyPEM: keyPEM)
        let escaped = id.addingPercentEncoding(withAllowedCharacters: .urlPathAllowed) ?? id
        let response = try await send(
            path: "/api/v1/actions/\(escaped)", address: address, port: port,
            identity: identity, hostFingerprint: hostFingerprint,
            body: (Data(), "application/json"))
        switch response.status {
        case 200, 202:
            return
        case 401, 403:
            throw LibraryError.unauthorized
        default:
            // The `ApiError` envelope carries the host's reason; prefer it over the code.
            let json = try? JSONSerialization.jsonObject(with: response.body) as? [String: Any]
            if let why = json?["error"] as? String, !why.isEmpty {
                throw LibraryError.unreachable(why)
            }
            throw LibraryError.http(response.status)
        }
    }

    /// Just the slice of `/status` this client reads. Everything else on that payload is the
    /// operator console's business, and decoding only what we use keeps an unrelated schema change
    /// on the host from breaking the library screen.
    struct HostStatus: Decodable {
        var games: [RunningGame]?
        var downloads: [HostDownload]?
        /// This device's live grants; absent from a host that predates the field.
        var grants: UInt32?
    }

    private struct HostActionList: Decodable {
        var actions: [HostAction]
    }

    /// `https://addr:port`, IPv6 literals bracketed — the mirror of the Rust client's `base_url`.
    static func baseURL(address: String, port: UInt16) -> String {
        let bare = MgmtTransport.unbracketed(address)
        return bare.contains(":") ? "https://[\(bare)]:\(port)" : "https://\(bare):\(port)"
    }

    /// Build the paired identity, restating any keychain failure in the UI's vocabulary.
    static func clientIdentity(certPEM: String, keyPEM: String) throws -> SecIdentity {
        do {
            return try ClientTLS.makeIdentity(certPEM: certPEM, keyPEM: keyPEM)
        } catch {
            throw LibraryError.unreachable(
                (error as? LocalizedError)?.errorDescription ?? error.localizedDescription)
        }
    }

    /// Build and cache the TLS identity ahead of the first request. Blocking Keychain work:
    /// call off the main actor, so the callers on it find the pair built.
    public static func warmIdentity(_ identity: ClientIdentity) {
        _ = try? ClientTLS.makeIdentity(certPEM: identity.certPEM, keyPEM: identity.keyPEM)
    }

    /// One request against the host — a GET, or a POST when `body` is given — with transport
    /// failures mapped onto `LibraryError`.
    static func send(
        path: String, address: String, port: UInt16,
        identity: SecIdentity, hostFingerprint: Data?,
        body: (data: Data, contentType: String)? = nil,
        delete: Bool = false
    ) async throws -> HTTPResponse {
        do {
            if delete {
                return try await MgmtTransport.delete(
                    host: address, port: port, path: path,
                    identity: identity, pinnedHostFingerprint: hostFingerprint)
            }
            if let body {
                return try await MgmtTransport.post(
                    host: address, port: port, path: path, body: body.data,
                    contentType: body.contentType,
                    identity: identity, pinnedHostFingerprint: hostFingerprint)
            }
            return try await MgmtTransport.get(
                host: address, port: port, path: path,
                identity: identity, pinnedHostFingerprint: hostFingerprint)
        } catch MgmtTransportError.pinMismatch {
            throw LibraryError.pinMismatch
        } catch MgmtTransportError.unpinned {
            throw LibraryError.unauthorized
        } catch MgmtTransportError.timedOut {
            throw LibraryError.unreachable("timed out")
        } catch let error as MgmtTransportError {
            throw LibraryError.unreachable(String(describing: error))
        } catch {
            throw LibraryError.unreachable(error.localizedDescription)
        }
    }
}

extension Artwork {
    /// Rewrite any host-relative field (one starting with `/`) into an absolute URL against `base`.
    /// External CDN URLs (GOG/Heroic/Xbox) and `data:` URLs (Lutris) already don't start with `/`,
    /// so they pass through unchanged. `internal` (not `fileprivate`) so `LibraryClientTests` can
    /// exercise it directly without a live host.
    func resolved(against base: URL) -> Artwork {
        func abs(_ s: String?) -> String? {
            guard let s, s.hasPrefix("/") else { return s }
            return URL(string: s, relativeTo: base)?.absoluteString ?? s
        }
        var a = self
        a.portrait = abs(a.portrait)
        a.hero = abs(a.hero)
        a.logo = abs(a.logo)
        a.header = abs(a.header)
        return a
    }
}

/// Anything that answers poster bytes for a cover-art URL. The production implementation is
/// [`LibraryArtLoader`]; the screenshot harness substitutes a canned source so store frames carry
/// artwork without a host on the network.
public protocol LibraryArtSource: Sendable {
    func data(for url: URL) async throws -> Data
    /// Release pooled connections when the owning screen goes away. Sources without connections
    /// have nothing to do.
    func close() async
}

/// One fetch per key, shared by everyone who asks while it flies, and cancelled once the last
/// of them is. A tile scrolled past gives up its fetch; a tile still on screen keeps it.
final class ArtFlights: @unchecked Sendable {
    private struct Flight {
        let task: Task<Data, Error>
        var waiters: Int
    }

    private let lock = NSLock()
    private var flights: [String: Flight] = [:]

    func value(
        for key: String, fetch: @escaping @Sendable () async throws -> Data
    ) async throws -> Data {
        let task: Task<Data, Error> = lock.withLock {
            if var flying = flights[key] {
                flying.waiters += 1
                flights[key] = flying
                return flying.task
            }
            let task = Task.detached(operation: fetch)
            flights[key] = Flight(task: task, waiters: 1)
            return task
        }
        defer { lock.withLock { if flights[key]?.task == task { flights[key] = nil } } }
        return try await withTaskCancellationHandler {
            try await task.value
        } onCancel: {
            lock.withLock {
                guard var flying = flights[key], flying.task == task else { return }
                flying.waiters -= 1
                if flying.waiters > 0 {
                    flights[key] = flying
                } else {
                    flights[key] = nil
                    task.cancel()
                }
            }
        }
    }
}

/// Loads cover art for the library UI, routing each URL to the transport that suits its origin.
///
/// A `GameEntry`'s art candidates mix two very different things: the host's own art proxy
/// (`/api/v1/library/art/...`, resolved to absolute URLs against this host) and public store CDN
/// URLs carried verbatim on custom/GOG/Heroic entries. Host URLs go over [`MgmtTransport`] with
/// the paired identity and the pinned fingerprint — outside the URL loading system, so App
/// Transport Security can stay ON app-wide. Every other origin keeps ordinary `URLSession` with
/// full system trust evaluation and no client certificate, which is exactly what it should get.
///
/// Posters are cached on disk (`ArtCache`), so a second visit to a library costs no network at
/// all — and the connections behind a first visit are pooled and kept alive rather than paying a
/// TLS handshake per tile.
///
/// Built once per library screen and reused across a whole grid's worth of posters.
public final class LibraryArtLoader: LibraryArtSource, @unchecked Sendable {
    private let address: String
    private let port: UInt16
    private let identity: SecIdentity
    private let hostFingerprint: Data?
    /// Third-party origins only, with the system's normal certificate validation and no URLCache
    /// (`ArtCache` owns persistence). Process-wide and never invalidated: a fetch that outlives
    /// `close()` would otherwise create a task on a dead session, which raises.
    private static let cdn: URLSession = {
        let config = URLSessionConfiguration.default
        config.urlCache = nil
        return URLSession(configuration: config)
    }()
    /// nil when the caches directory is unavailable — then we simply always fetch.
    private var cache: ArtCache? { ArtCache.shared }
    /// One fetch per cache key at a time — the same entry shown in two sections must not fetch
    /// its art twice on a cold cache. Failures are deliberately not remembered.
    private let flights = ArtFlights()

    public init(
        address: String,
        port: UInt16 = punktfunkDefaultMgmtPort,
        certPEM: String,
        keyPEM: String,
        hostFingerprint: Data?
    ) throws {
        self.address = address
        self.port = port
        self.identity = try LibraryClient.clientIdentity(certPEM: certPEM, keyPEM: keyPEM)
        self.hostFingerprint = hostFingerprint
    }

    /// Image bytes for one art URL, cached on disk after the first fetch. A miss propagates the
    /// error so the poster can move on to its next candidate.
    public func data(for url: URL) async throws -> Data {
        // Inline art never leaves the manifest — decode it here rather than probe a cache that
        // could never hold it.
        if url.scheme?.lowercased() == "data" { return try Self.inlineBytes(url) }
        let key = Self.cacheKey(for: url, hostAddress: address, hostPort: port, pin: hostFingerprint)
        if let cache, let cached = await cache.data(forKey: key) { return cached }
        let fetched = try await flights.value(for: key) { try await self.fetch(url) }
        if let cache { await cache.store(fetched, forKey: key) }
        return fetched
    }

    /// The bytes of a base64 `data:` URL — art a plugin inlined rather than linked. Base64 is the
    /// only form the kit emits; anything else, or a body over the transport ceiling, is refused.
    static func inlineBytes(_ url: URL) throws -> Data {
        let s = url.absoluteString
        guard let comma = s.firstIndex(of: ","), s[..<comma].lowercased().hasSuffix(";base64"),
              let data = Data(base64Encoded: String(s[s.index(after: comma)...])),
              !data.isEmpty, data.count <= MgmtTransport.maxResponseBytes
        else { throw LibraryError.badArtURL }
        return data
    }

    /// Release this host's pooled connections — call when the library screen goes away, so we
    /// don't sit on open TLS sockets the user is finished with.
    public func close() async {
        await MgmtConnectionPool.shared.closeAll(
            matching: "\(MgmtTransport.unbracketed(address)):\(port):")
    }

    /// The cache entry's identity. A HOST-origin URL is `pin | path` — the machine, not the
    /// address it happened to have at fetch time: a re-addressed host must keep its cache, and a
    /// different machine answering the same address must not inherit it. Anything else is keyed
    /// by its URL, which already names its origin.
    static func cacheKey(for url: URL, hostAddress: String, hostPort: UInt16, pin: Data?) -> String {
        guard isHostOrigin(url, address: hostAddress, port: hostPort) else {
            return url.absoluteString
        }
        if let pin { return "\(MgmtTransport.hex(pin))|\(requestPath(url))" }
        return "tofu|\(MgmtTransport.unbracketed(hostAddress)):\(hostPort)|\(requestPath(url))"
    }

    /// The request path exactly as it goes on the wire — the ENCODED components. `url.path` and
    /// `url.query` hand back percent-DECODED text: an id that needed encoding breaks the request
    /// line, and a decoded CRLF splits the request outright.
    static func requestPath(_ url: URL) -> String {
        let parts = URLComponents(url: url, resolvingAgainstBaseURL: true)
        var path = parts?.percentEncodedPath ?? ""
        if path.isEmpty { path = "/" }
        if let query = parts?.percentEncodedQuery { path += "?\(query)" }
        return path
    }

    private func fetch(_ url: URL) async throws -> Data {
        guard Self.isHostOrigin(url, address: address, port: port) else {
            // A library entry names its own art URL, so this is host-supplied. Web schemes and
            // inline `data:` only — a `file:` URL would make the client read its own container and
            // cache the result as a poster — and the same ceiling the pinned path enforces, since
            // nothing else bounds a CDN body.
            if url.scheme?.lowercased() == "data" { return try Self.inlineBytes(url) }
            guard let scheme = url.scheme?.lowercased(), scheme == "https" || scheme == "http"
            else { throw LibraryError.badArtURL }
            let (bytes, response) = try await Self.cdn.bytes(from: url)
            guard let http = response as? HTTPURLResponse else { throw LibraryError.badArtURL }
            guard (200..<300).contains(http.statusCode) else {
                throw LibraryError.http(http.statusCode)
            }
            // Bound the body WHILE it streams: the ceiling is decorative if every byte is in
            // memory already when it's checked. A declared length past it is refused unread.
            let ceiling = MgmtTransport.maxResponseBytes
            guard http.expectedContentLength <= Int64(ceiling) else {
                throw MgmtTransportError.tooLarge
            }
            var data = Data()
            var block: [UInt8] = []
            block.reserveCapacity(65_536)
            for try await byte in bytes {
                block.append(byte)
                guard block.count == 65_536 else { continue }
                data.append(contentsOf: block)
                block.removeAll(keepingCapacity: true)
                if data.count > ceiling { throw MgmtTransportError.tooLarge }
            }
            data.append(contentsOf: block)
            if data.count > ceiling { throw MgmtTransportError.tooLarge }
            return data
        }
        let response = try await LibraryClient.send(
            path: Self.requestPath(url), address: address, port: port,
            identity: identity, hostFingerprint: hostFingerprint)
        guard response.status == 200 else { throw LibraryError.http(response.status) }
        return response.body
    }

    /// Does this URL point at the host's own art proxy? Compared on host + port rather than a
    /// string prefix, so a differently-spelled but equivalent URL still takes the pinned path.
    static func isHostOrigin(_ url: URL, address: String, port: UInt16) -> Bool {
        guard let host = url.host else { return false }
        let bare = MgmtTransport.unbracketed(address)
        let scheme = url.scheme?.lowercased()
        return host.caseInsensitiveCompare(bare) == .orderedSame
            && (url.port ?? (scheme == "http" ? 80 : 443)) == Int(port)
    }
}
