// Captured Steam Controller 2 pads: the glue between a transport link and the punktfunk wire.
// Exactly one transport runs (`startTransport`): `Sc2UsbLink` for a wired pad or a Puck (macOS
// only), else `Sc2BleLink`. Running both would double-feed a pad that is charging and paired.
//
// Each HID source (a USB collection or the BLE link) is its own `PadSource` with its own wire
// slot, so a Puck with four pads claims four. A slot is claimed on the source's first state report
// (arrival pref 9, or 10 for a Puck slot) and released on link drop, suspend, stop or a Puck
// disconnect; the index comes from `GamepadManager.reserveExternalPadIndex()`.
//
// - Raw plane: every report goes to core (`sendHidReport`), which gates it (`setSc2Gate`).
// - Typed mirror: buttons, sticks and triggers also go out typed, for a host that degraded the kind.
// - Local chords: escape, stats and the ring read the HARDWARE mask, so no gate hides the way out.
// - Raw return: host writes arrive through `GamepadFeedback` → `onHidRaw` → the link.
//
// macOS also shows the pad to GameController; `syncShadowSuppression` drops that twin only while a
// slot is claimed. Reports arrive on the link queue, host writes on the feedback thread, lifecycle
// on the main actor; all mutable state sits behind `lock`.

#if !os(macOS)
import UIKit
#endif
import Foundation

private let log = ClientLog(category: "gamepad")

public final class Sc2Capture {
    private let connection: PunktfunkConnection
    private let manager: GamepadManager
    /// The link delegate/report queue — USER_INTERACTIVE (SDL's warning: BLE packets are
    /// silently dropped if the consumer stalls).
    private let queue = DispatchQueue(
        label: "io.unom.punktfunk.sc2-ble",
        qos: .userInteractive)
    private var link: Sc2BleLink!
    #if os(macOS)
    /// The USB transport — wired pads or a Puck dongle. macOS only: IOKit HID device access is
    /// not available to apps on iOS, which is why BLE remains the only iOS transport.
    private var usbLink: Sc2UsbLink!
    #endif
    /// Which transport `start` engaged. Exactly one runs at a time, deliberately: a single pad
    /// can be BLE-paired AND plugged in (charging), and running both links would stream the same
    /// controller onto two wire slots, doubling every input.
    private enum Transport { case none, ble, usb }
    private var observers: [NSObjectProtocol] = []

    /// The BLE link's source key. The USB link keys sources by IOKit registry entry id, which
    /// is never 0 for a real device, so the spaces cannot collide.
    private static let bleSource: UInt64 = 0

    /// Everything one physical pad owns: its wire slot, typed-mirror diff state and escape-chord
    /// timer. One per live HID source, in `sources`.
    /// All fields are guarded by the capture's `lock`.
    private final class PadSource {
        var padIndex: UInt8?
        var claimPending = false
        /// What the HOST has been told is held (wire layout) — the typed mirror's diff state,
        /// gated, so it is 0 for anything the ring swallowed.
        var wireButtons: UInt32 = 0
        /// This pad's local chords and ring state, including the ungated hardware mask the
        /// escape chord reads.
        let ring = Sc2RingGate()
        var lastAxis = [Int32](repeating: Int32.min, count: 6)
        /// Reusable up-path buffer: the gated report is copied in and sent from here, so the
        /// raw plane costs no per-report allocation beyond the link's own framing.
        var rawBuf = [UInt8](repeating: 0, count: 64)
        /// Armed while the escape chord is held (fires `onDisconnectRequest` on main).
        var chordWork: DispatchWorkItem?
    }

    /// Guards every field below (see the threading note in the header).
    private let lock = NSLock()
    /// Written on the main actor (`startTransport`/`stopTransport`), read on the link queue and
    /// the feedback drain thread — which is why it sits under `lock` with the rest.
    private var transport: Transport = .none
    /// Per-source pad state, keyed by the link's source id (`bleSource` for BLE, the IOKit
    /// registry entry id per USB collection). Entries materialize on a source's first report
    /// and die with `releaseSource`.
    private var sources: [UInt64: PadSource] = [:]
    private var stopped = false
    /// iOS/tvOS app inactive → BLE is released and the slot freed; resume re-acquires (the
    /// recommended backgrounding behavior for a CoreBluetooth central). Never set on macOS.
    private var suspended = false

    /// The cross-client controller escape chord, read off this capture's own hardware mask —
    /// MUST stay equal to `GamepadCapture.escapeChord` (pinned by `Sc2EscapeChordMirrorTests`;
    /// re-declared here because the original is main-actor-isolated and this class reads the
    /// mask on the BLE queue). Held `disconnectHold` it ends the session, so a captured SC2 —
    /// whose raw feed bypasses GamepadCapture entirely — can still exit the stream.
    static let escapeChord: UInt32 =
        GamepadWire.leftShoulder | GamepadWire.rightShoulder | GamepadWire.start | GamepadWire.back
    /// pf-client-core's `DISCONNECT_HOLD` — the same 1.5 s on every client (and the same value
    /// as GamepadCapture's private `disconnectHold`; the mirror test pins it).
    static let disconnectHold: TimeInterval = 1.5

    /// Fired ON MAIN once the escape chord has been held `disconnectHold` — the session owner
    /// disconnects (same contract as `GamepadCapture.onDisconnectRequest`).
    public var onDisconnectRequest: (() -> Void)?

    /// The capture's claim/release edges, for the stream surface. `captured` fires whenever a
    /// wire slot lands (the host is building a virtual SC2 — at stream start, or whenever a
    /// pad powers on mid-session); `released` only when the LAST claimed slot goes away, so a
    /// badge over a multi-pad Puck does not flicker off while other pads are still captured.
    /// Delivered ON MAIN, like `onDisconnectRequest` — the capture otherwise leaves no UI
    /// trace at all, since the device never enters the GameController world the Controllers
    /// page lists.
    public enum Phase: Equatable {
        case captured(pad: UInt8)
        case released
    }
    /// Fired ON MAIN on `Phase` edges — the session owner surfaces the passthrough badge.
    public var onPhaseChange: ((Phase) -> Void)?

    /// `Select+A` on a captured pad: the quick-action ring's opener, the same contract as
    /// `GamepadCapture.onRingChord`. Fired ON MAIN, on the press that completes the chord.
    public var onRingChord: (() -> Void)?
    /// A press while the ring owns the pad (`ringOpen`) — fired ON MAIN.
    public var onRingNav: ((RingNav) -> Void)?
    /// The ring is up: the pad drives it, and the host sees a neutral report at the same cadence
    /// instead — the raw plane's answer to GamepadCapture's flush, which a frozen last frame (a
    /// held sprint surviving a menu) is exactly what it must avoid. Set from the session owner
    /// on main; read on the link queue, hence the lock.
    public var ringOpen: Bool {
        get {
            lock.lock()
            defer { lock.unlock() }
            return ringOpenLocked
        }
        set {
            lock.lock()
            ringOpenLocked = newValue
            // Every live pad, plus whatever arrives while the dial is up (`sourceLocked`).
            for src in sources.values {
                src.ring.ringOpen = newValue
                if let pad = src.padIndex { pushGateLocked(pad: pad) }
            }
            lock.unlock()
        }
    }
    private var ringOpenLocked = false

    /// Core holds back of this pad's raw reports what the ring gate holds back of the typed
    /// plane. Steam and QAM always go to the host's Steam. Caller holds `lock`.
    private func pushGateLocked(pad: UInt8) {
        connection.setSc2Gate(
            pad: pad, masked: ringOpenLocked, systemLocal: false, chords: onRingChord != nil)
    }

    public init(connection: PunktfunkConnection, manager: GamepadManager) {
        self.connection = connection
        self.manager = manager
        link = Sc2BleLink(
            queue: queue,
            onReport: { [weak self] report in
                self?.handleReport(source: Sc2Capture.bleSource, report)
            },
            onClosed: { [weak self] in
                // Controller powered off / out of range. Release the slot (the punktfunk
                // analogue of "Steam sees a REAL disconnect": the host tears down /
                // neutralizes its virtual pad) — the link keeps re-acquiring on its own 2 s
                // poll, and the next connection re-claims + re-proves its IMU live.
                self?.releaseSource(Sc2Capture.bleSource, reason: "link closed")
            })
        #if os(macOS)
        usbLink = Sc2UsbLink(
            queue: queue,
            onReport: { [weak self] source, report in
                self?.handleReport(source: source, report)
            },
            onSourceClosed: { [weak self] source in
                self?.releaseSource(source, reason: "usb collection removed")
            })
        #endif
    }

    /// The live transport, safe from any thread. Never call with `lock` held (NSLock does not
    /// re-enter).
    private var currentTransport: Transport {
        lock.lock()
        defer { lock.unlock() }
        return transport
    }

    /// Whether `source` is a Puck-dongle collection. Decides the declared wire kind AND whether
    /// wireless-status reports may be acted on — see `handleWireless`. Safe from any thread;
    /// never call with `lock` held.
    private func isDongleSource(_ source: UInt64) -> Bool {
        #if os(macOS)
        return source != Self.bleSource && usbLink.isDongle(source: source)
        #else
        return false
        #endif
    }

    /// Hold the SC2 twins back before `GamepadCapture` starts forwarding. On a Mac that can open
    /// an attached USB SC2 the claim lands within a second (the identity read included); its twin
    /// would otherwise reach the host first as an Xbox pad and push the SC2 to the next pad index.
    @MainActor
    public static func holdTwins(manager: GamepadManager) {
        #if os(macOS)
        guard Sc2UsbLink.canOpenAttached() else { return }
        manager.holdSteamController2Twins(for: 3)
        #endif
    }

    /// The one capture that holds the controller: a second would open the same link twice.
    @MainActor private static weak var running: Sc2Capture?

    /// Begin acquisition. Wire slots are claimed later, on each source's first state report.
    /// A Mac app keeps its HID and Bluetooth access while inactive, so focus changes leave the
    /// capture and its host slots alone. iOS and tvOS release BLE while inactive.
    @MainActor
    public func start() {
        guard Self.running == nil || Self.running === self else {
            log.info("SC2: passthrough already runs in another window's session")
            lock.lock()
            stopped = true
            lock.unlock()
            return
        }
        Self.running = self
        lock.lock()
        stopped = false
        suspended = false
        lock.unlock()
        // Take the hardware off the menu-nav reader (iOS): one peripheral, one central.
        manager.holdSc2Hardware(true)
        #if !os(macOS)
        // Both observers run on main, where `start` put this object; the weak reference crosses
        // no thread.
        nonisolated(unsafe) weak let weakSelf = self
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.willResignActiveNotification, object: nil, queue: .main
        ) { _ in
            guard let self = weakSelf else { return }
            self.lock.lock()
            self.suspended = true
            self.lock.unlock()
            // The slot goes with the radio: a host pad frozen on the last raw state would hold
            // its buttons for the whole background stay.
            self.releaseAll(reason: "app inactive")
            self.stopTransport()
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.didBecomeActiveNotification, object: nil, queue: .main
        ) { _ in
            guard let self = weakSelf else { return }
            self.lock.lock()
            self.suspended = false
            let dead = self.stopped
            self.lock.unlock()
            if !dead { self.startTransport() } // reacquire; the first report re-claims a slot
        })
        #endif
        startTransport()
    }

    /// Engage exactly one transport: USB when an SC2 controller collection is attached right now,
    /// otherwise BLE.
    ///
    /// USB wins because a plugged-in pad is the lower-latency path and is unambiguous — the
    /// alternative, running both, double-feeds a pad that is simultaneously charging and paired.
    /// The check opens nothing, so it cannot prompt or disturb a device another app holds.
    ///
    /// ponytail: the choice is made once per stream, so plugging a pad in mid-session keeps the
    /// BLE link it started with. Re-evaluate on an IOKit matching callback if that proves
    /// annoying on glass; a stream restart already picks the cable up today.
    private func startTransport() {
        // The link being replaced never fires `onSourceClosed` from its own `stop()`, so its
        // claimed slots would survive the switch: the host keeps a frozen virtual pad for the
        // session and the GameController twin stays suppressed behind it.
        releaseAll(reason: "transport switch")
        #if os(macOS)
        if Sc2UsbLink.attached() {
            // Exactly one link runs, so the other stops FIRST: two would double-feed the pad.
            link.stop()
            lock.lock()
            transport = .usb
            lock.unlock()
            usbLink.start()
            return
        }
        usbLink.stop()
        #endif
        lock.lock()
        transport = .ble
        lock.unlock()
        link.start()
    }

    /// Stop whichever transport is live, and forget which it was.
    private func stopTransport() {
        #if os(macOS)
        usbLink.stop()
        #endif
        link.stop()
        lock.lock()
        transport = .none
        lock.unlock()
    }

    /// Tear everything down: link stopped (unsubscribe, cancel, stop scanning), every slot
    /// released, typed state cleared. Idempotent (main actor, like `start`).
    @MainActor
    public func stop() {
        lock.lock()
        let wasStopped = stopped
        stopped = true
        lock.unlock()
        guard !wasStopped else { return }
        if Self.running === self { Self.running = nil }
        manager.steamController2Claims = 0
        manager.holdSc2Hardware(false)
        observers.forEach { NotificationCenter.default.removeObserver($0) }
        observers.removeAll()
        releaseAll(reason: "stop")
        stopTransport()
    }

    /// Replay one host raw write on the physical pad that claimed `pad` — wire this to
    /// `GamepadFeedback`'s hidRaw sink. Called on the feedback drain thread; `kind` is
    /// `PUNKTFUNK_HID_RAW_OUTPUT` (0) / `PUNKTFUNK_HID_RAW_FEATURE` (1) and `data` the id-first
    /// frame. NO main-actor hop — the rumble replay runs at Steam's 25–40 ms resend cadence and
    /// the device write happens on the link queue anyway.
    public func onHidRaw(pad: UInt8, kind: UInt8, data: [UInt8]) {
        lock.lock()
        let source = sources.first(where: { $0.value.padIndex == pad })?.key
        lock.unlock()
        guard let source else { return } // addressed to some other controller
        write(source: source, kind: kind, frame: data)
    }

    /// One id-first frame to `source`'s transport, on its link queue.
    private func write(source: UInt64, kind: UInt8, frame: [UInt8]) {
        #if os(macOS)
        if source != Self.bleSource {
            usbLink.writeRaw(source: source, kind: kind, frame: frame)
            return
        }
        #endif
        link.writeRaw(kind: kind, frame: frame)
    }

    /// A zero `0x80` rumble. The firmware holds the last level it got, so a stream that ends
    /// mid-rumble would leave the motors running.
    private static let rumbleStop: [UInt8] = [0x80] + [UInt8](repeating: 0, count: 9)

    // MARK: - Report path (link queue)

    private func handleReport(source: UInt64, _ framed: [UInt8]) {
        guard let id = framed.first else { return }
        if id == Sc2Device.idWireless || id == Sc2Device.idWirelessX {
            handleWireless(source: source, framed)
            return
        }
        var state = Sc2Device.State()
        var report = framed
        let isState = Sc2Device.parseState(report, into: &state)
        lock.lock()
        if stopped || suspended {
            lock.unlock()
            return
        }
        let src = sourceLocked(source)
        guard let pad = src.padIndex else {
            // Lazy slot claim on the source's FIRST parsed state report, BEFORE any input. The
            // claim hops to the main actor; this source's reports drop until it lands —
            // idempotent snapshots at ~66 Hz, nothing is missed. `claimPending` also clears on
            // a full table (all 16 indices taken), so a later report simply retries.
            let shouldClaim = isState && !src.claimPending
            if shouldClaim { src.claimPending = true }
            lock.unlock()
            if shouldClaim { claimSlot(source: source) }
            return
        }
        if !isState {
            // Battery/status and future report types still belong to the as-is stream.
            forwardRawLocked(src, &report, pad: pad)
            lock.unlock()
            return
        }
        // The gate reads the ungated hardware; the two planes below see what is left of it, so
        // a press the client consumed never reaches the game.
        deliver(src.ring.read(state))
        updateChordLocked(src)
        src.ring.apply(&state)
        forwardRawLocked(src, &report, pad: pad)
        mirrorTypedLocked(src, state, pad: pad)
        lock.unlock()
    }

    /// Deliver one pad's local-chord events. The gate decided them off the ungated hardware;
    /// this only routes them to the main actor, where the ring and the overlay live.
    private func deliver(_ events: [Sc2RingGate.Event]) {
        guard !events.isEmpty else { return }
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            MainActor.assumeIsolated {
                for event in events {
                    switch event {
                    case .ringChord: self.onRingChord?()
                    case .nav(let nav): self.onRingNav?(nav)
                    case .statsChord: StatsVerbosity.requestCycle(for: self.connection)
                    }
                }
            }
        }
    }

    /// The state slot for `source`, created on first sight — a pad that powers on while the ring
    /// is up joins it already gated. Caller holds `lock`.
    private func sourceLocked(_ source: UInt64) -> PadSource {
        if let existing = sources[source] { return existing }
        let fresh = PadSource()
        fresh.ring.ringOpen = ringOpenLocked
        sources[source] = fresh
        return fresh
    }

    /// One wireless connect/disconnect report (`0x79`/`0x46`).
    ///
    /// Authoritative ONLY through a Puck dongle, which is why this does nothing on any other
    /// transport. A wired or BLE pad emits these too — truthfully, since it genuinely has no
    /// radio link — and acting on that tore the wire slot down 255 ms after it was created on
    /// Android's first on-glass run. SDL's wired path likewise marks the controller connected
    /// unconditionally and reconnects on any state report.
    ///
    /// A connect needs nothing: the first state report claims the slot, and the host's virtual
    /// Puck queues its own connect edge.
    private func handleWireless(source: UInt64, _ framed: [UInt8]) {
        guard isDongleSource(source), framed.count >= 2,
              framed[1] == Sc2Device.wirelessDisconnect else { return }
        log.info("SC2: Puck reports controller powered off — releasing wire slot")
        releaseSource(source, reason: "puck wireless disconnect")
    }

    /// Forward one id-first report on the raw plane: copy into the source's reusable buffer,
    /// send. Caller holds `lock`.
    private func forwardRawLocked(_ src: PadSource, _ report: inout [UInt8], pad: UInt8) {
        let n = min(report.count, src.rawBuf.count)
        src.rawBuf.replaceSubrange(0 ..< n, with: report[0 ..< n])
        src.rawBuf.withUnsafeBytes { buf in
            connection.sendHidReport(pad: pad, UnsafeRawBufferPointer(rebasing: buf[0 ..< n]))
        }
    }

    /// Diff the parsed state onto the per-transition plane (buttons + axes, on change only).
    /// The state is already gated, so a swallowed button is simply never held here. Caller
    /// holds `lock`.
    private func mirrorTypedLocked(_ src: PadSource, _ state: Sc2Device.State, pad: UInt8) {
        let wired = Sc2Device.wireButtons(state.buttons)
        var changed = wired ^ src.wireButtons
        while changed != 0 {
            let bit = changed & (~changed &+ 1) // lowest changed bit
            connection.send(.gamepadButton(bit, down: wired & bit != 0, pad: UInt32(pad)))
            changed &= ~bit
        }
        src.wireButtons = wired
        axisLocked(src, GamepadWire.axisLSX, state.lsX, pad: pad)
        axisLocked(src, GamepadWire.axisLSY, state.lsY, pad: pad)
        axisLocked(src, GamepadWire.axisRSX, state.rsX, pad: pad)
        axisLocked(src, GamepadWire.axisRSY, state.rsY, pad: pad)
        axisLocked(src, GamepadWire.axisLT, state.lt, pad: pad)
        axisLocked(src, GamepadWire.axisRT, state.rt, pad: pad)
    }

    private func axisLocked(_ src: PadSource, _ id: UInt32, _ value: Int32, pad: UInt8) {
        let i = Int(id)
        guard src.lastAxis[i] != value else { return }
        src.lastAxis[i] = value
        connection.send(.gamepadAxis(id, value: value, pad: UInt32(pad)))
    }

    /// Arm the disconnect timer while the full chord is held, disarm on any release —
    /// GamepadCapture's rule, off this capture's own state. Read from the HARDWARE mask, never
    /// the forwarded one: the way out of a stream must not be something a client-side gate can
    /// swallow. Any captured pad can fire it. Caller holds `lock`.
    private func updateChordLocked(_ src: PadSource) {
        let held = src.ring.hardware & Self.escapeChord == Self.escapeChord
        if held, src.chordWork == nil {
            let work = DispatchWorkItem { [weak self] in
                guard let self else { return }
                MainActor.assumeIsolated { self.onDisconnectRequest?() }
            }
            src.chordWork = work
            DispatchQueue.main.asyncAfter(deadline: .now() + Self.disconnectHold, execute: work)
        } else if !held, let work = src.chordWork {
            work.cancel()
            src.chordWork = nil
        }
    }

    // MARK: - Slot lifecycle

    /// Reserve a wire index on the main actor and finish `source`'s claim under the lock. On
    /// success sends `gamepadArrival` (pref 9, or 10 for a Puck slot) — the declaration the
    /// host builds the virtual SC2 from — before any input can flow on that index.
    private func claimSlot(source: UInt64) {
        // The host builds the virtual pad from the identity, so it is read before the arrival.
        readIdentity(source: source) { [weak self] identity in
            self?.finishClaim(source: source, identity: identity)
        }
    }

    /// `source`'s identity, read on the link queue: synchronously over USB, as a GATT exchange
    /// over Bluetooth. `done` runs on the link queue.
    private func readIdentity(
        source: UInt64, _ done: @escaping (PunktfunkConnection.PadIdentity?) -> Void
    ) {
        #if os(macOS)
        if source != Self.bleSource {
            done(usbLink.identity(source: source))
            return
        }
        #endif
        link.readIdentity(done)
    }

    private func finishClaim(source: UInt64, identity: PunktfunkConnection.PadIdentity?) {
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            MainActor.assumeIsolated {
                let index = self.manager.reserveExternalPadIndex()
                // Resolve the identity BEFORE the slot goes live. Setting `padIndex` is what
                // opens the raw plane on this index, so everything between it and the arrival
                // is a window in which a report can precede the declaration — and both reads
                // below take the link's own lock, which must not be held under `lock` anyway.
                //
                // A Puck slot is its own host-side backend (native seven-interface topology,
                // four controller slots; a Windows host folds it onto the wired identity), so
                // it declares its own kind — a wired or BLE pad stays `.steamController2`.
                let dongle = self.isDongleSource(source)
                let kind: PunktfunkConnection.GamepadType =
                    dongle ? .steamController2Puck : .steamController2
                #if os(macOS)
                let serial = source == Sc2Capture.bleSource
                    ? nil : self.usbLink.serial(source: source)
                #else
                let serial: String? = nil
                #endif
                self.lock.lock()
                guard let src = self.sources[source], src.claimPending else {
                    // The source went away between the report that scheduled this claim and
                    // the hop landing: `releaseSource` cleared the pending token (it is set
                    // nowhere else while a claim is in flight). Completing now would arm a
                    // virtual pad — and announce a badge — for a dead source, with no
                    // `.released` ever coming. Hand the index straight back instead; a live
                    // source's next state report simply schedules a fresh claim.
                    self.lock.unlock()
                    if let index { self.manager.releaseExternalPadIndex(index) }
                    return
                }
                src.claimPending = false
                if self.stopped || self.suspended {
                    self.lock.unlock()
                    if let index { self.manager.releaseExternalPadIndex(index) }
                    return
                }
                guard let index else {
                    // All 16 wire indices taken — drop reports until one frees (a later
                    // report retries).
                    self.lock.unlock()
                    return
                }
                src.padIndex = index
                // Under the lock, like the link queue's own sends: a report must not reach the
                // host before the arrival that creates its pad, nor the arrival before its identity.
                if let identity { self.connection.sendPadIdentity(pad: index, identity) }
                self.connection.send(.gamepadArrival(pref: kind.rawValue, pad: UInt32(index)))
                self.pushGateLocked(pad: index)
                self.lock.unlock()
                // This pad is ours now, so its GameController twin must stop being forwarded.
                self.syncShadowSuppression()
                let via = dongle ? "Puck" : (self.currentTransport == .usb ? "USB" : "BLE")
                log.info(
                    "SC2 captured → wire pad \(index) (\(via, privacy: .public) passthrough, pref \(kind.rawValue), serial \(serial ?? "?"))"
                )
                self.onPhaseChange?(.captured(pad: index))
            }
        }
    }

    /// Free one source's wire slot: `gamepadRemove` (the host tears its virtual pad down — no
    /// stuck last frame), typed-diff state dropped with the source entry, index handed back to
    /// the shared allocator. The removal restarts core's gate for that index.
    /// `.released` fires only when no other source still holds a slot, so the badge survives a
    /// single pad of several powering off. Safe from any thread; no-op for an unseen source.
    private func releaseSource(_ source: UInt64, reason: String) {
        lock.lock()
        guard let src = sources.removeValue(forKey: source) else {
            lock.unlock()
            return
        }
        let index = src.padIndex
        let chord = src.chordWork
        let lastClaimed = index != nil && !sources.values.contains { $0.padIndex != nil }
        lock.unlock()
        chord?.cancel()
        guard let index else { return }
        write(source: source, kind: 0, frame: Self.rumbleStop)
        connection.send(.gamepadRemove(pad: UInt32(index)))
        DispatchQueue.main.async { [weak self, manager] in
            MainActor.assumeIsolated {
                manager.releaseExternalPadIndex(index)
                // Hand the GameController twin back the moment the last slot goes.
                self?.syncShadowSuppression()
                if lastClaimed { self?.onPhaseChange?(.released) }
            }
        }
        log.info("SC2: wire pad \(index) released (\(reason))")
    }

    /// Drop the GameController shadow exactly while this capture holds a claimed wire slot.
    ///
    /// Not for the capture's lifetime: the capture is built for every stream the toggle is on
    /// for, whether or not an SC2 is reachable, and suppressing on that would leave a pad the
    /// capture cannot open (Input Monitoring refused, nothing paired) forwarded on NEITHER
    /// plane — worse than the double feed this exists to stop. Never called holding `lock`:
    /// the setter rebuilds the manager's published pad list.
    @MainActor
    private func syncShadowSuppression() {
        lock.lock()
        let owned = sources.values.filter { $0.padIndex != nil }.count
        lock.unlock()
        manager.steamController2Claims = owned
    }

    /// Release every source — the stop/suspend teardown.
    private func releaseAll(reason: String) {
        lock.lock()
        let keys = Array(sources.keys)
        lock.unlock()
        for key in keys { releaseSource(key, reason: reason) }
    }
}
