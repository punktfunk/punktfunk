// LAN auto-discovery of punktfunk/1 hosts over mDNS — the client side of the host's
// `crate::discovery` advert (`_punktfunk._udp`). Browses with NWBrowser (TXT rides in the
// result metadata), resolves each service to a connectable IP:port with a throwaway
// NWConnection, and publishes the live set.
//
// The advertised `fp` (host cert SHA-256) is ADVISORY: mDNS is unauthenticated, so pinning
// still verifies the host on connect. iOS/tvOS browse only with `_punktfunk._udp` listed in
// Info.plist `NSBonjourServices` (Config/Info.plist).
//
// SELF-HEALING: neither Network.framework primitive recovers on its own.
//   - `browseResultsChangedHandler` fires only when the result SET changes, so a service whose
//     resolve failed is never re-offered.
//   - `NWConnection` has no timeout: a resolve that cannot complete (v6-only advert against our
//     IPv4 pin, Wi-Fi still associating) parks in `.preparing`/`.waiting` forever.
//   - `NWBrowser` parks in `.waiting` while the browse is blocked, as by iOS's local-network
//     prompt on first launch, and a grant does NOT revive it: only a new browser sees it.
// A 1 Hz `sweep` times out stuck resolves, retries failed ones on a backoff and re-arms a browser
// that stopped working; `refresh()` (pull-to-refresh, the Refresh button) does so at once.

import Foundation
import Network
import PunktfunkShared

/// A punktfunk/1 host found on the LAN. `fingerprintHex` is advisory (see file header).
public struct DiscoveredHost: Identifiable, Sendable, Equatable {
    /// Stable host id (mDNS `id` TXT); falls back to the Bonjour instance name.
    public let id: String
    /// Bonjour instance name (the host's chosen label).
    public let name: String
    /// Resolved address to hand to `PunktfunkConnection`.
    public let host: String
    public let port: UInt16
    /// Host cert SHA-256 (lowercase hex) the host advertised, or nil if absent.
    public let fingerprintHex: String?
    /// The host advertised `pair=required` — a client must pair before it can stream.
    public let requiresPairing: Bool
    /// The host EXPLICITLY advertised `pair=optional` — only then may the client offer the
    /// reduced-security TOFU "Trust" path. A missing/unknown `pair` field is NOT optional:
    /// pairing is mandatory unless this is true (the policy authority is the host's advert).
    public let allowsTofu: Bool
    /// Wake-on-LAN MAC address(es) the host advertised (mDNS `mac` TXT, comma-separated
    /// `aa:bb:cc:dd:ee:ff`, routed NIC first). Empty when not advertised. A client persists these
    /// onto the saved host so it can wake it after it sleeps; advisory/unauthenticated (a wrong
    /// value only makes a wake fail — the magic packet is inert and the fingerprint still gates
    /// the connection).
    public let macAddresses: [String]
    /// The host's OS-identity chain (mDNS `os` TXT, e.g. `linux/fedora/bazzite`), sanitized
    /// (`sanitizeOsChain`) — drives the host card's OS mark and is persisted like the MACs.
    /// Empty when not advertised (older host). Advisory/unauthenticated like the rest.
    public let osChain: String
    /// The host's management-API port (mDNS `mgmt` TXT) — where the game library is served, NOT
    /// `port`, which is the native QUIC plane. nil when not advertised (older host), and the
    /// client then assumes `punktfunkDefaultMgmtPort`.
    ///
    /// Persisted onto the saved host like the MACs and the OS chain: without it
    /// `StoredHost.effectiveMgmtPort` falls back to 47990, and a host that moved its mgmt port
    /// (to share a machine with a Sunshine fork, whose web UI owns 47990) shows no library.
    public let mgmtPort: UInt16?
}

public extension DiscoveredHost {
    /// Is this advert the saved host pinned to `pin` at `address`:`port`?
    ///
    /// Two known fingerprints decide it alone: it survives a DHCP address change, and it keeps
    /// the other OS of a dual-boot box — one lease, one MAC, a certificate each — from reading
    /// as the OS already saved, which is what keeps it in the discovered section where it can
    /// be added. Only when one side is unpinned does address:port answer. The Rust clients hold
    /// the same rule in `discovery::same_host`.
    func matches(pin: String?, address: String, port: UInt16) -> Bool {
        if let pin, !pin.isEmpty, let fp = fingerprintHex, !fp.isEmpty {
            return pin.lowercased() == fp.lowercased()
        }
        return address == host && port == self.port
    }
}

@MainActor
public final class HostDiscovery: ObservableObject {
    /// Currently-visible hosts, deduped by `id`, sorted by name. Main-actor.
    @Published public private(set) var hosts: [DiscoveredHost] = []
    /// True for a moment after a rescan is kicked off, so a Refresh control can show that it did
    /// something on the surfaces with no pull-to-refresh spinner of their own (macOS, tvOS).
    @Published public private(set) var isScanning = false

    private var browser: NWBrowser?
    /// Every service the browser currently reports, keyed by the endpoint's description (a stable,
    /// Sendable handle we can capture into the resolve callbacks without smuggling non-Sendable
    /// Network types across hops). Held — not just diffed — so a retry can re-resolve a service
    /// the browser will never report again (see the file header).
    private var services: [String: NWBrowser.Result] = [:]
    /// The transport address a completed resolve produced, per service key. The rest of a
    /// `DiscoveredHost` comes from the advert's TXT, which is re-read on every browse report.
    private var addresses: [String: (host: String, port: UInt16)] = [:]
    private var connections: [String: NWConnection] = [:]
    /// Deadline for each in-flight resolve — `NWConnection` has none of its own.
    private var deadlines: [String: Date] = [:]
    /// Consecutive failed resolves per service, and when the next attempt is allowed.
    private var failures: [String: Int] = [:]
    private var retryAt: [String: Date] = [:]
    /// Services whose address should be re-resolved even though we already have one — set by
    /// `refresh()`. The old address keeps showing until the new one lands, so a rescan never
    /// blinks the list empty; without this a manual Refresh silently skipped every host it had
    /// already resolved, which is exactly the host whose address may have moved.
    private var staleAddresses: Set<String> = []
    /// Consecutive non-ready browser states, and when to tear it down and re-arm. nil = healthy.
    private var browserFailures = 0
    private var browserRearmAt: Date?
    /// Bumped on every re-arm so callbacks from a superseded browser — and from the resolves it
    /// started — are ignored instead of clobbering the current generation's bookkeeping.
    private var generation = 0
    /// The 1 Hz maintenance tick. Nothing else re-drives a stuck resolve or a sick browser.
    private var sweep: Task<Void, Never>?
    private var scanningUntil: Date?
    /// Until then a new browser's result set is still filling: a service missing from it is
    /// not gone. nil once settled.
    private var settlesAt: Date?

    /// A LAN resolve answers in milliseconds; this only has to outlast a slow Wi-Fi wake.
    private static let resolveTimeout: TimeInterval = 6
    /// How long `isScanning` holds — and `rescan()` waits — after a manual refresh.
    private static let scanSettle: TimeInterval = 1.5
    /// 1s, 2s, 4s, 8s … capped at 30s, for the resolve retry and the browser re-arm alike. Long
    /// enough that a genuinely-down network doesn't spin the main queue, short enough that a host
    /// coming back is picked up while the user is still looking at the screen.
    private static func backoff(_ failures: Int) -> TimeInterval {
        min(pow(2, Double(max(0, failures - 1))), 30)
    }

    public init() {}

    /// Start browsing `_punktfunk._udp`. Idempotent — a second call while live is a no-op.
    public func start() {
        #if DEBUG
        guard !debugPinned else { return } // a seeded advert set outranks the live LAN
        #endif
        guard browser == nil else { return }
        armBrowser()
        startSweep()
    }

    /// Stop browsing and drop all discovered state.
    public func stop() {
        sweep?.cancel()
        sweep = nil
        generation &+= 1
        browser?.cancel()
        browser = nil
        for conn in connections.values { conn.cancel() }
        connections.removeAll()
        deadlines.removeAll()
        services.removeAll()
        addresses.removeAll()
        failures.removeAll()
        retryAt.removeAll()
        staleAddresses.removeAll()
        browserFailures = 0
        browserRearmAt = nil
        scanningUntil = nil
        settlesAt = nil
        if isScanning { isScanning = false }
        if !hosts.isEmpty { hosts = [] }
    }

    /// Force a rescan now: re-arm the browser and retry every service whose resolve had failed,
    /// clearing the backoffs so nothing is left waiting. This is the manual escape hatch for the
    /// failure modes in the file header — and the only thing that clears the iOS local-network
    /// permission gate without an app restart, since only a NEW browser sees a permission the
    /// user granted after the old one started.
    ///
    /// Also starts discovery if it wasn't running, so a Refresh button does the obvious thing.
    public func refresh() {
        #if DEBUG
        guard !debugPinned else { return } // as in `start()` — the harness's set is the truth
        #endif
        isScanning = true
        scanningUntil = Date().addingTimeInterval(Self.scanSettle)
        failures.removeAll()
        retryAt.removeAll()
        staleAddresses = Set(services.keys)
        browserFailures = 0
        armBrowser()
        startSweep()
        pump()
    }

    /// `refresh()` for a `.refreshable` gesture: holds briefly so the control's spinner reflects a
    /// browse that had time to answer instead of blinking out instantly.
    public func rescan() async {
        refresh()
        try? await Task.sleep(nanoseconds: UInt64(Self.scanSettle * 1_000_000_000))
    }

    /// `refresh()`, but only when discovery is already running — the app-foreground hook. iOS
    /// suspends a backgrounded process's browse and `onAppear`/`onDisappear` don't fire across
    /// background/foreground, so a browse that died while suspended stayed dead on return; this
    /// re-arms it without starting a browse on a screen that deliberately isn't browsing
    /// (mid-session, where the home tore discovery down).
    public func refreshIfRunning() {
        guard browser != nil else { return }
        refresh()
    }

    deinit {
        sweep?.cancel()
        browser?.cancel()
        for conn in connections.values { conn.cancel() }
    }

    #if DEBUG
    /// A seeded advert set is in force — `start()` must not replace it with the live browse.
    private var debugPinned = false

    /// Screenshot/preview seam, the discovery counterpart to `HostWaker.debugSet`: publish a FIXED
    /// set of adverts and keep browsing off. Without it a capture shows whatever happens to be on
    /// the machine's LAN — the App Store screenshots shipped a stranger's hostname more than once —
    /// and every mock host reads Offline because nothing advertises it.
    public func debugSet(_ adverts: [DiscoveredHost]) {
        stop()
        debugPinned = true
        hosts = adverts
    }

    /// Builds one advert. `DiscoveredHost`'s memberwise init is internal (a public struct's is), and
    /// making it public would expose a wire-shaped model's construction to every consumer just to
    /// serve the harness.
    public static func debugAdvert(
        id: String, name: String, host: String, port: UInt16 = 9777,
        fingerprintHex: String? = nil, requiresPairing: Bool = false, allowsTofu: Bool = true,
        macAddresses: [String] = [], osChain: String = "", mgmtPort: UInt16? = nil
    ) -> DiscoveredHost {
        DiscoveredHost(
            id: id, name: name, host: host, port: port, fingerprintHex: fingerprintHex,
            requiresPairing: requiresPairing, allowsTofu: allowsTofu,
            macAddresses: macAddresses, osChain: osChain, mgmtPort: mgmtPort)
    }
    #endif

    // MARK: - Browser

    /// Build and start a fresh browser, retiring the previous one and every resolve it started.
    /// Those resolves' callbacks are gated on `generation`, so they must not be left holding map
    /// entries — `pump()` restarts them against the new generation.
    private func armBrowser() {
        generation &+= 1
        browser?.cancel()
        for conn in connections.values { conn.cancel() }
        connections.removeAll()
        deadlines.removeAll()
        browserRearmAt = nil
        settlesAt = Date().addingTimeInterval(Self.scanSettle)

        let generation = self.generation
        let browser = NWBrowser(
            for: .bonjourWithTXTRecord(type: "_punktfunk._udp", domain: nil),
            using: NWParameters())
        // `[weak self]` sits on the handler itself: on the inner closure it would make the handler
        // hold `self` strongly, and the running browser keeps its handlers.
        browser.browseResultsChangedHandler = { [weak self] results, _ in
            MainActor.assumeIsolated {
                guard let self, generation == self.generation else { return }
                self.reconcile(results)
            }
        }
        browser.stateUpdateHandler = { [weak self] state in
            MainActor.assumeIsolated {
                guard let self, generation == self.generation else { return }
                self.browserStateChanged(state)
            }
        }
        self.browser = browser
        browser.start(queue: .main)
    }

    /// A browser that stops working never recovers on its own, and it has two ways to stop:
    /// `.failed` (dead) and `.waiting` (blocked — a network change, or the iOS local-network
    /// permission gate described in the file header). Schedule a re-arm for both, on a backoff,
    /// so a browser that fails at once every time never spins the main queue.
    private func browserStateChanged(_ state: NWBrowser.State) {
        switch state {
        case .ready:
            browserFailures = 0
            browserRearmAt = nil
        case .failed, .waiting:
            guard browserRearmAt == nil else { return } // one re-arm already scheduled
            browserFailures += 1
            browserRearmAt = Date().addingTimeInterval(Self.backoff(browserFailures))
        default:
            break // .setup / .cancelled — nothing to heal
        }
    }

    /// Diff the browser's current result set against what we're tracking: drop departed services,
    /// record the rest — re-reading the advert every time, so a host that re-keys, moves or flips
    /// its pairing policy republishes under the same name and the card follows it — then resolve
    /// whatever still needs an address.
    ///
    /// A browser that has not settled drops nothing: its first sets are partial, and dropping
    /// from them blinks rows out of the list. `tick` reconciles again once it has.
    private func reconcile(_ results: Set<NWBrowser.Result>) {
        var live: Set<String> = []
        for result in results {
            let key = Self.key(result)
            live.insert(key)
            services[key] = result
        }
        if settlesAt == nil {
            for key in Array(services.keys) where !live.contains(key) { forget(key) }
        }
        publish()
        pump()
    }

    private func forget(_ key: String) {
        connections[key]?.cancel()
        connections[key] = nil
        deadlines[key] = nil
        services[key] = nil
        addresses[key] = nil
        failures[key] = nil
        retryAt[key] = nil
        staleAddresses.remove(key)
    }

    // MARK: - Resolve

    /// Start the resolves that are due: every live service with no address yet, nothing in flight,
    /// and past its retry time.
    private func pump() {
        let now = Date()
        for (key, result) in services {
            guard addresses[key] == nil || staleAddresses.contains(key) else { continue }
            guard connections[key] == nil else { continue }
            if let at = retryAt[key], at > now { continue }
            resolve(key, result)
        }
    }

    /// Resolve one service to IP:port via a short UDP connection (it reaches `.ready` once the
    /// path is established — no data is sent). The TXT is NOT read here: it comes from the browse
    /// result at publish time, so a re-advertised host doesn't need a fresh resolve to be re-read.
    private func resolve(_ key: String, _ result: NWBrowser.Result) {
        // Resolve over IPv4 only: Network.framework prefers IPv6 (RFC 6724), and the host's OS
        // mDNS responder often answers AAAA for its hostname even though the punktfunk host stack
        // (control QUIC + data UDP) binds IPv4 sockets exclusively — a v6-resolved address would
        // produce a host card whose connect always fails in the Rust core (`host:port` parse).
        // Same policy as the Android/desktop clients; lift when the stack speaks IPv6.
        let params = NWParameters.udp
        if let ip = params.defaultProtocolStack.internetProtocol as? NWProtocolIP.Options {
            ip.version = .v4
        }
        let conn = NWConnection(to: result.endpoint, using: params)
        connections[key] = conn
        deadlines[key] = Date().addingTimeInterval(Self.resolveTimeout)
        let generation = self.generation
        conn.stateUpdateHandler = { [weak self] state in
            MainActor.assumeIsolated {
                // Look the connection back up rather than capturing it — capturing it here would
                // retain the connection through its own handler.
                guard let self, generation == self.generation,
                      let conn = self.connections[key] else { return }
                switch state {
                case .ready:
                    let endpoint = conn.currentPath?.remoteEndpoint
                    self.connections[key] = nil
                    self.deadlines[key] = nil
                    conn.cancel()
                    if case let .hostPort(host, port)? = endpoint,
                       let address = Self.hostString(host) {
                        self.addresses[key] = (address, port.rawValue)
                        self.failures[key] = nil
                        self.retryAt[key] = nil
                        self.staleAddresses.remove(key)
                        self.publish()
                    } else {
                        // Ready but no usable remote — a failed attempt, not a finished one.
                        self.resolveFailed(key)
                    }
                case .failed, .cancelled:
                    self.connections[key] = nil
                    self.deadlines[key] = nil
                    self.resolveFailed(key)
                default:
                    break // .preparing / .waiting — the sweep's deadline is what ends these
                }
            }
        }
        conn.start(queue: .main)
    }

    private func resolveFailed(_ key: String) {
        let count = (failures[key] ?? 0) + 1
        failures[key] = count
        retryAt[key] = Date().addingTimeInterval(Self.backoff(count))
    }

    // MARK: - Sweep

    private func startSweep() {
        sweep?.cancel()
        sweep = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                guard !Task.isCancelled, let self else { return }
                self.tick()
            }
        }
    }

    private func tick() {
        let now = Date()
        // Time out the resolves that parked. Without this they never end, and `pump()` skips a
        // service that has a connection in flight — so that host stayed invisible indefinitely.
        for key in deadlines.filter({ $0.value <= now }).keys {
            connections[key]?.cancel()
            connections[key] = nil
            deadlines[key] = nil
            resolveFailed(key)
        }
        if let at = browserRearmAt, at <= now { armBrowser() }
        if let at = settlesAt, at <= now {
            settlesAt = nil
            if let browser { reconcile(browser.browseResults) }
        }
        pump()
        if let until = scanningUntil, until <= now {
            scanningUntil = nil
            isScanning = false
        }
    }

    // MARK: - Publish

    /// Publish the live adverts that have an address, deduped by `id` (a host on several
    /// interfaces / re-advertising collapses to one row), sorted by name.
    private func publish() {
        var byID: [String: DiscoveredHost] = [:]
        for key in services.keys.sorted() {
            guard let result = services[key], let address = addresses[key] else { continue }
            let host = Self.host(from: result, address: address.host, port: address.port)
            byID[host.id] = host
        }
        let next = byID.values.sorted {
            $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending
        }
        if next != hosts { hosts = next }
    }

    /// Join a browse result's advert (instance name + TXT) to a resolved address.
    private static func host(
        from result: NWBrowser.Result, address: String, port: UInt16
    ) -> DiscoveredHost {
        let name = instanceName(result.endpoint)
        var fp: String?
        var pair: String?
        var id: String?
        var macs: [String] = []
        var osChain = ""
        var mgmtPort: UInt16?
        if case let .bonjour(txt) = result.metadata {
            fp = entry(txt, "fp")
            pair = entry(txt, "pair")
            id = entry(txt, "id")
            macs = (entry(txt, "mac") ?? "")
                .split(separator: ",")
                .map { $0.trimmingCharacters(in: .whitespaces) }
                .filter { !$0.isEmpty }
            osChain = sanitizeOsChain(entry(txt, "os") ?? "")
            // Unauthenticated input, so range-check rather than trust: a non-numeric or 0 value
            // means "not advertised" and the client falls back to the default.
            mgmtPort = entry(txt, "mgmt").flatMap(UInt16.init).flatMap { $0 > 0 ? $0 : nil }
        }
        return DiscoveredHost(
            id: (id?.isEmpty == false) ? id! : name,
            name: name, host: address, port: port,
            fingerprintHex: fp, requiresPairing: pair == "required",
            allowsTofu: pair == "optional", macAddresses: macs,
            osChain: osChain, mgmtPort: mgmtPort)
    }

    private static func key(_ result: NWBrowser.Result) -> String {
        "\(result.endpoint)"
    }

    private static func instanceName(_ endpoint: NWEndpoint) -> String {
        if case let .service(name, _, _, _) = endpoint { return name }
        return "Punktfunk host"
    }

    private static func entry(_ txt: NWTXTRecord, _ field: String) -> String? {
        if case let .string(value) = txt.getEntry(for: field), !value.isEmpty { return value }
        return nil
    }

    /// A resolved `NWEndpoint.Host` → a plain address string for `PunktfunkConnection` (the
    /// scope id on a link-local address is stripped — the host+port pair is resolved again on
    /// the Rust side, which can't parse the `%iface` suffix).
    private static func hostString(_ host: NWEndpoint.Host) -> String? {
        switch host {
        case .ipv4(let address):
            return "\(address)".split(separator: "%").first.map(String.init)
        case .ipv6(let address):
            return "\(address)".split(separator: "%").first.map(String.init)
        case .name(let name, _):
            return name
        @unknown default:
            return nil
        }
    }
}
