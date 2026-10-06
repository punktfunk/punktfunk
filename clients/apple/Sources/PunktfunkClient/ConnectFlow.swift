// The connect, trust and wake flow behind every host tap, link and shelf. ContentView builds one
// per use over its model, stores and prompts; the prompts' state stays in the view.
//
// Ways to establish trust on first contact: the TOFU prompt (host fingerprint over the
// live-but-blurred stream, compared with the host's log; only for a host advertising pair=optional),
// the PIN pairing ceremony (verifies both sides at once), or — for a host that requires pairing —
// delegated approval ("Request Access": a plain identified connect the host parks until the operator
// approves this device in its console, no PIN). Once pinned, reconnects are silent and a changed
// host identity refuses to connect.

import Foundation
import PunktfunkKit
import SwiftUI

@MainActor
struct ConnectFlow {
    let model: SessionModel
    let store: HostStore
    let presets: PresetStore
    let discovery: HostDiscovery
    let waker: HostWaker
    /// Auto-wake on connect (Settings → General). On: a dial to an offline saved host fires
    /// Wake-on-LAN up front and falls into the "Waking…" wait if the dial fails. The explicit
    /// "Wake Host" action is unaffected either way.
    @Binding var autoWake: Bool
    /// A fresh `pair=required`/unknown host the user tapped: drives the choice between no-PIN
    /// delegated approval ("Request Access") and the SPAKE2 PIN ceremony (rule 3b).
    @Binding var approvalChoice: ApprovalRequest?
    /// A delegated-approval connect is in flight (host parks it until the operator approves):
    /// drives the cancelable "Waiting for approval" prompt and the pin-as-paired on success.
    @Binding var awaitingApproval: ApprovalRequest?

    /// `preset` is this connect's one-off pick ("Connect with ▸", a pinned card, a link's
    /// `preset=`). `.inherit` — the default, and what a plain card tap passes — falls through to
    /// the host's binding. A one-off NEVER rebinds the host: rebinding is always an explicit act
    /// in the edit sheet (design §5.2).
    func connect(
        _ host: StoredHost, launchID: String? = nil,
        preset: PresetSelection = .inherit, allowTofu: Bool? = nil,
        fromLibrary: Bool = false
    ) {
        // A pinned host dials on its stored fingerprint. An unpinned one may TOFU only when the
        // caller says so, or when its live advert says `pair=optional` (rule 3a); any other gets
        // the approval choice instead of a silent trust prompt (rules 3b + 4).
        if host.pinnedSHA256 == nil {
            let tofuOK = allowTofu ?? discovery.hosts.contains {
                host.matches($0) && $0.allowsTofu
            }
            if !tofuOK {
                // pair=required / unknown policy / manual entry (rule 3b): never a silent
                // connect — offer no-PIN delegated approval or the PIN ceremony.
                approvalChoice = ApprovalRequest(
                    host: host, advertisedFingerprint: advertisedFingerprint(for: host))
                return
            }
        }
        startSession(
            host, launchID: launchID, preset: preset, allowTofu: host.pinnedSHA256 == nil,
            fromLibrary: fromLibrary)
    }

    /// Resolve the stream mode + input prefs and hand off to the session model. The gamepad-type
    /// setting resolves NOW (Automatic → match the active physical controller): the host's virtual
    /// pad backend is fixed per session. `requestAccess` opens the no-PIN delegated-approval
    /// connect (host parks it until the operator approves).
    private func startSession(
        _ host: StoredHost, launchID: String? = nil,
        preset: PresetSelection = .inherit,
        allowTofu: Bool, requestAccess: Bool = false, approvalReq: ApprovalRequest? = nil,
        fromLibrary: Bool = false
    ) {
        // Dial the record as it stands NOW: a host that came back on a new DHCP lease was re-keyed
        // by the reachability check while we waited, and the value captured here is then stale.
        let go = {
            startSessionDirect(
                store.hosts.first { $0.id == host.id } ?? host,
                launchID: launchID, preset: preset, allowTofu: allowTofu,
                requestAccess: requestAccess, approvalReq: approvalReq,
                fromLibrary: fromLibrary)
        }
        // A host the probe calls down still gets the dial first: a routed host (VPN, another
        // subnet) answers without advertising. `prepareWake` already sent the magic packet, so
        // only a failed dial falls into the visible "Waking…" wait, which redials once it answers.
        if autoWake, PunktfunkConnection.wakeOnLANAvailable,
           !host.wakeMacs.isEmpty, !store.probedOnline.contains(host.id) {
            discovery.start() // so the wake-wait can pick up a host that moved address
            startSessionDirect(
                host, launchID: launchID, preset: preset, allowTofu: allowTofu,
                requestAccess: requestAccess, approvalReq: approvalReq, fromLibrary: fromLibrary,
                onUnreachable: {
                    waker.start(
                        host: host, connectsAfter: true, macs: host.wakeMacs, lastIP: host.address,
                        isOnline: { await store.isReachable(host, discovery: discovery) },
                        onOnline: go)
                })
        } else {
            go()
        }
    }

    /// The actual dial — reached directly when the host is awake, or from the waker once a woken
    /// host is back online. `prepareWake` still runs here to LEARN/refresh the MAC now that the host
    /// is advertising (and is a harmless no-op otherwise). `onUnreachable` hands a plain connect
    /// failure back to the caller (the wake-wait fallback) instead of the error alert.
    private func startSessionDirect(
        _ host: StoredHost, launchID: String? = nil,
        preset: PresetSelection = .inherit,
        allowTofu: Bool, requestAccess: Bool = false, approvalReq: ApprovalRequest? = nil,
        fromLibrary: Bool = false,
        onUnreachable: (@MainActor () -> Void)? = nil
    ) {
        prepareWake(for: host)
        // The delegated-approval wait prompt only makes sense once we're actually dialing — set it
        // here (after any wake), not before, so it never stacks under the "Waking…" overlay.
        if let approvalReq { awaitingApproval = approvalReq }
        // THE resolution point (design §4.4): the globals plus this connect's preset, once, here.
        // The model latches the result for the whole session, so nothing downstream can end up
        // applying a preset to half of it.
        let effective = EffectiveSettings.resolve(
            host: host, selection: preset, launch: launchID, catalog: presets.catalog)
        model.connect(
            to: host,
            effective: effective,
            gamepad: GamepadManager.shared.resolveType(
                setting: PunktfunkConnection.GamepadType(
                    rawValue: UInt32(clamping: effective.gamepadType)) ?? .auto),
            launchID: launchID,
            // Where this session goes back to when it ends: the shelf it started from — the
            // host's own, or the pinned card whose preset it is using. nil for a connect that
            // did NOT come off a shelf, which is what keeps a plain host-list connect ending on
            // the host list.
            shelf: launchID != nil || fromLibrary
                ? LibraryTarget(host: host, preset: preset) : nil,
            allowTofu: allowTofu,
            requestAccess: requestAccess,
            onUnreachable: onUnreachable)
    }

    /// Learn-while-awake, wake-while-asleep — run just before every connect:
    ///  • an advert matches this host → refresh the MAC(s), OS chain and mgmt port it publishes, so
    ///    a later wake has an up-to-date target and the library keeps working once this device can
    ///    no longer see the advert (VPN, routed subnet, multicast-dead Wi-Fi);
    ///  • the probe did NOT reach it and we have MAC(s) → fire a magic packet first. The two are
    ///    independent: a sleeping host keeps advertising for up to 75 minutes, so a live advert is
    ///    no reason to withhold the packet. Best-effort and non-blocking (the send is off-main).
    private func prepareWake(for host: StoredHost) {
        if let live = discovery.hosts.first(where: { host.matches($0) }) {
            store.updateMacs(host.id, macs: live.macAddresses) // learn — on every platform
            store.updateOsChain(host.id, chain: live.osChain) // ditto for the card's OS mark
            store.updateMgmtPort(host.id, port: live.mgmtPort)
        }
        // Auto-wake only. With it off, connects go straight through (no packet).
        if autoWake, PunktfunkConnection.wakeOnLANAvailable, !host.wakeMacs.isEmpty,
           !store.probedOnline.contains(host.id) {
            let macs = host.wakeMacs
            let ip = host.address
            DispatchQueue.global(qos: .userInitiated).async {
                PunktfunkConnection.wakeOnLAN(macs: macs, lastKnownIP: ip)
            }
        }
    }

    /// The no-PIN delegated-approval flow: open an identified connect the host parks until the
    /// operator approves it in the console, showing the cancelable "Waiting for approval" prompt
    /// meanwhile. On success the SAME connection is admitted (no reconnect) and ContentView pins
    /// the host as paired when the session starts streaming.
    func requestAccess(_ req: ApprovalRequest) {
        guard !model.isBusy else { return }
        // Pin the advertised certificate for a discovered host (impostor defence during the long
        // wait); a manually-typed host has no advertised fingerprint, so trust-on-first-use.
        var host = req.host
        host.pinnedSHA256 = req.advertisedFingerprint
        // `awaitingApproval` is set inside startSessionDirect (after any wake), so it never stacks
        // under the "Waking…" overlay.
        startSession(host, allowTofu: false, requestAccess: true, approvalReq: req)
    }

    /// Explicit wake-only (the touch card's "Wake Host" menu item / a future gamepad action): fire
    /// the packet and wait for the host to come online, but don't connect — the user then sees it
    /// go online and can connect.
    func wakeOnly(_ host: StoredHost) {
        guard PunktfunkConnection.wakeOnLANAvailable, !host.wakeMacs.isEmpty else { return }
        discovery.start()
        waker.start(
            host: host, connectsAfter: false, macs: host.wakeMacs, lastIP: host.address,
            isOnline: { await store.isReachable(host, discovery: discovery) }, onOnline: {})
    }

    /// Tap a discovered host: save it (so the session has a stored identity and the trust pin
    /// persists), then connect or pair per the host's advertised policy. The host is the policy
    /// authority — TOFU is offered ONLY when it explicitly advertised `pair=optional` (rule 3a);
    /// a `pair=required` host, or one with no/unknown `pair` field, gets the approval choice
    /// (request access / pair with PIN) (rule 3b). (A pinned discovered host connects silently
    /// inside `connect`.)
    func connectDiscovered(_ d: DiscoveredHost) {
        guard !model.isBusy else { return }
        let host = save(d)
        if d.allowsTofu {
            connect(host, allowTofu: true)
        } else {
            // pair=required / unknown policy (rule 3b): offer no-PIN delegated approval or PIN.
            approvalChoice = ApprovalRequest(
                host: host, advertisedFingerprint: pinFingerprint(d.fingerprintHex))
        }
    }

    /// A discovered host as a saved record, so a session has a stored identity to pin.
    private func save(_ d: DiscoveredHost) -> StoredHost {
        let host = StoredHost(
            name: d.name, address: d.host, port: d.port,
            mgmtPort: d.mgmtPort,
            macAddresses: d.macAddresses.isEmpty ? nil : d.macAddresses,
            osChain: d.osChain.isEmpty ? nil : d.osChain)
        store.add(host)
        return host
    }

    /// The console Pair screen's "Request access" on a saved host: keep its pin, else take
    /// the one its advert carries.
    func consoleRequestAccess(_ host: StoredHost) {
        requestAccess(
            ApprovalRequest(
                host: host,
                advertisedFingerprint: host.pinnedSHA256 ?? advertisedFingerprint(for: host)))
    }

    /// The same on a host the console only saw advertised: saved first, as a tap on it would.
    func requestAccessDiscovered(_ d: DiscoveredHost) {
        guard !model.isBusy else { return }
        requestAccess(
            ApprovalRequest(host: save(d), advertisedFingerprint: pinFingerprint(d.fingerprintHex)))
    }

    /// The certificate fingerprint a live mDNS advert carries for this saved host (advisory — see
    /// `HostDiscovery`), to pin during a delegated-approval wait. nil if the host isn't currently
    /// advertising or advertised no/invalid `fp`.
    private func advertisedFingerprint(for host: StoredHost) -> Data? {
        pinFingerprint(discovery.hosts.first { host.matches($0) }?.fingerprintHex)
    }

    /// Parse an advertised cert fingerprint (lowercase hex) into the 32-byte pin the connect
    /// expects; nil unless it's exactly a 32-byte (SHA-256) value, so a malformed advert falls
    /// back to trust-on-first-use rather than failing the connect closed.
    private func pinFingerprint(_ hex: String?) -> Data? {
        guard let hex, let data = Data(hexString: hex), data.count == 32 else { return nil }
        return data
    }
}
