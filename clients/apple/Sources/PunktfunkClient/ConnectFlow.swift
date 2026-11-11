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
    /// The profile picker a connect is waiting on.
    @Binding var profileAsk: ProfileAsk?
    /// The wait for the picked profile's seat to come up.
    @Binding var seatWait: SeatWait?
    /// The password prompt of a profile on a person's own account.
    @Binding var unlockAsk: UnlockAsk?

    /// Which profile a connect plays as.
    enum ProfileChoice {
        /// Ask the paired host first; `link` is a link's `as=`, which wins for this connect.
        case ask(link: String? = nil)
        /// Already decided (the console's own picker): dial with this id, nil sends none, with
        /// the password the console's prompt typed. `console` takes back what the console asks.
        case send(String?, unlock: ProfileUnlock? = nil, console: ConsoleDial? = nil)
    }

    /// What a dial the console issued reports to the console instead of the app's own prompts.
    struct ConsoleDial {
        /// The host closed with `ProfileLocked` and this sentence: the console's prompt reopens.
        let locked: @MainActor (String?) -> Void
        /// The host holds the unlock dial while the profile's desktop starts.
        let starting: @MainActor () -> Void
    }

    /// The profile a dial plays as, and what opens it. `prompts`: a `ProfileLocked` close opens
    /// the password prompt rather than an error; `console` sends it to the console's instead.
    struct ProfileDial {
        var id: String?
        var name: String?
        var unlock: ProfileUnlock?
        var prompts = false
        var console: ConsoleDial?
    }

    /// `preset` is this connect's one-off pick ("Connect with ▸", a pinned card, a link's
    /// `preset=`). `.inherit` — the default, and what a plain card tap passes — falls through to
    /// the host's binding. A one-off NEVER rebinds the host: rebinding is always an explicit act
    /// in the edit sheet (design §5.2). `fromLink`: a pairing on the way resumes this connect.
    /// `ConnectPlan.door` picks the way in.
    func connect(
        _ host: StoredHost, launchID: String? = nil,
        preset: PresetSelection = .inherit, allowTofu: Bool? = nil,
        fromLibrary: Bool = false, profile: ProfileChoice = .ask(), fromLink: Bool = false
    ) {
        let door = ConnectPlan.door(
            pinned: host.pinnedSHA256 != nil, allowTofu: allowTofu,
            advertsTofu: discovery.hosts.contains { host.matches($0) && $0.allowsTofu },
            advertisedFingerprint: advertisedFingerprint(for: host), fromLink: fromLink,
            isDemo: DemoMode.isDemo(host))
        switch door {
        case .approvalChoice(let fingerprint, let resumes):
            var then: (@MainActor (StoredHost) -> Void)?
            if resumes {
                let flow = self
                then = { pinned in
                    flow.connect(
                        pinned, launchID: launchID, preset: preset,
                        fromLibrary: fromLibrary, profile: profile)
                }
            }
            approvalChoice = ApprovalRequest(
                host: host, advertisedFingerprint: fingerprint, thenConnect: then)
        case .dial(let allowTofu, let asksProfile):
            switch profile {
            case .send(let id, let unlock, let console):
                // The console asks for the password itself. Only a pinned host is sent one.
                startSession(
                    host, launchID: launchID, preset: preset, allowTofu: allowTofu,
                    fromLibrary: fromLibrary,
                    profile: ProfileDial(id: id, unlock: allowTofu ? nil : unlock, console: console))
            case .ask(let link) where asksProfile:
                askProfile(host, link: link) { profile in
                    startSession(
                        host, launchID: launchID, preset: preset, allowTofu: allowTofu,
                        fromLibrary: fromLibrary, profile: profile)
                }
            case .ask:
                startSession(
                    host, launchID: launchID, preset: preset, allowTofu: allowTofu,
                    fromLibrary: fromLibrary)
            }
        }
    }

    /// Ask the host who is playing, then run `go` with the profile to dial as and what opens it.
    /// `ConnectPlan.afterProfiles` decides what follows the list.
    private func askProfile(
        _ host: StoredHost, link: String?, go: @escaping @MainActor (ProfileDial) -> Void
    ) {
        let saved = (store.hosts.first { $0.id == host.id } ?? host).pickedProfile
        Task { @MainActor in
            let answer = await ProfileFetch.list(host, within: ProfileFetch.wait)
            let rows = answer.rows
            let unlocked: @MainActor (String?) -> Void = { id in
                guard let row = rows?.first(where: { $0.id == id }) else {
                    go(ProfileDial(id: id, prompts: true))
                    return
                }
                unlock(host, row) { unlock in
                    go(ProfileDial(id: id, name: row.displayName, unlock: unlock, prompts: true))
                }
            }
            switch ConnectPlan.afterProfiles(
                rows: rows, failed: answer.failed, saved: saved, link: link)
            {
            case .dial(let id, let name):
                go(ProfileDial(id: id, name: name, prompts: true))
            case .settle(let id, let remember):
                store.setProfile(host.id, remember)
                settleSeat(host, rows: rows, id: id, go: unlocked)
            case .pick(let rows, let saved, let gone):
                profileAsk = ProfileAsk(
                    host: host, content: .choose(rows, saved: saved, gone: gone),
                    pick: { pick in
                        store.setProfile(host.id, pick)
                        settleSeat(host, rows: rows, id: pick.id, afterSheet: true, go: unlocked)
                    })
            }
        }
    }

    /// Run `then` with what opens `row`: nothing, its key, or the password the player typed. A
    /// cancelled prompt runs nothing. An unpinned host gets no unlock.
    private func unlock(
        _ host: StoredHost, _ row: ListedProfile,
        then: @escaping @MainActor (ProfileUnlock?) -> Void
    ) {
        guard row.unlock != .none, let unlocker = unlocker(for: host) else {
            then(nil)
            return
        }
        let prompt = passwordPrompt(host, name: row.displayName, unlocker: unlocker)
        Task { @MainActor in
            if case .dial(let unlock) = await unlocker.before(row, prompt: prompt) { then(unlock) }
        }
    }

    /// The prompt after a `ProfileLocked` close, with the host's sentence; then the dial again.
    /// False when there is nothing to prompt for: the caller shows the error.
    private func reprompt(
        _ host: StoredHost, _ profile: ProfileDial, said: String?,
        dial: @escaping @MainActor (ProfileDial) -> Void
    ) -> Bool {
        guard profile.prompts, let id = profile.id, let unlocker = unlocker(for: host) else {
            return false
        }
        let prompt = passwordPrompt(host, name: profile.name ?? id, unlocker: unlocker)
        Task { @MainActor in
            guard case .dial(let unlock) = await unlocker.refused(
                profile: id, said: said, prompt: prompt)
            else { return }
            var next = profile
            next.unlock = unlock
            dial(next)
        }
        return true
    }

    /// The key and prompt rules for `host`: only a pinned host is sent an unlock.
    private func unlocker(for host: StoredHost) -> ProfileUnlocker? {
        guard let pin = host.pinnedSHA256,
              case .success(let target) = MgmtTarget.make(host: host)
        else { return nil }
        #if os(tvOS)
        let keys: (any UnlockKeys)? = nil
        #else
        let keys: (any UnlockKeys)? = EnclaveUnlockKeys()
        #endif
        return ProfileUnlocker(
            keys: keys, host: pin,
            device: (try? ClientIdentityStore.shared.load())?.identity.fingerprint,
            hostName: host.displayName
        ) { id in
            try await LibraryClient.unlockChallenge(id: id, on: target)
        }
    }

    /// Shows the password prompt and answers what the player entered.
    private func passwordPrompt(
        _ host: StoredHost, name: String, unlocker: ProfileUnlocker
    ) -> ProfileUnlocker.Prompt {
        let ask = $unlockAsk
        let (hostName, switchTitle) = (host.displayName, unlocker.switchTitle)
        return { error in
            await withCheckedContinuation { done in
                Task { @MainActor in
                    // A picker or seat sheet may still be closing, and a sheet shown over it drops.
                    try? await Task.sleep(nanoseconds: 400_000_000)
                    ask.wrappedValue = UnlockAsk(
                        hostName: hostName, profileName: name, error: error,
                        switchTitle: switchTitle
                    ) { done.resume(returning: $0) }
                }
            }
        }
    }

    /// Run `go` with `id` once its seat can take the connect: now for a profile without a seat
    /// or one that is up, after a wake and a wait for one that is not, never for one that
    /// can't play. `afterSheet`: the picker is still closing, so a sheet or alert waits for it.
    private func settleSeat(
        _ host: StoredHost, rows: [ListedProfile]?, id: String?, afterSheet: Bool = false,
        go: @escaping @MainActor (String?) -> Void
    ) {
        guard let id, let row = rows?.first(where: { $0.id == id }) else {
            go(id)
            return
        }
        let gate = HostProfiles.seatGate(row)
        if gate == .dial {
            go(id)
            return
        }
        Task { @MainActor in
            if afterSheet { try? await Task.sleep(nanoseconds: 400_000_000) }
            switch gate {
            case .dial: go(id)
            case .refuse(let line): model.errorMessage = line
            case .wake, .wait: waitForSeat(host, row, wake: gate == .wake, go: go)
            }
        }
    }

    /// The sheet while a seat comes up: wake it when stopped, then read the list every 2 s, each
    /// read stepped by `ConnectPlan.seatStep`. Closing the sheet cancels; there is no timeout.
    private func waitForSeat(
        _ host: StoredHost, _ first: ListedProfile, wake: Bool,
        go: @escaping @MainActor (String?) -> Void
    ) {
        var detail: String?
        if case .wait(let line) = HostProfiles.seatGate(first) { detail = line }
        let wait = SeatWait(title: HostProfiles.wakingLine(first.displayName), detail: detail)
        seatWait = wait
        wait.task = Task { @MainActor in
            if wake {
                do {
                    try await ProfileFetch.wake(host, id: first.id)
                } catch {
                    // A cancelled wait says nothing and leaves a newer wait's sheet alone.
                    guard !Task.isCancelled, seatWait === wait else { return }
                    seatWait = nil
                    model.errorMessage = LibraryClient.wakeFailedLine(first.displayName, error)
                    return
                }
            }
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 2_000_000_000)
                let answer = await ProfileFetch.list(host, within: ProfileFetch.wait)
                // The sheet closed meanwhile: that is Cancel.
                guard !Task.isCancelled, seatWait === wait else { return }
                switch ConnectPlan.seatStep(rows: answer.rows, failed: answer.failed, id: first.id) {
                case .dial:
                    seatWait = nil
                    go(first.id)
                    return
                case .refuse(let line):
                    seatWait = nil
                    model.errorMessage = line
                    return
                case .keepWaiting(let detail): wait.detail = detail
                case .retry: continue
                }
            }
        }
    }

    /// Resolve the stream mode + input prefs and hand off to the session model. The gamepad-type
    /// setting resolves NOW (Automatic → match the active physical controller): the host's virtual
    /// pad backend is fixed per session. `requestAccess` opens the no-PIN delegated-approval
    /// connect (host parks it until the operator approves). Only the first dial carries the
    /// profile's unlock; the waker's redial goes without it.
    private func startSession(
        _ host: StoredHost, launchID: String? = nil,
        preset: PresetSelection = .inherit,
        allowTofu: Bool, requestAccess: Bool = false, approvalReq: ApprovalRequest? = nil,
        fromLibrary: Bool = false, profile: ProfileDial = ProfileDial()
    ) {
        var redial = profile
        redial.unlock = nil
        // Dial the record as it stands NOW: a host that came back on a new DHCP lease was re-keyed
        // by the reachability check while we waited, and the value captured here is then stale.
        let go = {
            startSessionDirect(
                store.hosts.first { $0.id == host.id } ?? host,
                launchID: launchID, preset: preset, allowTofu: allowTofu,
                requestAccess: requestAccess, approvalReq: approvalReq,
                fromLibrary: fromLibrary, profile: redial)
        }
        // A host the probe calls down still gets the dial first: a routed host (VPN, another
        // subnet) answers without advertising. `prepareWake` already sent the magic packet, so
        // only a failed dial falls into the visible "Waking…" wait, which redials once it answers.
        if wakesFirst(host) {
            discovery.start() // so the wake-wait can pick up a host that moved address
            startSessionDirect(
                host, launchID: launchID, preset: preset, allowTofu: allowTofu,
                requestAccess: requestAccess, approvalReq: approvalReq, fromLibrary: fromLibrary,
                profile: profile,
                onUnreachable: {
                    waker.start(
                        host: host, connectsAfter: true, macs: host.wakeMacs, lastIP: host.address,
                        isOnline: { await store.isReachable(host, discovery: discovery) },
                        onOnline: go)
                })
        } else {
            startSessionDirect(
                store.hosts.first { $0.id == host.id } ?? host,
                launchID: launchID, preset: preset, allowTofu: allowTofu,
                requestAccess: requestAccess, approvalReq: approvalReq, fromLibrary: fromLibrary,
                profile: profile)
        }
    }

    /// The actual dial — reached directly when the host is awake, or from the waker once a woken
    /// host is back online. `prepareWake` still runs here to LEARN/refresh the MAC now that the host
    /// is advertising (and is a harmless no-op otherwise). `onUnreachable` hands a plain connect
    /// failure back to the caller (the wake-wait fallback) instead of the error alert. A dial the
    /// console issued hands a `ProfileLocked` close and an unlock's wait back to the console.
    private func startSessionDirect(
        _ host: StoredHost, launchID: String? = nil,
        preset: PresetSelection = .inherit,
        allowTofu: Bool, requestAccess: Bool = false, approvalReq: ApprovalRequest? = nil,
        fromLibrary: Bool = false, profile: ProfileDial = ProfileDial(), redialed: Bool = false,
        onUnreachable: (@MainActor () -> Void)? = nil
    ) {
        let profileID = profile.id
        prepareWake(for: host)
        // The delegated-approval wait prompt only makes sense once we're actually dialing — set it
        // here (after any wake), not before, so it never stacks under the "Waking…" overlay.
        if let approvalReq { awaitingApproval = approvalReq }
        launchOverrides(host, launchID) { overrides in
            // THE resolution point (design §4.4): the globals plus this connect's preset, once,
            // here, then the host's overrides for the game. The model latches the result for the
            // whole session, so nothing downstream can apply a preset to half of it.
            var effective = EffectiveSettings.resolve(
                host: host, selection: preset, launch: launchID, catalog: presets.catalog)
            if let hdr = overrides?.hdr { effective.hdrEnabled = hdr }
            if let pad = overrides?.gamepad.flatMap(PunktfunkConnection.GamepadType.init(name:)) {
                effective.gamepadType = Int(pad.rawValue)
            }
            model.connect(
                to: host,
                effective: effective,
                gamepad: GamepadManager.shared.resolveType(
                    setting: PunktfunkConnection.GamepadType(
                        rawValue: UInt32(clamping: effective.gamepadType)) ?? .auto),
                overrides: overrides,
                launchID: launchID,
                profileID: profileID,
                // Where this session goes back to when it ends: the shelf it started from — the
                // host's own, or the pinned card whose preset it is using. nil for a connect that
                // did NOT come off a shelf, which is what keeps a plain host-list connect ending on
                // the host list.
                shelf: launchID != nil || fromLibrary
                    ? LibraryTarget(host: host, preset: preset) : nil,
                allowTofu: allowTofu,
                requestAccess: requestAccess,
                autoWake: autoWake,
                onProfileUnknown: {
                    // One re-dial when the list still has the profile: a seat host refused a stale
                    // seat. Otherwise forget the pick, so the next connect asks.
                    guard !redialed, let profileID else {
                        store.setProfile(host.id, nil)
                        return false
                    }
                    Task { @MainActor in
                        let answer = await ProfileFetch.list(host, within: ProfileFetch.wait)
                        guard case .listed(let rows?) = answer,
                              rows.contains(where: { $0.id == profileID }) else {
                            store.setProfile(host.id, nil)
                            model.errorMessage = L10n.dialHostReason(
                                host: host.displayName,
                                reason: HostRejection.profileUnknown.userMessage)
                            return
                        }
                        settleSeat(host, rows: rows, id: profileID) { id in
                            var next = profile
                            next.id = id
                            next.unlock = nil
                            startSessionDirect(
                                store.hosts.first { $0.id == host.id } ?? host,
                                launchID: launchID, preset: preset, allowTofu: allowTofu,
                                requestAccess: requestAccess, approvalReq: approvalReq,
                                fromLibrary: fromLibrary, profile: next, redialed: true,
                                onUnreachable: onUnreachable)
                        }
                    }
                    return true
                },
                unlock: profile.unlock,
                onUnlockPending: profile.console?.starting,
                onProfileLocked: { said in
                    if let console = profile.console {
                        console.locked(said)
                        return true
                    }
                    return reprompt(host, profile, said: said) { next in
                        startSessionDirect(
                            store.hosts.first { $0.id == host.id } ?? host,
                            launchID: launchID, preset: preset, allowTofu: allowTofu,
                            fromLibrary: fromLibrary, profile: next, redialed: redialed,
                            onUnreachable: onUnreachable)
                    }
                },
                onUnreachable: onUnreachable)
        }
    }

    /// Runs `go` with the launched game's overrides: a tile's entry at once (the hold mounts in
    /// this turn), else the cached catalog's once read. A title never fetched here gets nil and
    /// dials as before; the host still lowers the mode and swaps the pad.
    private func launchOverrides(
        _ host: StoredHost, _ launchID: String?,
        go: @escaping @MainActor (GameOverrides?) -> Void
    ) {
        guard let launchID else { return go(nil) }
        if let entry = LaunchedEntry.peek(launchID) { return go(entry.overrides) }
        guard let cache = LibraryCache.shared else { return go(nil) }
        Task { @MainActor in
            let cached = await cache.load(hostID: host.id.uuidString)
            go(cached?.games.first { $0.id == launchID }?.overrides)
        }
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
        if wakesFirst(host) {
            let macs = host.wakeMacs
            let ip = host.address
            DispatchQueue.global(qos: .userInitiated).async {
                PunktfunkConnection.wakeOnLAN(macs: macs, lastKnownIP: ip)
            }
        }
    }

    private func wakesFirst(_ host: StoredHost) -> Bool {
        WakePolicy.wakesFirst(
            host.wakeMacs, autoWake: autoWake, probedOnline: store.probedOnline.contains(host.id))
    }

    /// The no-PIN delegated-approval flow: ask the host, which parks the request until the
    /// operator approves it in the console, under the cancelable "Waiting for approval" prompt.
    /// Approval pins the host as paired and streams only for a link. The advertised certificate
    /// is the pin (impostor defence during the long wait); a typed host has none, so first use.
    func requestAccess(_ req: ApprovalRequest) {
        guard !model.isBusy else { return }
        awaitingApproval = req
        let (store, model, awaiting) = (store, model, $awaitingApproval)
        let (address, port, pin) = (req.host.address, req.host.port, req.advertisedFingerprint)
        Task { @MainActor in
            let result = await Task.detached(priority: .userInitiated) {
                Result {
                    let identity = try ClientIdentityStore.shared.loadForPairing()
                    return try PunktfunkKit.requestAccess(
                        host: address, port: port, identity: identity, pinSHA256: pin,
                        name: DeviceName.current)
                }
            }.value
            // Cancelled, or a later request owns the prompt.
            guard awaiting.wrappedValue?.token == req.token else { return }
            awaiting.wrappedValue = nil
            switch result {
            case .success(let fingerprint):
                store.pin(req.host.id, fingerprint: fingerprint)
                var pinned = req.host
                pinned.pinnedSHA256 = fingerprint
                req.thenConnect?(pinned)
            case .failure(let error):
                model.errorMessage = ConnectOffer.failureMessage(
                    error, hostName: req.host.displayName, pinned: false, requestAccess: true,
                    callerRecovers: false)
            }
        }
    }

    /// Explicit wake-only (a host card's or the library's "Wake Host"): fire the packet and wait
    /// for the host to come online, then run `onOnline`, but don't connect.
    func wakeOnly(_ host: StoredHost, onOnline: @escaping () -> Void = {}) {
        guard WakePolicy.canWake(host.wakeMacs) else { return }
        discovery.start()
        waker.start(
            host: host, connectsAfter: false, macs: host.wakeMacs, lastIP: host.address,
            isOnline: { await store.isReachable(host, discovery: discovery) }, onOnline: onOnline)
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
        let host = StoredHost(advert: d)
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

extension ProfileAnswer {
    /// The list, nil for a box without profiles and for a failed answer.
    fileprivate var rows: [ListedProfile]? { if case .listed(let rows) = self { rows } else { nil } }
    fileprivate var failed: Bool { if case .failed = self { true } else { false } }
}
