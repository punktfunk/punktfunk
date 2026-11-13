// What each paired host has UP right now (`GET /api/v1/status`), so a host card can say so
// before anybody connects — the Apple port of the console's `HostRow::running`
// (crates/client/pf-console-ui/src/model.rs), fed by the same route on the same mTLS lane.
//
// A sibling of `HostPowerStore` with a far shorter fuse: what a host may be asked to do changes
// when an operator edits access, but what it is PLAYING changes while somebody is looking at the
// card. Nothing here is ever persisted — a remembered title would have the home screen claiming a
// game is up because it was up last night.

import Foundation
import PunktfunkKit

/// The per-host "now playing" cache, shared by the touch grid, the console carousel and the
/// library shelf's own menu — one answer, so no two surfaces disagree about what is up.
@MainActor
final class NowPlayingStore: ObservableObject {
    static let shared = NowPlayingStore()

    /// Each host's title, "" for nothing up. 20 s, short on purpose (see the file note); the
    /// surfaces that call `refresh` already tick about this often for reachability.
    private var cache = TTLCache<String>(ttl: 20)

    /// The title `host` last said it has up, or nil for nothing running, an unpaired or
    /// unreachable host, and one nobody has asked yet — all of which a card draws the same way.
    func title(for host: StoredHost) -> String? {
        cache[host.id.uuidString].flatMap { $0.isEmpty ? nil : $0 }
    }

    /// Ask again unless the cached answer is still fresh. Cheap and idempotent — call it from
    /// whatever refresh tick a surface already runs.
    func refresh(_ host: StoredHost) {
        // The demo host serves no management API.
        guard !DemoMode.isDemo(host), case .success(let target) = MgmtTarget.make(host: host),
              let ticket = cache.ask(host.id.uuidString, now: Date())
        else { return }
        Task { @MainActor in
            let games = await LibraryClient.statusIfAnswered(target)?.games
            objectWillChange.send()
            cache.land(ticket, games.map(Self.title))
        }
    }

    /// Take an answer a caller already has — the library shelf fetches `/status` for its own
    /// Resume badges, and a second request for the same fact would be one the host is asked
    /// twice. Also stamps it fresh: this IS the freshest answer available.
    func adopt(_ games: [RunningGame], for host: StoredHost) {
        objectWillChange.send()
        cache.store(Self.title(games), for: host.id.uuidString, now: Date())
    }

    /// The first entry that is up and has a title to show, so a launch the host cannot track
    /// (no catalog id, hence no badge on any shelf) still names itself on the card.
    private static func title(_ games: [RunningGame]) -> String {
        games.first { $0.isUp && !$0.title.isEmpty }?.title ?? ""
    }

    /// Forget what this host said — the caller just ended a session on it, so the answer is
    /// about to change and the next tick must ask rather than wait out the TTL.
    func invalidate(_ host: StoredHost) {
        objectWillChange.send()
        cache.invalidate(host.id.uuidString)
    }
}
