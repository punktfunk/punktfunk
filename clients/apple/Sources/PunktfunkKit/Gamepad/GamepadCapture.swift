// Gamepad capture → punktfunk/1 datagrams. Forwards every controller GamepadManager selected,
// each on its own stable wire pad index (pf-client-core's slot model), for a streaming session.
// One unpinned controller is pad 0; a pin forwards only that one, also as pad 0.
//
// Each forwarded controller gets a `Slot`: its GC handlers plus the wire state for its pad index.
// A slot opens with GamepadArrival and closes with held state flushed, then GamepadRemove.
//
// The wire is incremental (one button/axis transition per event), so every valueChanged snapshots
// the pad and sends the diff. Sticks are ±32767 with +y up, triggers 0...255. Touchpad contacts
// (0...65535, +y down) and motion (DualSense units, converted in `Wire`) ride the rich-input
// plane, which the host reads only for a DualSense or DualShock 4 pad.
//
// A controller cannot click local UI, so forwarding ignores the mouse-capture toggle. Deactivation,
// a controller switch or stop releases every held control on the wire.
//
// GameController holds one handler per element, so one capture per process owns the physical
// controllers (`claim`). The rest are parked: slots and pad indices kept, handlers detached,
// nothing sent. A Mac stream window claims when it becomes key.

#if os(macOS)
import AppKit
#else
import UIKit
#endif
import Combine
import Foundation
import GameController

@MainActor
public final class GamepadCapture {
    private let connection: PunktfunkConnection
    private let manager: GamepadManager
    private var forwardedSub: AnyCancellable?
    private var observers: [NSObjectProtocol] = []
    /// App inactive → GC stops delivering; everything is released and stays silent.
    private var suspended = false
    /// Another capture owns the controllers: no handlers attached, nothing sent.
    private var parked = false
    /// Input reaches the wire only while neither holds.
    private var live: Bool { !suspended && !parked }
    /// The capture the physical controllers drive right now.
    private static weak var owner: GamepadCapture?
    /// Fired ON MAIN when this capture gains (`true`) or loses the controllers — the session
    /// quiets its pad feedback while another window plays.
    public var onOwnershipChange: ((Bool) -> Void)?

    /// One forwarded controller: the open device plus the last wire state for its pad index (the
    /// diff base — also what `flush` unwinds). Held per Slot so two controllers never clobber each
    /// other's held buttons/axes/fingers. Mirrors pf-client-core's `Slot`. Main-actor like its
    /// owner, so the main-run-loop timers that fire on it may hold it.
    @MainActor private final class Slot {
        let controller: GCController
        /// Wire pad index (GamepadManager's stable lowest-free assignment), threaded onto every
        /// event this controller sends — the low byte of `flags`.
        let pad: UInt32
        /// The controller KIND declared to the host (GamepadArrival) when the slot opened — the
        /// user's explicit "Controller type" setting when they picked one, else the detected
        /// kind (`GamepadManager.declaredKind(for:)`). NOT the physical pad's kind: local feedback
        /// keys off the live `GCController` subclass instead, so whatever the host DOES send is
        /// applied natively to the pad in the user's hands. What the host sends is bounded by the
        /// emulated type, though — a virtual DualShock 4 has no adaptive-trigger reports in its
        /// protocol, so emulating one gives those up by construction (rumble + lightbar remain).
        let pref: PunktfunkConnection.GamepadType
        var buttons: UInt32 = 0
        var axes: [Int32] = [0, 0, 0, 0, 0, 0]
        var fingerActive: [Bool] = [false, false]
        /// A motion sample went out on this pad — `flush` then owes the wire a zero-gyro
        /// sample: the host holds motion as STATE and re-emits it, so a nonzero angular
        /// velocity left behind reads as endless rotation (the gyro-sweep latch).
        var motionSent = false
        /// The last accel sent, re-used by the flush zero so "rotation stopped" doesn't
        /// also replace a plausible gravity vector with free-fall.
        var lastAccel: (Int16, Int16, Int16) = (0, 0, 0)
        // Hold-Select→guide gesture state (pf-client-core's `SelectGesture`, adapted to
        // this class's mask-diff model): a Select pressed ALONE is held out of the mask
        // until it resolves into a tap (delivered on release) or — past `guideHold` — a
        // synthetic guide, down until release.
        var selectPending = false
        var selectAsGuide = false
        /// A delivered tap's release is owed (`tapTimer` scheduled) — its down went out
        /// outside `buttons`, so `flush` must know to lift it.
        var tapReleaseOwed = false
        /// `Select+A` opened the ring: neither press reached the host, so neither release may.
        var swallowA = false
        var swallowSelect = false
        var gestureTimer: Timer?
        var tapTimer: Timer?
        init(controller: GCController, pad: UInt32, pref: PunktfunkConnection.GamepadType) {
            self.controller = controller
            self.pad = pad
            self.pref = pref
        }
    }

    /// Open forwarded controllers, one Slot per physical pad on its own wire index. Reconciled
    /// against `manager.forwarded` (empty until a session's `start`, cleared by `stop`).
    private var slots: [Slot] = []

    /// The cross-client controller escape chord (pf-client-core's `ESCAPE_CHORD`):
    /// L1+R1+Start+Select held together — four simultaneous buttons no game uses, so normal
    /// play can't trip it. Held for `disconnectHold` it ends the session via
    /// `onDisconnectRequest`; the chord keeps forwarding to the host meanwhile (the user is
    /// leaving anyway). The desktop clients' quick-press step (leave fullscreen / release
    /// capture) has no Apple equivalent worth wiring — macOS has ⌃⌥⇧Q/D, touch has the HUD.
    /// Internal rather than private only so `GamepadEscapeChordTests` can pin it against
    /// `escapeChordElements` below — the two must not drift.
    static let escapeChord: UInt32 =
        GamepadWire.leftShoulder | GamepadWire.rightShoulder | GamepadWire.start | GamepadWire.back
    /// `escapeChord`'s four elements by GameController alias — the ONLY system gestures claimed
    /// while forwarding is off (see `openSlot`). Kept beside the mask it mirrors: change one and
    /// change the other, or the chord silently stops reaching us on tvOS. A test asserts the two
    /// agree, because the failure is invisible until someone is stuck in a stream on an Apple TV.
    static let escapeChordElements = [
        GCInputLeftShoulder, GCInputRightShoulder, GCInputButtonMenu, GCInputButtonOptions,
    ]
    /// The stats-overlay chord: Select + X, one tier per completion (off → compact → normal →
    /// detailed → off). It exists because a controller in both hands has no other way to the
    /// numbers — the ⌃⌥⇧S combo needs a keyboard and the three-finger tap needs a free screen —
    /// and on tvOS there is no other way AT ALL, which is what this fixes.
    ///
    /// Built like Android's mic chord (`GamepadRouter.MIC_CHORD`, Select + Y) and deliberately
    /// not overlapping `escapeChord`: X is none of its four buttons, so no way of reaching the
    /// exit chord passes through this one on the way, and vice versa. Select is a menu button
    /// rather than a twitch action, which keeps the pair out of real play. Y is left free so the
    /// mic chord can be ported onto it later without moving this one.
    static let statsChord: UInt32 = GamepadWire.back | GamepadWire.x
    /// `statsChord`'s elements by GameController alias — same mirror-the-mask rule (and same
    /// invisible failure) as `escapeChordElements`; the same test pins both.
    static let statsChordElements = [GCInputButtonOptions, GCInputButtonX]
    /// Every element some chord reads — what a NON-forwarding slot claims (see `openSlot`). The
    /// escape chord's four plus the stats chord's X; Select is shared, so it appears once.
    static let chordElements: [String] =
        escapeChordElements + statsChordElements.filter { !escapeChordElements.contains($0) }
    /// pf-client-core's `DISCONNECT_HOLD` — the same 1.5 s on every client.
    private static let disconnectHold: TimeInterval = 1.5
    /// pf-client-core's `GUIDE_HOLD`: hold Select alone this long → the HOST's guide goes
    /// down (until release, so a long hold is the host's long-press — a Gaming-Mode
    /// host's QAM). The gesture exists because iOS reserves the physical Home press (the
    /// Game Overlay; sanctioned opt-out only via the user's iOS 27+ Home-button setting)
    /// and tvOS never delivers it at all.
    private static let guideHold: TimeInterval = 0.35
    /// pf-client-core's `TAP_PRESS`: a held-back Select tap is delivered as a press with
    /// its release this far behind — back-to-back transitions can fold into nothing in
    /// the host's per-pad input fold.
    private static let tapPress: TimeInterval = 0.05
    private var chordTimer: Timer?
    /// Fired ON MAIN once the escape chord has been held `disconnectHold` — the session owner
    /// disconnects. On tvOS this (plus the Siri Remote's hold-Back) is the ONLY way out of a
    /// stream with a controller: B/Menu presses are deliberately swallowed during a session so
    /// gameplay can't end it (see ContentView's tvOS session branch).
    public var onDisconnectRequest: (() -> Void)?

    /// Fired ON MAIN, once per slot at open, when a controller that HAS a gyro was given a host
    /// backend without a motion plane — its motion is not being sent, because every sample would
    /// be decoded and dropped. The argument is the kind this pad declared, so the UI can name it.
    ///
    /// It fires at open rather than on the first sample precisely because nothing is sampled: the
    /// IMU is never powered in this case (see `openSlot`), which is also what stops the pad
    /// burning battery streaming gyro nobody reads.
    public var onMotionUnreachable: ((PunktfunkConnection.GamepadType) -> Void)?

    /// `Select+A`, Select first, while Select is still pending its guide hold: the quick-action
    /// ring's opener (design/touch-client-overlay.md §2.6) — the first chord the host never
    /// sees. A is "jump" or "confirm" in most games, so both presses are swallowed.
    public var onRingChord: ((UInt32) -> Void)?
    /// A pad press while the ring owns the pad (`ringOpen`).
    public var onRingNav: ((RingNav) -> Void)?
    /// The ring is up: everything held is released on the host NOW (a held sprint must not
    /// survive a menu), and until it closes the hardware state is adopted silently while its
    /// edges drive the ring. On close nothing is replayed: what is still held is already the
    /// slot's state, so the next diff sends only real changes.
    public var ringOpen = false {
        didSet {
            guard ringOpen != oldValue else { return }
            stickSector = nil
            if ringOpen { for slot in slots { flush(slot) } }
        }
    }
    /// The ring sector the left stick last resolved to — see `ringSector`.
    private var stickSector: Int?

    /// Forward this device's controllers to the host at all (`Settings.gamepadForwarding`,
    /// default true). Off is for a couch whose controller reaches the host another way — USB
    /// passthrough such as VirtualHere, or a pad plugged into the host itself — where
    /// forwarding as well would give the host two pads for one pair of hands.
    ///
    /// Off still opens slots and tracks button state; it just sends nothing (see `wire`). That
    /// is deliberate, not laziness: the escape chord is read off the same slots, and on tvOS it
    /// is the ONLY controller way out of a stream — a session that silently lost its exit
    /// because a forwarding preference was off would be a worse bug than the one this fixes.
    /// Unlike pf-client-core's slots, GameController claims nothing exclusive, so holding one
    /// open costs the host nothing and blocks no passthrough tool.
    public let forwarding: Bool

    /// The connection, or nil while forwarding is off — every wire send goes through this, so
    /// "don't forward" is one fact in one place rather than a condition at twelve call sites.
    private var wire: PunktfunkConnection? { forwarding ? connection : nil }

    /// Forward the raw guide + share/QAM presses (`EffectiveSettings.systemButtonsForward`,
    /// default true on Apple — where the OS shows its own overlay for them, that's the OS's
    /// business; local mode exists for preset parity with the Gaming-Mode clients).
    public let systemForward: Bool
    /// The hold-Select guide gesture (`EffectiveSettings.guideGestureEnabled` — auto = on
    /// everywhere but macOS). See `guideHold`.
    public let guideGesture: Bool

    #if os(iOS)
    /// Opt-in phone-gyro mirror (`DefaultsKey.gyroFromDevice`): while player 1's forwarded
    /// controller has no rotation sensor, this device's IMU sources pad 0's motion instead —
    /// for clip-on pads without a gyro. Session-scoped (the setting is read once here); nil
    /// when off, unavailable, or forwarding is off (the mirror is wire-only, so with nothing
    /// to send there is nothing to mirror). Engage/stand-down lives in `updateDeviceGyro`.
    private let deviceGyro: DeviceGyro?
    #endif

    public init(
        connection: PunktfunkConnection, manager: GamepadManager, forwarding: Bool = true,
        systemForward: Bool = true, guideGesture: Bool = false
    ) {
        self.connection = connection
        self.manager = manager
        self.forwarding = forwarding
        self.systemForward = systemForward
        self.guideGesture = guideGesture
        #if os(iOS)
        if forwarding, DeviceGyro.isAvailable,
            UserDefaults.standard.bool(forKey: DefaultsKey.gyroFromDevice) {
            deviceGyro = DeviceGyro { [weak connection] gyro, accel in
                // Thread-safe (sendMotion locks); pad 0 by the same rule as the rumble mirror.
                connection?.sendMotion(pad: 0, gyro: gyro, accel: accel)
            }
        } else {
            deviceGyro = nil
        }
        #endif
    }

    public func start() {
        claim()
        // Session-scoped index assignment: a controller pinned before the session forwards as
        // pad 0 (pf-client-core assigns indices at slot-open time, not app-launch time).
        manager.resetForwardingAssignment()
        // Fires immediately with the current forwarded set, then on every change — a connect,
        // disconnect, or pin change reconciles the open slots against it (opening/closing devices
        // and flushing wire state so nothing sticks down).
        forwardedSub = manager.$forwarded.sink { [weak self] list in
            MainActor.assumeIsolated { self?.reconcile(list) }
        }
        #if os(macOS)
        let resign = NSApplication.willResignActiveNotification
        let activate = NSApplication.didBecomeActiveNotification
        #else
        let resign = UIApplication.willResignActiveNotification
        let activate = UIApplication.didBecomeActiveNotification
        #endif
        observers.append(NotificationCenter.default.addObserver(
            forName: resign, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.suspended = true
                self?.releaseAll()
                // The mirror pauses with capture (its stop parks the host pad's rotation
                // at zero — an overlay pull-down must not leave the game spinning).
                self?.updateDeviceGyro()
            }
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: activate, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.suspended = false
                // Re-send every open pad's current state (GC delivered nothing while inactive).
                for slot in self.slots {
                    if let ext = slot.controller.extendedGamepad { self.sync(slot, ext) }
                }
                self.updateDeviceGyro()
            }
        })
    }

    public func stop() {
        #if os(iOS)
        deviceGyro?.stop()
        #endif
        closeAllSlots()
        forwardedSub = nil
        observers.forEach { NotificationCenter.default.removeObserver($0) }
        observers.removeAll()
        if Self.owner === self { Self.setOwner(nil) }
        parked = false
    }

    /// Take the physical controllers: the current owner releases what it holds and parks, and
    /// this capture attaches its handlers and sends the pads' current state.
    public func claim() {
        guard Self.owner !== self else { return }
        Self.owner?.park()
        Self.setOwner(self)
        parked = false
        for slot in slots {
            attach(slot)
            if let ext = slot.controller.extendedGamepad { sync(slot, ext) }
        }
        updateDeviceGyro()
        onOwnershipChange?(true)
    }

    /// Hand the controllers to nobody: a window without a stream came to the front, so its
    /// menus read the pad instead.
    public static func releaseControllers() {
        owner?.park()
        setOwner(nil)
    }

    private static func setOwner(_ capture: GamepadCapture?) {
        owner = capture
        GamepadManager.shared.streamOwnsPads = capture != nil
    }

    private func park() {
        guard !parked else { return }
        releaseAll()
        for slot in slots { detach(slot) }
        parked = true
        updateDeviceGyro()
        onOwnershipChange?(false)
    }

    /// A one-shot synthetic tap of a system button on the host's pad (pf-client-core's
    /// `GamepadService.tapButton`): down now, up `tapPress` later, on the first forwarded slot's
    /// wire index — pad 0 when none is open, which is best-effort (the host pad may not exist).
    /// The quick-action ring's route to the host's guide on a device whose OS keeps the physical
    /// Home press for itself. Deliberately outside `slot.buttons` and the system-buttons policy:
    /// this is the ring's own press, not the player's.
    public func tapButton(_ bit: UInt32) {
        guard let wire else { return }
        let pad = slots.first?.pad ?? 0
        wire.send(.gamepadButton(bit, down: true, pad: pad))
        let timer = Timer(timeInterval: Self.tapPress, repeats: false) { [weak self] _ in
            MainActor.assumeIsolated {
                self?.wire?.send(.gamepadButton(bit, down: false, pad: pad))
            }
        }
        RunLoop.main.add(timer, forMode: .common)
    }

    /// The controller type every forwarded pad declares (`.auto` = each pad as itself).
    public var padType: PunktfunkConnection.GamepadType { manager.typeSetting }

    /// Whether any physical controller holds a slot.
    public var hasForwardedPads: Bool { !slots.isEmpty }

    /// Emulate `type` for the rest of this stream. The host builds a pad from its arrival and
    /// never swaps a built one, so each slot whose declared kind moves is closed and reopened.
    /// The SC2 passthrough declares its own kind and is left alone.
    public func setPadType(_ type: PunktfunkConnection.GamepadType) {
        manager.typeSetting = type
        for slot in slots {
            guard let dc = manager.forwarded.first(where: { $0.controller === slot.controller }),
                  manager.declaredKind(for: dc) != slot.pref else { continue }
            closeSlot(slot)
            openSlot(dc)
        }
        updateEscapeChord()
        updateDeviceGyro()
    }

    /// Bring `slots` in line with the forwarded set: close any slot no longer wanted (flushing its
    /// held wire state and sending GamepadRemove first) and open any newly-forwarded controller into
    /// its assigned wire index. A controller that stays forwarded on its index keeps its slot
    /// untouched, so a second pad connecting never disturbs the first. Mirrors pf-client-core's
    /// `reconcile_slots`.
    ///
    /// A slot whose index moved is closed and opened on the new one. Another window's session
    /// start renumbers the pads, and the host's rumble and HID output are routed by index.
    private func reconcile(_ forwarded: [GamepadManager.DiscoveredController]) {
        var wanted: [ObjectIdentifier: UInt32] = [:]
        for dc in forwarded {
            if let pad = manager.padIndex(for: dc) {
                wanted[ObjectIdentifier(dc.controller)] = UInt32(pad)
            }
        }
        for slot in slots where wanted[ObjectIdentifier(slot.controller)] != slot.pad {
            closeSlot(slot)
        }
        for dc in forwarded where !slots.contains(where: { $0.controller === dc.controller }) {
            openSlot(dc)
        }
        // A chord-holding pad may have just unplugged — re-evaluate so a stale hold disarms.
        updateEscapeChord()
        // Pad 0 may have changed hands — re-evaluate whether this device's IMU speaks for it.
        updateDeviceGyro()
    }

    /// Open one forwarded controller on its assigned wire index: declare its kind (GamepadArrival
    /// — before any input), wake the host pad, then attach its handlers and send its initial
    /// state unless another capture owns the controllers. Skipped when the pad has no wire index
    /// (every slot taken) or exposes no extended profile.
    private func openSlot(_ dc: GamepadManager.DiscoveredController) {
        guard let pad = manager.padIndex(for: dc), let ext = dc.controller.extendedGamepad else { return }
        let slot = Slot(controller: dc.controller, pad: UInt32(pad), pref: manager.declaredKind(for: dc))
        slots.append(slot)
        // Declare the pad's KIND before any input: the host builds its virtual device from this,
        // not the handshake default. Then wake the host pad, which exists from its first event.
        wire?.send(.gamepadArrival(pref: slot.pref.rawValue, pad: slot.pad))
        wire?.send(.gamepadAxis(GamepadWire.axisLSX, value: 0, pad: slot.pad))
        if forwarding, dc.controller.motion?.hasRotationRate == true,
           !connection.motionReaches(declared: slot.pref) {
            onMotionUnreachable?(slot.pref)
        }
        guard !parked else { return }
        attach(slot)
        sync(slot, ext)
    }

    /// Attach a slot's GameController handlers and claim its system gestures and motion sensors.
    /// Only the owning capture's slots are attached.
    private func attach(_ slot: Slot) {
        let c = slot.controller
        guard let ext = c.extendedGamepad else { return }
        ext.valueChangedHandler = { [weak self, weak slot] g, _ in
            MainActor.assumeIsolated { if let self, let slot { self.sync(slot, g) } }
        }
        // Claim EVERY element's system gesture while this pad drives a stream. The OS attaches
        // gestures to several controller buttons — share/create → local screenshot/recording,
        // Home → Game Center overlay (iOS) / Launchpad's Games folder (macOS) — and with a
        // gesture attached the press is the system's, not the game's. During capture the remote
        // session IS the game: the share button must reach the host (e.g. Steam screenshots),
        // the PS button must open the host's Steam overlay. Restored to .enabled on close.
        //
        // With forwarding OFF none of that applies — no press reaches the host, so taking the
        // user's screenshot gesture away buys nothing. NARROWED, not skipped: the CHORDS are
        // still read off this slot — on tvOS the escape chord is the only controller way out of
        // a stream, and the stats chord the only way to the overlay — so their own elements keep
        // their claim. (Menu especially: leave its gesture attached on tvOS and the press is the
        // system's — the chord would never complete and the session would have no controller
        // exit at all.)
        let claimed = forwarding
            ? Array(c.physicalInputProfile.elements.values)
            : Self.chordElements.compactMap { c.physicalInputProfile.elements[$0] }
        for element in claimed {
            element.preferredSystemGestureState = .disabled
        }
        // Home/PS → the host's guide (the Steam-overlay button). On macOS/iOS 27 the OS honors
        // the claim above only for apps under Home Button Overrides (or macOS with Home Button
        // Actions off); Settings links there (`watchHomeButton`). Driven from this handler: the
        // legacy `extendedGamepad.buttonHome` is often nil when the element exists. tvOS: nil.
        if let home = c.physicalInputProfile.buttons[GCInputButtonHome] {
            home.pressedChangedHandler = { [weak self, weak slot] _, _, pressed in
                MainActor.assumeIsolated { if let self, let slot { self.sendGuide(slot, down: pressed) } }
            }
        }
        if let tp = Self.touchpad(ext) {
            tp.primary.valueChangedHandler = { [weak self, weak slot] _, x, y in
                MainActor.assumeIsolated { if let self, let slot { self.touch(slot, finger: 0, x: x, y: y) } }
            }
            tp.secondary.valueChangedHandler = { [weak self, weak slot] _, x, y in
                MainActor.assumeIsolated { if let self, let slot { self.touch(slot, finger: 1, x: x, y: y) } }
            }
        }
        // Motion is wire-only — `forwardMotion` has nothing to do with forwarding off, and no
        // local feature reads it. Powering the IMU anyway costs the pad real battery (it streams
        // gyro + accel continuously over Bluetooth, which is why `closeSlot` is careful to power
        // it back down), so with nothing to forward we simply never turn it on.
        //
        // A host that built this pad a backend WITHOUT a motion plane is the same situation: every
        // sample would be decoded and dropped, so there is equally nothing to forward. Asked per
        // pad off what this slot declared, not off the session echo — under "Automatic" a couch
        // with an X-Box pad on 0 and a DualSense on 1 echoes X-Box 360 while the host builds pad 1
        // a DualSense whose gyro works.
        //
        // Gated on `hasRotationRate`, not on `motion != nil`. An X-Box controller exposes a
        // `GCMotion` that reports gravity and NOTHING else — attaching to it streamed a
        // permanently-zero `rotationRate` to the host as authoritative gyro, under a declaration
        // that says this pad has one. A game reading it sees a controller being held perfectly
        // still forever, which is worse than seeing no motion plane at all: there is nothing to
        // fall back to and nothing to notice.
        if forwarding, let motion = c.motion, motion.hasRotationRate,
           connection.motionReaches(declared: slot.pref) {
            if motion.sensorsRequireManualActivation { motion.sensorsActive = true }
            // On main like every handler here: `handlerQueue` belongs to the controller, so
            // moving motion would move the tvOS escape chord too. Jitter? Read the host's
            // `motion_cadence` histogram first.
            motion.valueChangedHandler = { [weak self, weak slot] m in
                MainActor.assumeIsolated { if let self, let slot { self.forwardMotion(slot, m) } }
            }
        }
    }

    /// Flush a slot's held wire state (so nothing sticks down host-side), signal the host to tear
    /// its virtual device down (GamepadRemove), then `detach` unless parked. Wire-only until the
    /// GC cleanup, so it is safe even when the device already physically unplugged. Mirrors
    /// pf-client-core's `close_slot_at`.
    private func closeSlot(_ slot: Slot) {
        flush(slot)
        // Sent after the flush so the core stamps it with a seq past the zeroing snapshots; the host
        // seq-gates it, so a reordered snapshot can't resurrect the removed pad.
        wire?.send(.gamepadRemove(pad: slot.pad))
        // A parked slot's handlers are the owner's now.
        if !parked { detach(slot) }
        slots.removeAll { $0 === slot }
    }

    /// Detach a slot's handlers, hand its system gestures back and power its sensors down.
    private func detach(_ slot: Slot) {
        let c = slot.controller
        if let ext = c.extendedGamepad {
            ext.valueChangedHandler = nil
            let tp = Self.touchpad(ext)
            tp?.primary.valueChangedHandler = nil
            tp?.secondary.valueChangedHandler = nil
        }
        c.physicalInputProfile.buttons[GCInputButtonHome]?.pressedChangedHandler = nil
        // Hand the system gestures back to the OS before letting the pad go — outside a stream the
        // share button's screenshot and the Home overlay are the user's, not ours.
        for element in c.physicalInputProfile.elements.values {
            element.preferredSystemGestureState = .enabled
        }
        if let motion = c.motion {
            motion.valueChangedHandler = nil
            // Power the sensors back down — left active they keep the pad streaming gyro/accel
            // over Bluetooth (battery drain) long after the session.
            if motion.sensorsRequireManualActivation { motion.sensorsActive = false }
        }
    }

    private func closeAllSlots() {
        while let slot = slots.first { closeSlot(slot) }
        chordTimer?.invalidate()
        chordTimer = nil
    }

    /// Snapshot the profile into a slot's wire state and send every transition since the last one,
    /// tagged with the slot's wire pad index.
    private func sync(_ slot: Slot, _ g: GCExtendedGamepad) {
        guard live else { return }
        // guide is driven separately (`sendGuide`, off the Home handler) and deliberately kept out
        // of `buttonMask`. Preserve its current held state here so the XOR diff below never sees it
        // as "changed" — otherwise the first stick/button move after a guide press would emit a
        // spurious guide-UP while the button is still physically held (and drop the bit from
        // `slot.buttons`, swallowing the real release too). `flush`/`allButtons` still release it.
        var raw = Self.buttonMask(g)
        // Raw system buttons stay local when passthrough is off: misc1 (share/QAM) is
        // masked here, guide is gated at its own handler.
        if !systemForward { raw &= ~GamepadWire.misc1 }
        raw = ringChordFiltered(slot, raw)
        // The hold-Select gesture rewrites the mask: a Select pressed alone is held out
        // until it resolves (tap on release / synthetic guide past the threshold).
        if guideGesture { raw = gestureFiltered(slot, raw) }
        let newButtons = raw | (slot.buttons & GamepadWire.guide)
        let newAxes = Self.axesOf(g)
        if ringOpen {
            // Adopt, don't send: the flush at open released everything; a press inside the ring
            // must not fire in the game the instant it closes.
            let pressed = newButtons & ~slot.buttons
            slot.buttons = newButtons
            slot.axes = newAxes
            let map: [(UInt32, RingNav)] = [
                (GamepadWire.dpadUp, .up), (GamepadWire.dpadDown, .down),
                (GamepadWire.dpadLeft, .left), (GamepadWire.dpadRight, .right),
                (GamepadWire.a, .confirm), (GamepadWire.b, .back), (GamepadWire.y, .centre),
            ]
            for (bit, nav) in map where pressed & bit != 0 { onRingNav?(nav) }
            // The left stick AIMS: its sector is the slot, so the ring follows the thumb the way
            // a weapon wheel does. Sent on every sector change, neutral included — the D-pad is
            // what steps disc by disc.
            let (lx, ly) = (g.leftThumbstick.xAxis.value, g.leftThumbstick.yAxis.value)
            let sector = Self.ringSector(lx, ly, stickSector)
            if sector != stickSector {
                stickSector = sector
                onRingNav?(.sector(sector))
            }
            return
        }
        let changed = newButtons ^ slot.buttons
        if changed != 0 {
            let was = slot.buttons
            for bit in GamepadWire.allButtons where changed & bit != 0 {
                wire?.send(.gamepadButton(bit, down: newButtons & bit != 0, pad: slot.pad))
            }
            slot.buttons = newButtons
            // The stats chord, edge-triggered on the press that COMPLETES it: one cycle per
            // chord rather than one per press, since a third button pressed on top finds the
            // mask already complete and can't re-fire it. Read off the wire mask like the escape
            // chord, which means a Select the hold-Select gesture has turned into a guide is not
            // in it — a guide hold can't cycle the overlay on its way past. The buttons still
            // forward (the chord is a local overlay change, not an input the host must not see).
            if was & Self.statsChord != Self.statsChord,
               newButtons & Self.statsChord == Self.statsChord {
                StatsVerbosity.requestCycle(for: connection)
            }
        }
        for (i, v) in newAxes.enumerated() where v != slot.axes[i] {
            wire?.send(.gamepadAxis(UInt32(i), value: v, pad: slot.pad))
            slot.axes[i] = v
        }
        updateEscapeChord()
    }

    /// A sector, once engaged, keeps the stick until the angle is this far past its 30° edge — a
    /// thumb resting on the boundary between two slots would otherwise flicker between them.
    nonisolated static let sectorOverlapDeg = 5.0

    /// The ring slot the left stick points at, given the sector already engaged: past the dead
    /// zone by MAGNITUDE (a diagonal counts) the angle falls into one of six 60° sectors centred
    /// on the slots, slot `k` at `-90° + 60°·k`, 12 o'clock first, clockwise. The Swift half of
    /// `pf_client_core::menu_nav::ring_sector` — same 0.5 engage / 0.3 release thresholds, so the
    /// dial feels identical on a Mac and on a Steam Deck. GameController is +y = up.
    nonisolated static func ringSector(_ lx: Float, _ ly: Float, _ current: Int?) -> Int? {
        guard hypot(lx, ly) > (current == nil ? 0.5 : 0.3) else { return nil }
        // Degrees clockwise from 12 o'clock, so slot k's centre is at 60·k. atan2 spans
        // (-180°, 180°], so the +90 turn can only reach -90 — one wrap covers it.
        var deg = Double(atan2(-ly, lx)) * 180 / .pi + 90
        if deg < 0 { deg += 360 }
        if let k = current {
            // Signed distance from the engaged slot's centre, folded into ±180°.
            let off = (deg - 60 * Double(k) + 540).truncatingRemainder(dividingBy: 360) - 180
            if abs(off) <= 30 + sectorOverlapDeg { return k }
        }
        return Int((deg + 30) / 60) % 6
    }

    /// The six wire axes from a profile, in the wire's order and scale.
    private static func axesOf(_ g: GCExtendedGamepad) -> [Int32] {
        [
            Int32(g.leftThumbstick.xAxis.value * 32767),
            Int32(g.leftThumbstick.yAxis.value * 32767),
            Int32(g.rightThumbstick.xAxis.value * 32767),
            Int32(g.rightThumbstick.yAxis.value * 32767),
            Int32(g.leftTrigger.value * 255),
            Int32(g.rightTrigger.value * 255),
        ]
    }

    /// `Select+A`, Select first, with Select already on the wire (sent, or the guide gesture is
    /// off): the ring chord. A is withheld until it physically releases; the ring's flush lifts
    /// the Select. A Select still pending its guide hold is `gestureFiltered`'s case.
    private func ringChordFiltered(_ slot: Slot, _ rawIn: UInt32) -> UInt32 {
        var raw = rawIn
        if slot.swallowA {
            if raw & GamepadWire.a != 0 { raw &= ~GamepadWire.a } else { slot.swallowA = false }
        }
        let aPressed = raw & GamepadWire.a != 0 && slot.buttons & GamepadWire.a == 0
        if aPressed, !ringOpen, slot.buttons & GamepadWire.back != 0 {
            slot.swallowA = true
            onRingChord?(slot.pad)
            raw &= ~GamepadWire.a
        }
        return raw
    }

    /// The hold-Select→guide state machine over one sync's raw mask (pf-client-core's
    /// `SelectGesture` rules): Select pressed ALONE is suppressed while pending; another
    /// button joining makes it real (unsuppressed — the diff sends its down); released
    /// inside `guideHold` it's a tap, delivered out-of-band on release with the release
    /// `tapPress` behind; past the threshold `gestureHoldFired` turned it into a synthetic
    /// guide, lifted here when Select physically releases.
    ///
    /// One deliberate divergence from the Rust worker: while transformed into a guide the
    /// Select stays OUT of `slot.buttons`, so the escape chord doesn't complete on top of
    /// an in-flight guide-hold — release Select and press the chord plainly instead (the
    /// chord's four-at-once press never lingers in pending long enough to be affected).
    private func gestureFiltered(_ slot: Slot, _ rawIn: UInt32) -> UInt32 {
        let back = GamepadWire.back
        var raw = rawIn
        // A swallowed Select stays out of the mask until it physically releases: its press never
        // went out, so its release must not either.
        if slot.swallowSelect {
            if raw & back != 0 { raw &= ~back } else { slot.swallowSelect = false }
        }
        let backDown = raw & back != 0
        let othersDown = raw & ~back != 0
        if slot.selectAsGuide {
            if backDown { return raw & ~back }
            slot.selectAsGuide = false
            sendGuide(slot, down: false, raw: false)
            return raw
        }
        if slot.selectPending {
            if !backDown {
                endPending(slot)
                deliverTap(slot)
                return raw
            }
            if raw & GamepadWire.a != 0 {
                // `Select+A`, Select first: the ring chord — swallowed on both buttons.
                endPending(slot)
                slot.swallowSelect = true
                slot.swallowA = true
                onRingChord?(slot.pad)
                return raw & ~back & ~GamepadWire.a
            }
            if othersDown {
                // A combo after all — Select unsuppresses and the diff sends its down.
                endPending(slot)
                return raw
            }
            return raw & ~back
        }
        if backDown, !othersDown, slot.buttons & back == 0 {
            // Newly pressed, alone: hold it back. An owed tap release goes out first so
            // the host never sees two downs in a row.
            if slot.tapReleaseOwed { finishTap(slot) }
            slot.selectPending = true
            let timer = Timer(timeInterval: Self.guideHold, repeats: false) { [weak self, weak slot] _ in
                MainActor.assumeIsolated {
                    if let self, let slot { self.gestureHoldFired(slot) }
                }
            }
            RunLoop.main.add(timer, forMode: .common)
            slot.gestureTimer?.invalidate()
            slot.gestureTimer = timer
            return raw & ~back
        }
        return raw
    }

    /// The hold threshold passed with Select still pending → it IS the guide now, down
    /// until the physical release (`gestureFiltered`'s `selectAsGuide` branch lifts it).
    private func gestureHoldFired(_ slot: Slot) {
        guard slot.selectPending else { return }
        slot.selectPending = false
        slot.gestureTimer = nil
        slot.selectAsGuide = true
        sendGuide(slot, down: true, raw: false)
    }

    private func endPending(_ slot: Slot) {
        slot.selectPending = false
        slot.gestureTimer?.invalidate()
        slot.gestureTimer = nil
    }

    /// Deliver a held-back Select tap: the press now, its release `tapPress` behind. Both
    /// sends bypass `slot.buttons` (the raw mask no longer carries Select, so the diff
    /// stays consistent); `tapReleaseOwed` is what `flush` checks so the press can't
    /// outlive the slot.
    private func deliverTap(_ slot: Slot) {
        wire?.send(.gamepadButton(GamepadWire.back, down: true, pad: slot.pad))
        slot.tapReleaseOwed = true
        let timer = Timer(timeInterval: Self.tapPress, repeats: false) { [weak self, weak slot] _ in
            MainActor.assumeIsolated {
                if let self, let slot { self.finishTap(slot) }
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        slot.tapTimer?.invalidate()
        slot.tapTimer = timer
    }

    private func finishTap(_ slot: Slot) {
        guard slot.tapReleaseOwed else { return }
        slot.tapReleaseOwed = false
        slot.tapTimer?.invalidate()
        slot.tapTimer = nil
        wire?.send(.gamepadButton(GamepadWire.back, down: false, pad: slot.pad))
    }

    /// Forward the guide (Home/PS) transition directly — it's kept out of `buttonMask` (the legacy
    /// `buttonHome` element is unreliable). Folds into the slot's `buttons` so a held PS button is
    /// released by `flush` on focus loss / close just like the others. `raw: true` marks the
    /// physical Home handler's calls, which the system-buttons policy can keep local; the
    /// gesture's synthetic transitions pass `raw: false` and always go out.
    private func sendGuide(_ slot: Slot, down: Bool, raw: Bool = true) {
        if raw, !systemForward { return }
        guard live else { return }
        let bit = GamepadWire.guide
        let now = down ? (slot.buttons | bit) : (slot.buttons & ~bit)
        guard now != slot.buttons else { return }
        wire?.send(.gamepadButton(bit, down: down, pad: slot.pad))
        slot.buttons = now
    }

    private static func buttonMask(_ g: GCExtendedGamepad) -> UInt32 {
        var b: UInt32 = 0
        if g.dpad.up.isPressed { b |= GamepadWire.dpadUp }
        if g.dpad.down.isPressed { b |= GamepadWire.dpadDown }
        if g.dpad.left.isPressed { b |= GamepadWire.dpadLeft }
        if g.dpad.right.isPressed { b |= GamepadWire.dpadRight }
        if g.buttonMenu.isPressed { b |= GamepadWire.start }
        if g.buttonOptions?.isPressed == true { b |= GamepadWire.back }
        // The dedicated share/create/capture element (Xbox-Series Share, DualSense Create, a clone
        // pad's screenshot button — e.g. the GameSir G8's, below its d-pad) → the wire's capture
        // bit, matching the Rust client's `Button::Misc1 => wire::BTN_MISC1`. On an Xbox-Series pad
        // this is a button physically DISTINCT from View (buttonOptions, above), so it must not
        // collapse onto back — the host reads MISC1 as its own control (DualSense mute / Steam
        // quick-access). Caveat: a pad that surfaces ONE physical button as both buttonOptions and
        // this share element now emits back+misc1 for it — harmless on a plain xpad session (no
        // misc button) and rare otherwise. NOTE: on-glass verify on a real Xbox-Series pad.
        if g.buttons[GCInputButtonShare]?.isPressed == true { b |= GamepadWire.misc1 }
        if g.leftThumbstickButton?.isPressed == true { b |= GamepadWire.leftStickClick }
        if g.rightThumbstickButton?.isPressed == true { b |= GamepadWire.rightStickClick }
        if g.leftShoulder.isPressed { b |= GamepadWire.leftShoulder }
        if g.rightShoulder.isPressed { b |= GamepadWire.rightShoulder }
        // guide (Home/PS) is NOT read here — it's forwarded directly by the Home button's
        // pressedChangedHandler (the legacy `buttonHome` element is unreliable). See `openSlot`.
        if g.buttonA.isPressed { b |= GamepadWire.a }
        if g.buttonB.isPressed { b |= GamepadWire.b }
        if g.buttonX.isPressed { b |= GamepadWire.x }
        if g.buttonY.isPressed { b |= GamepadWire.y }
        if Self.touchpad(g)?.button.isPressed == true {
            b |= GamepadWire.touchpadClick
        }
        return b
    }

    /// The touchpad surface of a PlayStation pad — present on both `GCDualSenseGamepad` and
    /// `GCDualShockGamepad` (DualShock 4), which don't share a common touchpad type, so we
    /// downcast either and project the identical `touchpad*` properties. `nil` for any other
    /// controller (Xbox, MFi).
    private static func touchpad(
        _ g: GCExtendedGamepad
    ) -> (primary: GCControllerDirectionPad, secondary: GCControllerDirectionPad,
          button: GCControllerButtonInput)? {
        if let ds = g as? GCDualSenseGamepad {
            return (ds.touchpadPrimary, ds.touchpadSecondary, ds.touchpadButton)
        }
        if let ds4 = g as? GCDualShockGamepad {
            return (ds4.touchpadPrimary, ds4.touchpadSecondary, ds4.touchpadButton)
        }
        return nil
    }

    /// One touchpad finger moved on a slot's pad. GC reports ±1 positions and snaps to exactly
    /// (0, 0) on lift — treated as the lift signal (a real finger landing on the precise center
    /// momentarily reads as a lift; harmless for a 1-in-65k coincidence).
    private func touch(_ slot: Slot, finger: Int, x: Float, y: Float) {
        guard live else { return }
        let lifted = x == 0 && y == 0
        if lifted {
            if slot.fingerActive[finger] {
                slot.fingerActive[finger] = false
                wire?.sendTouchpad(pad: UInt8(slot.pad), finger: UInt8(finger), active: false, x: 0, y: 0)
            }
            return
        }
        slot.fingerActive[finger] = true
        let w = GamepadWire.touchpad(x: x, y: y)
        wire?.sendTouchpad(pad: UInt8(slot.pad), finger: UInt8(finger), active: true, x: w.x, y: w.y)
    }

    private func forwardMotion(_ slot: Slot, _ m: GCMotion) {
        guard live else { return }
        #if os(iOS)
        // While the phone-gyro mirror speaks for pad 0, the controller's own motion —
        // necessarily rotation-less, that's the engage condition — stays off the wire:
        // two writers on one pad's motion state would fight, and this accel-only stream
        // would keep stomping the mirror's gyro with zeros.
        if slot.pad == 0, deviceGyro?.isRunning == true { return }
        #endif
        // Every sample goes out. There used to be a 4 ms floor here, and it was a DROP: a sample
        // arriving 3.9 ms after the last one was discarded outright.
        //
        // That is the wrong shape for this signal. Buttons and sticks are absolute state, so a
        // dropped frame costs nothing — the next one says everything it would have. Angular
        // velocity is a RATE, and a consumer integrates it into an angle; a dropped sample is
        // rotation that happened and can never be recovered. GameController's delivery is jittery
        // around the pad's own ~250 Hz, so a floor set AT that rate does not shed a rare extra
        // sample, it sheds a steady fraction of every turn — and the error is one-signed, so it
        // accumulates into aim drifting short rather than into noise.
        //
        // Nothing needed the ceiling: GC delivers at the sensor's rate rather than faster, the SDL
        // client has always forwarded every sample, and the host's own idle watchdog runs on a
        // 100 ms timeout this cannot outpace. The throttle's `lastMotionNs`/`motionIntervalNs` went
        // with it rather than being left set-but-unread — nothing else consumed either.
        // Total acceleration in g: gravity + user when split, else the raw vector — then NEGATED
        // into the wire's convention.
        //
        // Apple reports acceleration as the gravity VECTOR: a device lying flat face-up reads
        // z = −1, because gravity points down. An accelerometer physically measures proper
        // acceleration, which at rest is the +1 g normal force pushing UP, and that is what a
        // DualSense's report — the wire's convention — carries. The two are exact negatives, so
        // every sample we sent was upside down, on both branches (`m.acceleration` follows the
        // same Apple convention as the gravity/user split).
        //
        // Measured on glass 2026-08-07 (G16): a DualSense flat and face-up, streamed from an
        // iPhone to a Linux host, arrived at hid-playstation as z = −0.99 g where +1.00 was owed.
        // Magnitude was 1.006 g, so the SCALE was already right — this is purely direction.
        // `rotationRate` is a true angular rate and needs no flip; the same session confirmed yaw
        // came through with the correct sign.
        let ax: Float
        let ay: Float
        let az: Float
        if m.hasGravityAndUserAcceleration {
            ax = -Float(m.gravity.x + m.userAcceleration.x)
            ay = -Float(m.gravity.y + m.userAcceleration.y)
            az = -Float(m.gravity.z + m.userAcceleration.z)
        } else {
            ax = -Float(m.acceleration.x)
            ay = -Float(m.acceleration.y)
            az = -Float(m.acceleration.z)
        }
        let gs = GamepadWire.gyroLSBPerRadS
        let as_ = GamepadWire.accelLSBPerG
        // Into the DualSense report frame. GameController and the pad's own report do not agree
        // about which slot is which axis — measured, both from the same controller, on 2026-08-07
        // — so forwarding GC's x/y/z straight through sent yaw where the game reads roll. See
        // `GamepadWire.appleMotionToWire`. One change of basis, applied to both planes.
        let g = GamepadWire.appleMotionToWire(
            (Float(m.rotationRate.x), Float(m.rotationRate.y), Float(m.rotationRate.z)))
        let a = GamepadWire.appleMotionToWire((ax, ay, az))
        let gyro = (
            GamepadWire.motionRaw(g.0, scale: gs),
            GamepadWire.motionRaw(g.1, scale: gs),
            GamepadWire.motionRaw(g.2, scale: gs)
        )
        let accel = (
            GamepadWire.motionRaw(a.0, scale: as_),
            GamepadWire.motionRaw(a.1, scale: as_),
            GamepadWire.motionRaw(a.2, scale: as_)
        )
        // Recorded AFTER the frame conversion, deliberately: `flush` replays `lastAccel` beside a
        // zero gyro, so it has to be the vector that actually went on the wire. Stashing the
        // pre-conversion one would park a still pad's gravity in the wrong axis.
        if wire != nil {
            slot.motionSent = true
            slot.lastAccel = accel
        }
        wire?.sendMotion(pad: UInt8(slot.pad), gyro: gyro, accel: accel)
    }

    /// Engage or stand down the phone-gyro mirror: it speaks for pad 0 exactly while a
    /// forwarded controller holds that index but can't rotate for itself — no `GCMotion`,
    /// or a motion object without a rotation rate (gravity-only pads, e.g. an Xbox pad on
    /// iOS). Re-evaluated on every reconcile and on suspend/resume; `DeviceGyro.stop`
    /// parks the host pad's rotation at zero, so standing down never strands a spin.
    private func updateDeviceGyro() {
        #if os(iOS)
        guard let gyro = deviceGyro else { return }
        let pad0 = slots.first { $0.pad == 0 }
        let wants = live && pad0 != nil && pad0!.controller.motion?.hasRotationRate != true
        if wants { gyro.start() } else { gyro.stop() }
        #endif
    }

    /// Arm the disconnect timer when ANY forwarded pad holds the full escape chord, disarm the
    /// moment none do — a release, or the holding pad unplugged (pf-client-core's `chord_held` is
    /// likewise any-slot). GC events only arrive on state CHANGES, so a held chord needs the timer:
    /// the handler won't fire again until something moves.
    private func updateEscapeChord() {
        let held = slots.contains { $0.buttons & Self.escapeChord == Self.escapeChord }
        if held, chordTimer == nil {
            let timer = Timer(timeInterval: Self.disconnectHold, repeats: false) { [weak self] _ in
                MainActor.assumeIsolated { self?.onDisconnectRequest?() }
            }
            RunLoop.main.add(timer, forMode: .common)
            chordTimer = timer
        } else if !held, chordTimer != nil {
            chordTimer?.invalidate()
            chordTimer = nil
        }
    }

    /// Unwind everything a slot holds on the wire: button-ups, neutral axes, lifted fingers. The
    /// host's virtual pad returns to rest instead of running with the last state. Wire events only
    /// (no GC calls) — safe against an already-removed device. Does NOT close the slot or send
    /// GamepadRemove (that's `closeSlot`).
    private func flush(_ slot: Slot) {
        // Gesture first: a pending (never-sent) Select just drops, an owed tap release
        // goes out, and a transformed guide's bit — folded into `buttons` by `sendGuide`
        // — is lifted by the loop below like any held button.
        endPending(slot)
        slot.selectAsGuide = false
        if slot.tapReleaseOwed { finishTap(slot) }
        for bit in GamepadWire.allButtons where slot.buttons & bit != 0 {
            wire?.send(.gamepadButton(bit, down: false, pad: slot.pad))
        }
        slot.buttons = 0
        for (i, v) in slot.axes.enumerated() where v != 0 {
            wire?.send(.gamepadAxis(UInt32(i), value: 0, pad: slot.pad))
            slot.axes[i] = 0
        }
        for (f, active) in slot.fingerActive.enumerated() where active {
            wire?.sendTouchpad(pad: UInt8(slot.pad), finger: UInt8(f), active: false, x: 0, y: 0)
            slot.fingerActive[f] = false
        }
        // Motion is host-side STATE, re-emitted until replaced — a nonzero angular velocity
        // left behind reads as endless rotation (the gyro-sweep latch: Control Center
        // pull-down froze the last sample for as long as the overlay stayed up). Rest means
        // zero rotation; the last accel is kept so gravity doesn't become free-fall.
        if slot.motionSent {
            slot.motionSent = false
            wire?.sendMotion(pad: UInt8(slot.pad), gyro: (0, 0, 0), accel: slot.lastAccel)
        }
    }

    /// Flush every open slot's held state (app deactivation) — keeps the slots open (GC just stops
    /// delivering; resume re-syncs), disarms the escape chord. Distinct from `closeAllSlots`, which
    /// also sends GamepadRemove and detaches handlers.
    private func releaseAll() {
        chordTimer?.invalidate()
        chordTimer = nil
        for slot in slots { flush(slot) }
    }
}
