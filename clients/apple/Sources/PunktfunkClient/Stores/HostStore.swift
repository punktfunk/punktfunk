// Saved hosts + their pinned identities, persisted as JSON in UserDefaults.
//
// Trust model (client side of punktfunk/1): the host serves a persistent certificate and
// logs its SHA-256 fingerprint at startup. The pin lands here one of two ways — the
// trust-on-first-use prompt (user compares the observed fingerprint against the host's
// log) or the SPAKE2 PIN pairing ceremony (PairSheet; mutually verified, and the host
// stores our identity from ClientIdentityStore in return). Every later connect passes
// the pin into punktfunk-core, which refuses a host whose identity changed. Hosts running
// --require-pairing only admit paired clients, so for them pairing is the only way in.

import Foundation
import PunktfunkKit
import SwiftUI
#if canImport(WidgetKit)
import WidgetKit
#endif

// `StoredHost` (the model + its JSON codec) now lives in PunktfunkShared so the widget extension
// can read the same store; PunktfunkKit re-exports it. The discovery-join helpers below stay here
// because they reference PunktfunkKit's `DiscoveredHost`/`HostDiscovery`.

extension StoredHost {
    /// True when a live mDNS advert (`DiscoveredHost`) describes THIS saved host — drives the
    /// "online" indicator and de-dupes the discovered section. The rule is
    /// `DiscoveredHost.matches(pin:address:port:)`. Online detection is LAN-scoped: a host not
    /// advertising on this network (off, or a remote/cross-subnet address) simply won't match —
    /// "not seen", not proven off.
    func matches(_ discovered: DiscoveredHost) -> Bool {
        discovered.matches(pin: pinnedSHA256?.hexLower, address: address, port: port)
    }
}

/// The join of live mDNS discovery against the saved-host store, shared by the touch grid
/// (HomeView) and the gamepad launcher (GamepadHomeView) so both screens classify hosts the same
/// way. Presence is NOT part of it: whether a host is up is `HostStore.isReachable`, because an
/// advert outlives the machine it describes by up to 75 minutes.
extension HostDiscovery {
    /// Discovered hosts not already saved — the saved list shows the rest, so this only surfaces
    /// genuinely-new hosts on the network. Same match as `advertises`, so a saved host whose IP
    /// changed (still fingerprint-matched) doesn't also appear as a stranger.
    func unsaved(among saved: [StoredHost]) -> [DiscoveredHost] {
        hosts.filter { d in !saved.contains { $0.matches(d) } }
    }
}

@MainActor
final class HostStore: ObservableObject {
    /// The one store per process. Every mutation rewrites the whole array from THIS instance's
    /// copy, so a second instance (macOS opens a window per Cmd+N) would persist its own stale
    /// view over the first's — a host paired in one window loses its pin the moment the other
    /// window writes, and the user has to pair again.
    static let shared = HostStore()

    private static let key = DefaultsKey.hosts

    @Published var hosts: [StoredHost] {
        didSet { persist() }
    }

    /// Saved hosts proven reachable by the periodic QUIC probe (by id) — the mDNS-independent
    /// counterpart to discovery presence, OR'd into the "online" pip so a routed/VPN host that
    /// never advertises still reads Online. Not persisted (it's live reachability, not config).
    @Published var probedOnline: Set<StoredHost.ID> = []

    /// The App-Group suite — shared with the Widget/Live-Activity extension so a launcher widget
    /// sees the same saved hosts. Falls back to `.standard` in an un-entitled process (see
    /// `AppGroup.defaults`).
    private let defaults = AppGroup.defaults

    init() {
        Self.migrateToAppGroupIfNeeded()
        // Per-element (see `StoredHost.loadAll`): decoding the array as a whole meant one
        // unreadable record lost every saved host, and the first `markConnected` after that
        // persisted the empty array straight over the user's real store.
        hosts = StoredHost.loadAll(from: defaults, recentFirst: false)
    }

    /// One-time move of the saved-host JSON from `UserDefaults.standard` (where every build before
    /// the App Group wrote it) into the shared suite. Idempotent: only fires when the suite has no
    /// hosts yet but standard does. The old value is LEFT in place — during a staged TestFlight
    /// rollout an older build still reads `.standard`, so tombstoning it now would hide hosts from
    /// the not-yet-updated app. Remove the standard copy a release later.
    private static func migrateToAppGroupIfNeeded() {
        let suite = AppGroup.defaults
        let standard = UserDefaults.standard
        guard suite !== standard else { return } // un-entitled fallback: nothing to migrate
        guard suite.data(forKey: key) == nil,
              let legacy = standard.data(forKey: key) else { return }
        suite.set(legacy, forKey: key)
    }

    func add(_ host: StoredHost) {
        // The address App Review is given saves the demo host instead.
        if DemoMode.isDemoAddress(host.address) {
            DemoMode.enable(in: self)
            return
        }
        var host = host
        // Stamped here rather than in the initializer: a `StoredHost` is also built to describe a
        // host we are only dialing (the dev auto-connect hook, a deep link's confirmation), and
        // those were never added to anything.
        host.addedAt = host.addedAt ?? Date()
        hosts.append(host)
    }

    /// Also drops what the device kept for it: the default-host pointer, the library position,
    /// the favorites and the cached catalog. Removing the demo host stops it.
    func remove(_ host: StoredHost) {
        hosts.removeAll { $0.id == host.id }
        if DemoMode.isDemo(host) { DemoMode.stop() }
        clearDefaultHostIfItNames(host)
        LibraryScrollMemory.forget(hostID: host.id.uuidString)
        LibraryFavorites.shared.forget(hostID: host.id.uuidString)
        Task { await LibraryCache.shared?.forget(hostID: host.id.uuidString) }
    }

    /// Replace a saved host in place (the edit sheet) — matched by id, so identity/pin/last-connected
    /// carried on the passed value are preserved.
    func update(_ host: StoredHost) {
        guard let i = hosts.firstIndex(where: { $0.id == host.id }) else { return }
        hosts[i] = host
    }

    func markConnected(_ hostID: UUID) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }) else { return }
        hosts[i].lastConnected = Date() // didSet → persist() writes the shared suite + reloads widget
    }

    /// Is `host` reachable RIGHT NOW — the one definition of online, used by the pip, the
    /// auto-wake gate and the wake-wait alike.
    ///
    /// A live advert is NOT that answer. An mDNS browse result is a cache entry with a 75-minute
    /// PTR TTL, and a host that suspends sends no goodbye, so a sleeping machine keeps advertising
    /// to every client for up to an hour. So a bounded QUIC handshake says whether THIS host is
    /// there, and the advert only says where else to look.
    ///
    /// A host lives at more than one address — its LAN lease at home, a Tailscale address
    /// anywhere. The saved one is asked first and kept while it answers. Only when it is silent
    /// are the live advert's and the addresses it left asked, together, and the first that
    /// answers (in that order) becomes the saved address: the LAN takes over with the VPN off,
    /// and Tailscale takes back over on mobile data.
    ///
    /// The pin decides, not the answer alone: whoever inherits a sleeping host's lease completes
    /// a handshake at its address too. A host saved by address carries no pin to compare, so it
    /// is asked there only, and any answer is the one it names.
    func isReachable(_ host: StoredHost, discovery: HostDiscovery) async -> Bool {
        let target = hosts.first { $0.id == host.id } ?? host
        let pin = target.pinnedSHA256
        if await Self.answers(target.address, target.port, pin: pin) { return true }
        guard pin != nil else { return false }
        var others: [(address: String, port: UInt16)] = []
        if let live = discovery.hosts.first(where: { host.matches($0) }) {
            others.append((address: live.host, port: live.port))
        }
        others += (target.previousAddresses ?? []).map { (address: $0, port: target.port) }
        others.removeAll { $0.address == target.address && $0.port == target.port }
        guard let at = await Self.firstAnswering(others, pin: pin) else { return false }
        updateAddress(host.id, address: at.address, port: at.port)
        return true
    }

    /// The first of `candidates`, in order, that the host answers at — all asked at once.
    private static func firstAnswering(
        _ candidates: [(address: String, port: UInt16)], pin: Data?
    ) async -> (address: String, port: UInt16)? {
        let answered = await withTaskGroup(of: (Int, Bool).self) { group in
            for (i, c) in candidates.enumerated() {
                group.addTask { (i, await Self.answers(c.address, c.port, pin: pin)) }
            }
            var hits = Set<Int>()
            for await (i, hit) in group where hit { hits.insert(i) }
            return hits
        }
        return candidates.indices.first(where: answered.contains).map { candidates[$0] }
    }

    /// Did the host pinned to `pin` (any host, when `nil`) answer a probe at this address?
    private static func answers(_ address: String, _ port: UInt16, pin: Data?) async -> Bool {
        await Task.detached(priority: .utility) {
            guard let answered = PunktfunkConnection.probeIdentity(host: address, port: port) else {
                return false
            }
            return pin.map { $0 == answered } ?? true
        }.value
    }

    /// One reachability sweep, driving `probedOnline`: probe every saved host and publish the
    /// reachable set. Call in a loop from a home view's `.task` (cancelled on disappear).
    ///
    /// All hosts at once, as the desktop clients' `probe_known` does. Asked in turn, every silent
    /// host cost a full probe timeout (1.7 s, twice for a pinned one with somewhere else to look)
    /// before the next was asked, and nothing was published until the last — so a list holding a
    /// few sleeping machines took half a minute to light the one that was up. A host lights the
    /// moment it answers; the end of the lap drops the ones that stopped.
    func refreshReachability(discovery: HostDiscovery) async {
        #if DEBUG
        guard !probePinned else { return } // a seeded reachable set outranks the live LAN
        #endif
        var online: Set<StoredHost.ID> = []
        await withTaskGroup(of: (StoredHost.ID, Bool).self) { group in
            for host in hosts {
                group.addTask { (host.id, await self.isReachable(host, discovery: discovery)) }
            }
            for await (id, up) in group where up {
                online.insert(id)
                if !probedOnline.contains(id) { probedOnline.insert(id) }
            }
        }
        probedOnline = online
    }

    #if DEBUG
    /// A seeded reachable set is in force — the sweep must not replace it with live probes.
    private var probePinned = false

    /// Screenshot/preview seam, the store's counterpart to `HostDiscovery.debugSet`: pin which
    /// saved hosts read Online and keep the sweep off. A capture has no network, so every real
    /// probe fails and every mock host would read Offline.
    func debugSetProbedOnline(_ ids: Set<StoredHost.ID>) {
        probePinned = true
        probedOnline = ids
    }
    #endif

    func pin(_ hostID: UUID, fingerprint: Data) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }) else { return }
        hosts[i].pinnedSHA256 = fingerprint
    }

    /// Learn/refresh this host's Wake-on-LAN MAC(s) from its live advert (called while the host is
    /// awake, so the client can wake it once it sleeps). No-op when unchanged, so it doesn't churn
    /// UserDefaults on every discovery tick.
    func updateMacs(_ hostID: UUID, macs: [String]) {
        guard !macs.isEmpty,
              let i = hosts.firstIndex(where: { $0.id == hostID }),
              hosts[i].macAddresses != macs else { return }
        hosts[i].macAddresses = macs
    }

    /// Re-point this host at the address it just answered at, keeping the one it leaves in
    /// `previousAddresses` so the sweep can find it there again. Same no-op-when-unchanged
    /// contract as `updateMacs`.
    func updateAddress(_ hostID: UUID, address: String, port: UInt16) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }),
              hosts[i].address != address || hosts[i].port != port else { return }
        var host = hosts[i]
        host.move(to: address, port: port)
        hosts[i] = host
    }

    /// Learn/refresh this host's OS-identity chain from its live advert — same contract as
    /// [`updateMacs`]: no-op when empty or unchanged, so discovery ticks don't churn UserDefaults.
    func updateOsChain(_ hostID: UUID, chain: String) {
        guard !chain.isEmpty,
              let i = hosts.firstIndex(where: { $0.id == hostID }),
              hosts[i].osChain != chain else { return }
        hosts[i].osChain = chain
    }

    /// Learn/refresh this host's management-API port from its live advert — same contract as
    /// `updateMacs`. Until this existed, `StoredHost.mgmtPort` was declared and read but never
    /// written, so `effectiveMgmtPort` always answered 47990 and a host that had moved its mgmt
    /// port simply had no working library here.
    func updateMgmtPort(_ hostID: UUID, port: UInt16?) {
        guard let port, port > 0,
              let i = hosts.firstIndex(where: { $0.id == hostID }),
              hosts[i].mgmtPort != port else { return }
        hosts[i].mgmtPort = port
    }

    /// Bind this host to a settings preset, or to "Default settings" (nil) — the ONLY way the
    /// default changes. A one-off "Connect with ▸" deliberately never lands here (§5.2:
    /// predictable, not sticky).
    func setPreset(_ hostID: UUID, presetID: String?) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }) else { return }
        hosts[i].presetID = presetID
    }

    /// Pin or unpin a host+preset combo as its own card (§5.2a). Presentation only: it never
    /// touches the default binding or the preset itself. nil stays out of the saved JSON when
    /// nothing is pinned, so the widget contract sees no new key for the common case.
    func setPinned(_ hostID: UUID, presetID: String, pinned: Bool) {
        guard let i = hosts.firstIndex(where: { $0.id == hostID }) else { return }
        var pins = hosts[i].pinnedPresetIDs ?? []
        pins.removeAll { $0 == presetID }
        if pinned { pins.append(presetID) }
        hosts[i].pinnedPresetIDs = pins.isEmpty ? nil : pins
    }

    /// Drop the pinned identity (e.g. after a legitimate host reinstall). This does NOT downgrade
    /// to TOFU: the next connect re-pairs via the PIN ceremony, unless the host advertises
    /// `pair=optional` (the only case the connect path still offers the trust prompt).
    func forgetIdentity(_ host: StoredHost) {
        guard let i = hosts.firstIndex(where: { $0.id == host.id }) else { return }
        hosts[i].pinnedSHA256 = nil
        clearDefaultHostIfItNames(host)
    }

    /// Drop the start-screen pointer when it names a host that just stopped being a landing.
    /// `StartScreen.resolve` already ignores a dangling or unpaired id, so this is hygiene: it
    /// stops a later re-pair of a different box inheriting somebody's old choice.
    private func clearDefaultHostIfItNames(_ host: StoredHost) {
        let stored = UserDefaults.standard.string(forKey: DefaultsKey.defaultHost) ?? ""
        guard stored.lowercased() == host.id.uuidString.lowercased() else { return }
        UserDefaults.standard.removeObject(forKey: DefaultsKey.defaultHost)
    }


    private func persist() {
        #if DEBUG
        // The screenshot harness fills a store with mock hosts (ShotMock) purely to render a
        // scene. On a dev Mac that store is the SAME App-Group suite the real app reads, so
        // persisting would replace the tester's saved hosts with "Battlestation" & co.
        if ScreenshotMode.isActive { return }
        #endif
        if let data = try? JSONEncoder().encode(hosts) {
            defaults.set(data, forKey: Self.key)
        }
        reloadHostsWidget() // the widgets read this store; any change refreshes their timelines
    }

    /// Ask WidgetKit to rebuild the launcher widgets' timelines after any store change (add/remove/
    /// pin/last-connected). iOS-only and a no-op where WidgetKit is absent; both widgets use
    /// `.never`-refresh entries and rely on this push.
    private func reloadHostsWidget() {
        #if canImport(WidgetKit) && os(iOS)
        WidgetCenter.shared.reloadTimelines(ofKind: WidgetKind.hosts)
        WidgetCenter.shared.reloadTimelines(ofKind: WidgetKind.library)
        #endif
    }
}
