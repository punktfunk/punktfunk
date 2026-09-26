import CoreHaptics
import Foundation
import GameController
import os

private let log = ClientLog(category: "gamepad")

/// Tuning constants + the pure scheduling decisions of the rumble renderer, split out so the
/// policy is unit-testable without a `CHHapticEngine` or a physical pad.
enum RumbleTuning {
    /// Haptic segment length. **No event is ever infinite**: a player the renderer loses track
    /// of (a stop dropped inside CoreHaptics, an engine race) self-silences when its segment
    /// expires, so this is the hard ceiling on how long the actuator can diverge from the
    /// target state.
    static let segmentSeconds: TimeInterval = 4.0
    /// Re-arm the successor segment once the current one has less than this left. Generous
    /// against the ticker period so a steady rumble can never miss the boundary and gap.
    static let rearmHeadroom: TimeInterval = 1.0
    /// Renderer ticker period while anything is (or should be) audible. Silence runs no timer.
    static let tickSeconds: TimeInterval = 0.05
    /// Minimum spacing between player rebuilds for nonzero→nonzero level changes — a game
    /// ramping rumble per frame would otherwise stop/start players at 60+ Hz, which is exactly
    /// the churn that lost stops inside CoreHaptics. Newest level wins when the window opens;
    /// zero is never throttled.
    static let minRebakeSeconds: TimeInterval = 0.025
    /// Levels closer than this (≈0.4 % of full scale) are the same level — an identical host
    /// refresh must never rebuild a player.
    static let levelEpsilon: Float = 1.0 / 256.0
    /// macOS DualSense raw-HID path: re-write an unchanged nonzero level this often so the
    /// pad's firmware never times the rumble out mid-effect (Bluetooth pads watchdog output
    /// reports), and a dropped report heals.
    static let hidKeepaliveSeconds: TimeInterval = 0.9

    /// `CHHapticEvent` sharpness = actuator frequency. A DualSense's voice-coil motors need a
    /// defined frequency to move at all (an intensity-only event left them silent) while a
    /// classic Xbox ERM rotor ignores it. On split-handle pads the wire's two motors render at
    /// distinct frequencies mirroring the real hardware they emulate — low/left ≈ the heavy
    /// low-frequency rotor, high/right ≈ the light buzzer; a single combined actuator keeps the
    /// proven mid value. The impulse-trigger motors are small and light — the same character as
    /// the high/right buzzer — so they reuse `sharpnessHigh` rather than introduce a number
    /// nobody has measured on real trigger hardware.
    static let sharpnessLow: Float = 0.3
    static let sharpnessHigh: Float = 0.7
    static let sharpnessCombined: Float = 0.5

    /// Wire amplitude (0...0xFFFF) → CoreHaptics intensity (0...1).
    static func amplitude(_ wire: UInt16) -> Float { Float(wire) / 65535 }
    /// Wire amplitude → DualSense HID motor byte. A nonzero command never collapses to silence:
    /// the top byte of anything below 0x0100 is 0, so a weak-but-real rumble used to render as
    /// nothing at all on this path. Floored at 1 — imperceptibly light, but moving. (Android's
    /// `toAmplitude` has always done this; this was the odd one out.)
    static func hidByte(_ wire: UInt16) -> UInt8 {
        let b = UInt8(wire >> 8)
        return wire != 0 && b == 0 ? 1 : b
    }
    /// Single-actuator pads render whichever motor is stronger.
    static func combined(low: UInt16, high: UInt16) -> UInt16 { max(low, high) }
    /// Are two baked levels the same (skip the rebuild)?
    static func sameLevel(_ a: Float, _ b: Float) -> Bool { abs(a - b) <= levelEpsilon }
    /// Time for a segment handoff to act (engine timeline).
    static func shouldRearm(endsAt: TimeInterval, now: TimeInterval) -> Bool {
        endsAt - now <= rearmHeadroom
    }
    /// When the successor segment starts: exactly as the current one expires — unless that
    /// already passed (the gap already happened; start now).
    static func handoffStart(endsAt: TimeInterval, now: TimeInterval) -> TimeInterval {
        max(endsAt, now)
    }
}

/// Rumble → the active physical controller (CoreHaptics; a DualSense on macOS goes over raw HID
/// instead, see `DualSenseHID`), built around one principle: **rumble is idempotent state on a
/// lossy channel, and the actuator's divergence from that state must be bounded** — not
/// best-effort. The previous renderer drove infinite-duration players torn down and rebuilt per
/// wire update; one asynchronous `stop` dropped inside CoreHaptics left an unstoppable player
/// buzzing with its handle discarded, which no later (0,0) could reach — the "walked into the
/// menu and the rumble never stopped" bug.
///
/// The invariants that bound divergence now:
///  1. **No infinite events.** A motor plays finite `segmentSeconds` segments; while the level
///     holds, the successor is scheduled ON the engine timeline to start exactly when the
///     current segment expires (seamless — no stop/start race in steady state). A leaked player
///     therefore self-silences in ≤ `segmentSeconds`.
///  2. **Idempotent targets.** An update equal to the current target (the host re-sends rumble
///     state every 500 ms as its loss heal) is a liveness stamp, never a player rebuild.
///  3. **Zero is immediate, ramps are throttled.** (0,0) stops players the moment it lands;
///     nonzero→nonzero changes rebuild at most every `minRebakeSeconds` per motor (the ticker
///     lands the newest value once the window opens).
///  4. **Escalating stop.** A throwing `player.stop` means the engine's state is unknown — the
///     whole engine is stopped (silencing every player it hosts) and lazily rebuilt behind the
///     exponential backoff.
///  5. **No staleness watchdog here.** There was one, keyed off a `Policy` type and a
///     `sessionStaleSeconds`; both are gone. Every liveness decision — lease expiry, legacy-host
///     staleness, session close — now belongs to punktfunk-core's shared policy engine
///     (`client/rumble.rs`), which emits explicit zero commands, so this renderer applies what it
///     is told and never decides on its own when a level should end.
///
/// Engines are created lazily on the first nonzero amplitude and torn down on retarget;
/// failures (pads without haptics, engine resets) downgrade to silence — rumble is best-effort
/// by design, but *staying silent* when told to stop is not.
///
/// `@unchecked Sendable` is sound because every property is read and written only inside
/// `queue` closures — the serial queue is the synchronization.
final class RumbleRenderer: @unchecked Sendable {
    /// Which physical actuator this renderer drives: the forwarded controller's haptics engine
    /// (the default), or THIS device's own Taptic Engine (`CHHapticEngine()`) — the opt-in
    /// "rumble on this device" mirror for phone-clip pads that ship without rumble motors.
    /// Device mode ignores `retarget`'s controller and always renders one combined motor
    /// (a phone body has a single actuator).
    enum Actuator {
        case controller
        case device
    }

    private let queue = DispatchQueue(label: "io.unom.punktfunk.haptics", qos: .userInteractive)
    private let actuator: Actuator

    /// One finite haptic play on a motor: the player plus when (engine timeline) it expires.
    /// A PLAIN pattern player on purpose: the controller haptics server (gamecontrollerd)
    /// advertises `adv players: 0`, and as of iOS 27 beta 2 an advanced-player sequence load
    /// doesn't degrade gracefully there — the daemon faults decoding the XPC message and drops
    /// it (CoreHaptics -4811/4097, rumble dead). We only need `start(atTime:)`/`stop(atTime:)`,
    /// which the plain protocol has.
    private struct Segment {
        let player: CHHapticPatternPlayer
        let endsAt: TimeInterval
    }

    /// One actuator's started engine and the segment(s) realizing `level` on it. `retiring` is
    /// the predecessor across a segment handoff — left to expire naturally (its successor
    /// starts the instant it ends), but the reference is held so a level change or stop can
    /// still force-stop it.
    private struct Motor {
        let engine: CHHapticEngine
        let sharpness: Float
        var level: Float = 0
        var current: Segment?
        var retiring: Segment?
        var lastRebake = DispatchTime(uptimeNanoseconds: 0)
    }

    private var controller: GCController?
    private var low: Motor?
    private var high: Motor?
    /// The two Xbox impulse-trigger motors, when the pad offers
    /// `GCHapticsLocality.leftTrigger`/`.rightTrigger`. **Nil is the normal case** — every pad but
    /// an Xbox One/Series/Elite has no such actuator, and the tree has already observed Xbox pads
    /// on Apple exposing no haptics engine at all — so their absence is never logged and never
    /// counts as a setup failure. Independent of the handle split: a pad may offer trigger
    /// localities with or without split handles, and losing one does not implicate the other.
    private var leftTrigger: Motor?
    private var rightTrigger: Motor?
    /// Wire-truth target (raw wire units) — the engine command's four levels, applied verbatim;
    /// the core policy engine owns when it ends (explicit zero commands), so no deadline lives
    /// here. The trigger levels are only ever non-zero against a Windows HID Xbox host pad; every
    /// other backend on every OS lacks the channel entirely (XInput's `XINPUT_VIBRATION` and
    /// evdev's `FF_RUMBLE` each carry exactly two magnitudes).
    private var target: (low: UInt16, high: UInt16, leftTrigger: UInt16, rightTrigger: UInt16) =
        (0, 0, 0, 0)
    /// Runs while anything is (or should be) audible: staleness watchdog, segment re-arm,
    /// throttled-level catch-up, engine rebuild after a reset, HID keepalive. Nil while silent,
    /// so an idle controller costs no timer wakeups and no radio traffic.
    private var ticker: DispatchSourceTimer?

    // `broken` latches OFF only for a controller that genuinely has no haptics engine (an Xbox pad
    // on an OS that doesn't expose rumble through GameController, a Siri Remote) — nothing to retry
    // until the controller changes. A transient engine failure does NOT latch it; it tears down for
    // a lazy rebuild instead, so a single hiccup can't kill rumble for the whole session.
    private var broken = false
    /// Last logged active/silent state — for a one-line transition log, not per-event spam.
    private var wasActive = false
    // Backoff after an engine failure. A broken `gamecontrollerd.haptics` XPC connection (CoreHaptics
    // -4811 "server connection broke") fails EVERY rebuild until the service relaunches — and that
    // break fires neither stoppedHandler nor resetHandler, so without a cooldown the next rumble
    // update immediately rebuilds into the same dead connection, flooding the log and never
    // recovering. Delay the next setup() — growing 0.5→1→2→4 s on repeated failure — and clear it
    // the moment a player is actually running (or the controller changes).
    private var retryAfter = DispatchTime(uptimeNanoseconds: 0)
    private var consecutiveFailures = 0
    /// Downgrade after split-handle engines fail: retry with ONE combined `.default` engine —
    /// the configuration virtually every iOS game (and this app's own menu haptics) uses — before
    /// treating the service as unreachable. A haptics daemon that mishandles per-handle
    /// localities for a particular pad can still serve the combined engine. One-way per
    /// controller; retarget resets it.
    private var preferCombined = false
    /// Health reporting for the debug test panel: a human-readable problem while rumble cannot
    /// work (nil = healthy). Without this, a wedged system haptics service (gamecontrollerd
    /// refusing every XPC connection — CoreHaptics -4811/4097, which no in-app retry can fix)
    /// reads as "the app's rumble is broken" when actually no app on the device can rumble.
    private var healthSink: ((String?) -> Void)?
    private var lastHealth: String?

    #if os(macOS)
    /// Set when the active pad is a DualSense: its motors are driven over raw HID (CoreHaptics
    /// does not reach them on macOS — adaptive triggers/lightbar work, rumble is silent). nil for
    /// every other controller, which keeps the CoreHaptics path.
    private var dualSenseHID: DualSenseHID?
    private var lastHidWrite: (levels: (UInt8, UInt8), at: DispatchTime) =
        ((0, 0), DispatchTime(uptimeNanoseconds: 0))
    #endif

    init(actuator: Actuator = .controller) {
        self.actuator = actuator
    }

    /// `onBackend`, if given, is invoked (on the internal queue) with a human-readable name of the
    /// rumble backend now in use; `onHealth` with a problem description whenever rumble transitions
    /// between working and structurally failing (nil = healthy) — both for the debug test panel.
    func retarget(
        _ c: GCController?, onBackend: ((String) -> Void)? = nil,
        onHealth: ((String?) -> Void)? = nil
    ) {
        queue.async {
            self.teardown()
            self.closeHID()
            // A RELEASE clears the level; a swap keeps it (see `render` below). `updateTicker`
            // keys off `target`, so a non-zero one left behind here spins a 50 ms timer against
            // no controller for the rest of the process, and deallocates an active source.
            if c == nil { self.target = (0, 0, 0, 0) }
            self.controller = c
            self.broken = false
            self.preferCombined = false
            self.consecutiveFailures = 0
            self.retryAfter = DispatchTime(uptimeNanoseconds: 0)
            if let onHealth { self.healthSink = onHealth }
            self.lastHealth = nil
            self.healthSink?(nil)
            _ = self.openHIDIfDualSense(c)
            onBackend?(self.backendNote(for: c))
            // The target survives a SWAP: render replays the current level onto the new pad right
            // away, so a mid-rumble controller change keeps rumbling.
            self.render()
        }
    }

    /// Set the wire-truth target: one policy-engine command's four motor levels, applied verbatim.
    /// Called with every 0xCA state the host sends — level changes AND renewals — and the core
    /// engine owns when a level ends (it emits explicit zero commands), so nothing here decides.
    ///
    /// `leftTrigger`/`rightTrigger` are the Xbox impulse-trigger motors. They default to zero so
    /// handle-only callers (the debug test panel, the tuning tests) read unchanged, which is also
    /// the wire's own rule: on a level-triggered plane an absent level is off, never "keep what
    /// you had".
    func apply(
        low lowAmp: UInt16, high highAmp: UInt16, leftTrigger ltAmp: UInt16 = 0,
        rightTrigger rtAmp: UInt16 = 0
    ) {
        queue.async {
            let next = (lowAmp, highAmp, ltAmp, rtAmp)
            let active = next != (0, 0, 0, 0)
            if active != self.wasActive {
                self.wasActive = active
                log.debug(
                    "rumble: \(active ? "active" : "stop", privacy: .public) low=\(lowAmp, privacy: .public) high=\(highAmp, privacy: .public) lt=\(ltAmp, privacy: .public) rt=\(rtAmp, privacy: .public)")
            }
            guard next != self.target else { return }
            self.target = next
            self.render()
        }
    }

    /// Silence the motors and drop the engines. Blocks until done — call off the main actor.
    func stop() {
        queue.sync {
            self.ticker?.cancel()
            self.ticker = nil
            self.target = (0, 0, 0, 0)
            self.wasActive = false
            self.teardown()
            self.closeHID()
        }
    }

    // MARK: - Reconciliation (all on `queue`)

    /// Drive the actuators toward `target`. Idempotent — safe to call from every wire update,
    /// tick, and retarget; when everything already matches it does nothing.
    private func render() {
        defer { updateTicker() }
        if renderHID() { return }
        guard !broken else { return }
        let audible = target != (0, 0, 0, 0)
        if audible, low == nil, high == nil, DispatchTime.now() >= retryAfter {
            setup()
        }
        // Reconcile BOTH motors (no short-circuit skipping the second on a first-motor error),
        // and tear down OUTSIDE the `inout` accesses so teardown() never mutates a motor a
        // reconcile call still holds an exclusive reference to.
        let ok: Bool
        if high != nil {
            // Per-handle: low = left/heavy motor, high = right/light — the XInput convention
            // the wire carries.
            let okLow = reconcile(&low, to: RumbleTuning.amplitude(target.low))
            let okHigh = reconcile(&high, to: RumbleTuning.amplitude(target.high))
            ok = okLow && okHigh
        } else {
            let mixed = RumbleTuning.combined(low: target.low, high: target.high)
            ok = reconcile(&low, to: RumbleTuning.amplitude(mixed))
        }
        // Impulse triggers: rendered ONLY where the hardware has the actuators, never folded into
        // the handles. `reconcile` on a nil slot is a no-op returning true, so a pad without them
        // silently drops the levels — which is the correct degrade and the common case.
        //
        // Their outcome is deliberately kept OUT of `ok`: a trigger engine erroring must not tear
        // down the handle engines (which are what the pad's rumble mostly is) nor flip
        // `preferCombined`, which is a statement about the handle split and nothing else. Nothing
        // is orphaned by that — a failed reconcile leaves the slot's Motor in place, so the next
        // tick simply retries it, and an engine that is genuinely dead fires its
        // stopped/reset handler, which tears down all four slots for a lazy rebuild.
        _ = reconcile(&leftTrigger, to: RumbleTuning.amplitude(target.leftTrigger))
        _ = reconcile(&rightTrigger, to: RumbleTuning.amplitude(target.rightTrigger))
        if !ok {
            let wasSplit = high != nil
            teardown()
            scheduleRetryBackoff()
            if wasSplit, !preferCombined {
                preferCombined = true
                log.info("rumble: split-handle engines failing — will retry with one combined engine")
            }
        } else if low?.current != nil || high?.current != nil {
            // A player is actually running — the path has recovered; clear the backoff.
            consecutiveFailures = 0
            retryAfter = DispatchTime(uptimeNanoseconds: 0)
            reportHealth(nil)
        }
    }

    /// Publish a health transition to the test panel (deduped — transitions only).
    private func reportHealth(_ problem: String?) {
        guard problem != lastHealth else { return }
        lastHealth = problem
        healthSink?(problem)
    }

    /// Housekeeping heartbeat while audible: segment re-arm, HID keepalive, backoff retries.
    /// Every liveness decision (lease expiry, legacy-host staleness, session close) lives in the
    /// core policy engine now — it emits explicit zero commands, so the renderer never guesses
    /// when a level should end.
    private func tick() {
        render()
    }

    /// Drive one motor toward `desired`, per the invariants above. Returns false when the
    /// engine errored — the caller then tears everything down (outside this `inout` access) for
    /// a lazy, backoff-gated rebuild.
    private func reconcile(_ slot: inout Motor?, to desired: Float) -> Bool {
        guard var m = slot else { return true }
        defer { slot = m }
        // Release a handed-off predecessor once it has expired on its own.
        if let r = m.retiring, m.engine.currentTime >= r.endsAt + 0.25 {
            m.retiring = nil
        }
        if desired <= RumbleTuning.levelEpsilon {
            guard m.level > 0 || m.current != nil || m.retiring != nil else { return true }
            m.level = 0
            return stopSegments(&m)
        }
        if RumbleTuning.sameLevel(desired, m.level), m.current != nil {
            return rearmIfNeeded(&m)
        }
        // Nonzero level change. Throttled: the ticker re-runs render() and lands the newest
        // value once the window opens (zero above is never throttled).
        if m.current != nil, seconds(since: m.lastRebake) < RumbleTuning.minRebakeSeconds {
            return true
        }
        guard stopSegments(&m) else { return false }
        do {
            m.current = try makeSegment(
                m.engine, sharpness: m.sharpness, amplitude: desired, at: CHHapticTimeImmediate)
            m.level = desired
            m.lastRebake = .now()
            return true
        } catch {
            // A transient failure (the engine stopped/reset between its handler firing and now).
            // Signal a rebuild — do NOT latch rumble off for the session.
            log.warning("rumble: haptic start failed — rebuilding: \(error, privacy: .public)")
            return false
        }
    }

    /// Keep a steady level seamless across the finite-segment boundary: when the current
    /// segment nears its end, start the successor ON the engine timeline exactly as it expires
    /// — no stop call, no race, no gap. The old segment is kept as `retiring` until it dies
    /// naturally, so a level change can still force-stop it.
    private func rearmIfNeeded(_ m: inout Motor) -> Bool {
        guard let cur = m.current else { return true }
        let now = m.engine.currentTime
        guard RumbleTuning.shouldRearm(endsAt: cur.endsAt, now: now) else { return true }
        // A predecessor still held this deep into the segment already expired; drop it.
        m.retiring = nil
        do {
            let next = try makeSegment(
                m.engine, sharpness: m.sharpness, amplitude: m.level,
                at: RumbleTuning.handoffStart(endsAt: cur.endsAt, now: now))
            m.retiring = m.current
            m.current = next
            return true
        } catch {
            log.warning("rumble: segment re-arm failed — rebuilding: \(error, privacy: .public)")
            return false
        }
    }

    /// Stop every segment on the motor NOW. False = a stop threw, so the engine's real state is
    /// unknown (a player may still run with its handle gone) — the caller must escalate to a
    /// full engine teardown, whose `engine.stop()` silences every player the engine hosts.
    private func stopSegments(_ m: inout Motor) -> Bool {
        var ok = true
        for seg in [m.current, m.retiring].compactMap({ $0 }) {
            do {
                try seg.player.stop(atTime: CHHapticTimeImmediate)
            } catch {
                log.warning(
                    "rumble: player stop failed — escalating to engine stop: \(error, privacy: .public)")
                ok = false
            }
        }
        m.current = nil
        m.retiring = nil
        return ok
    }

    /// Build + start one finite continuous event at `amplitude`. `at` is `CHHapticTimeImmediate`
    /// or an absolute engine-timeline instant (a scheduled handoff). The intensity is BAKED into
    /// the event: a fixed event scaled by a dynamic `.hapticIntensityControl` parameter drives
    /// the iPhone Taptic Engine but is silent on a controller's haptic engine.
    private func makeSegment(
        _ engine: CHHapticEngine, sharpness: Float, amplitude: Float, at start: TimeInterval
    ) throws -> Segment {
        let event = CHHapticEvent(
            eventType: .hapticContinuous,
            parameters: [
                CHHapticEventParameter(parameterID: .hapticIntensity, value: amplitude),
                CHHapticEventParameter(parameterID: .hapticSharpness, value: sharpness),
            ],
            relativeTime: 0,
            duration: RumbleTuning.segmentSeconds)
        let player = try engine.makePlayer(
            with: CHHapticPattern(events: [event], parameters: []))
        try player.start(atTime: start)
        let begins = start == CHHapticTimeImmediate ? engine.currentTime : start
        return Segment(player: player, endsAt: begins + RumbleTuning.segmentSeconds)
    }

    /// The ticker runs only while something needs tending — any nonzero target (watchdog,
    /// throttle catch-up, HID keepalive, post-reset engine rebuild) or segments still alive.
    private func updateTicker() {
        let needed = target != (0, 0, 0, 0)
            || low?.current != nil || low?.retiring != nil
            || high?.current != nil || high?.retiring != nil
            || leftTrigger?.current != nil || leftTrigger?.retiring != nil
            || rightTrigger?.current != nil || rightTrigger?.retiring != nil
        if needed, ticker == nil {
            let t = DispatchSource.makeTimerSource(queue: queue)
            t.schedule(
                deadline: .now() + RumbleTuning.tickSeconds, repeating: RumbleTuning.tickSeconds)
            t.setEventHandler { [weak self] in self?.tick() }
            t.resume()
            ticker = t
        } else if !needed, let t = ticker {
            t.cancel()
            ticker = nil
        }
    }

    // MARK: - Engine lifecycle

    /// Engines per handle when the pad distinguishes them (low = left/heavy motor,
    /// high = right/light — the Xbox/XInput convention the wire carries); one combined
    /// engine otherwise, driven by whichever amplitude is stronger.
    private func setup() {
        if actuator == .device {
            setupDevice()
            return
        }
        guard let haptics = controller?.haptics else {
            // No haptics engine at all — an Xbox controller on an OS/firmware that doesn't expose
            // rumble through GameController (works on Android via the standard Vibrator path, but
            // Apple's support is controller/OS-dependent), or a Siri Remote. Nothing to retry until
            // the controller changes; latch off (retarget clears it) and say so once.
            log.info("rumble: active controller exposes no haptics engine — rumble unavailable")
            broken = true
            reportHealth("This controller exposes no rumble engine to apps on this OS.")
            return
        }
        let localities = haptics.supportedLocalities
        let split =
            !preferCombined && localities.contains(.leftHandle)
            && localities.contains(.rightHandle)
        if split {
            low = makeMotor(haptics, .leftHandle, sharpness: RumbleTuning.sharpnessLow)
            high = makeMotor(haptics, .rightHandle, sharpness: RumbleTuning.sharpnessHigh)
            // HALF a split is worse than none, and it used to pass silently: only the all-nil case
            // below counts as failure, so one surviving handle left `ok` true and `reportHealth(nil)`
            // announced HEALTHY. What actually rendered was wrong in a direction that depends on
            // which handle died — lose `high` and `render` falls to the combined branch (selected
            // purely by `high != nil`), playing max(low, high) on the LEFT handle at the combined
            // sharpness; lose `low` and the split branch's reconcile no-ops on the nil slot, so the
            // heavy motor is discarded outright. Tear the survivor down and take the combined path,
            // which at least renders both motors somewhere.
            if low == nil || high == nil {
                log.warning("rumble: only one split-handle engine came up — falling back to combined")
                teardown() // disarms handlers, stops the survivor's players + engine, nils both
            }
        } else {
            low = makeMotor(haptics, .default, sharpness: RumbleTuning.sharpnessCombined)
        }
        if low == nil, high == nil {
            // Haptics present but no engine could be built right now (server busy / XPC broken). Do
            // NOT latch broken — back off and a later render past the cooldown retries.
            log.warning("rumble: haptics present but engine setup failed — backing off, will retry")
            scheduleRetryBackoff()
            if split {
                preferCombined = true
                log.info("rumble: split-handle engines failing — will retry with one combined engine")
            }
            // Return before the trigger engines: the retry path re-enters setup() on the same
            // `low == nil, high == nil` condition, so building them here would leak a fresh pair
            // on every attempt (teardown() only runs on the failure paths above, and this is not
            // one of them).
            return
        }
        // Impulse-trigger motors, built last and best-effort. Independent of the handle split —
        // the localities are separate and a pad can offer either, both or neither — and NOT part
        // of the failure test above: nil here is the ordinary state of every pad that is not an
        // Xbox One/Series/Elite, so it must not read as "engine setup failed", back off the handle
        // engines, or produce a log line on a path that runs per controller attach.
        //
        // Whether a given pad + OS pair actually reports these localities is UNVERIFIED on glass.
        // The degrade needs no code: `createEngine(withLocality:)` returns nil, the slots stay nil,
        // and `reconcile` no-ops on them.
        if localities.contains(.leftTrigger) {
            leftTrigger = makeMotor(haptics, .leftTrigger, sharpness: RumbleTuning.sharpnessHigh)
        }
        if localities.contains(.rightTrigger) {
            rightTrigger = makeMotor(haptics, .rightTrigger, sharpness: RumbleTuning.sharpnessHigh)
        }
    }

    /// Push the next engine-build attempt out after a failure (capped exponential backoff), so a
    /// broken `gamecontrollerd.haptics` connection gets time to relaunch instead of being re-hit on
    /// every rumble update.
    private func scheduleRetryBackoff() {
        consecutiveFailures += 1
        let shift = min(consecutiveFailures - 1, 4)
        retryAfter = .now() + min(0.5 * Double(1 << shift), 4)
        if consecutiveFailures >= 2 {
            // One failure is a hiccup; repeated ones are the wedged-service signature (every
            // XPC connection to gamecontrollerd.haptics breaks — no app on the device can
            // rumble until it relaunches). Say so instead of failing silently.
            reportHealth(
                "The system haptics service is refusing connections — no app can rumble a "
                    + "controller right now. Rebooting the device usually clears it.")
        }
    }

    /// Device-actuator mode: one combined motor on this device's own Taptic Engine. Only an
    /// iPhone has one — everything else (iPad, Mac, TV) reports no haptic hardware and latches
    /// off (nothing to retry; the settings toggle is hidden there anyway, this is the backstop).
    private func setupDevice() {
        #if os(iOS)
        guard CHHapticEngine.capabilitiesForHardware().supportsHaptics else {
            log.info("rumble: this device has no haptic actuator — device rumble unavailable")
            broken = true
            reportHealth("This device has no haptic actuator.")
            return
        }
        do {
            low = startMotor(try CHHapticEngine(), sharpness: RumbleTuning.sharpnessCombined)
        } catch {
            log.warning("rumble: device haptic engine creation failed: \(error, privacy: .public)")
        }
        if low == nil {
            // Same shape as the controller path: haptics exist but the engine couldn't be built
            // right now — back off and retry, don't latch off.
            scheduleRetryBackoff()
        }
        #else
        broken = true
        #endif
    }

    private func makeMotor(
        _ haptics: GCDeviceHaptics, _ locality: GCHapticsLocality, sharpness: Float
    ) -> Motor? {
        guard let engine = haptics.createEngine(withLocality: locality) else { return nil }
        return startMotor(engine, sharpness: sharpness)
    }

    /// Configure + start an engine (controller-locality or the device's own) into a [`Motor`].
    private func startMotor(_ engine: CHHapticEngine, sharpness: Float) -> Motor? {
        // A controller's motors carry no audio, so keep this engine OUT of the app's audio session
        // (the default is to join it). Streaming keeps an AVAudioSession active the whole time;
        // letting a haptics-only engine join it is a needless coupling that can get its
        // gamecontrollerd XPC connection interrupted (the repeated -4811 server-connection breaks).
        engine.playsHapticsOnly = true
        // The haptic server can stop or reset the engine out from under us — app backgrounding, an
        // audio-session interruption (a call, Siri, another audio app), or a server crash. Left
        // unhandled the players go dead and every later rumble throws, latching rumble off for the
        // rest of the session (the "rumble worked, then went spotty" failure). Tear down on the
        // serial queue; the ticker (or the next wire update) lazily rebuilds the engine and
        // re-renders the still-current target.
        engine.stoppedHandler = { [weak self] reason in
            log.info("rumble: haptic engine stopped (reason \(reason.rawValue, privacy: .public)) — will rebuild")
            self?.queue.async { self?.teardown() }
        }
        engine.resetHandler = { [weak self] in
            log.info("rumble: haptic engine reset — will rebuild")
            self?.queue.async { self?.teardown() }
        }
        do {
            // Start the engine now; the players that actually move the motor are the finite
            // segments `reconcile` bakes per level.
            try engine.start()
            return Motor(engine: engine, sharpness: sharpness)
        } catch {
            log.warning("haptic engine setup failed: \(error, privacy: .public)")
            return nil
        }
    }

    private func teardown() {
        for m in [low, high, leftTrigger, rightTrigger].compactMap({ $0 }) {
            // Disarm the handlers before stopping so stop() can't re-enter teardown via them.
            // (Both properties are non-optional closures on this SDK, so assign no-ops, not nil.)
            m.engine.stoppedHandler = { _ in }
            m.engine.resetHandler = {}
            for seg in [m.current, m.retiring].compactMap({ $0 }) {
                try? seg.player.stop(atTime: CHHapticTimeImmediate)
            }
            // The authoritative silencer: a stopped engine plays nothing, including any player
            // whose individual stop was dropped.
            m.engine.stop()
        }
        low = nil
        high = nil
        leftTrigger = nil
        rightTrigger = nil
    }

    private func seconds(since t: DispatchTime) -> TimeInterval {
        TimeInterval(DispatchTime.now().uptimeNanoseconds - t.uptimeNanoseconds) / 1_000_000_000
    }

    // MARK: - DualSense raw-HID rumble (macOS)
    //
    // On macOS the DualSense's motors aren't reachable through CHHapticEngine, so for a DualSense
    // we drive them over raw HID (see `DualSenseHID`); every other pad keeps the CoreHaptics path.
    // Runs on the serial `queue`, like the rest of the renderer state.

    /// Set a DualSense's mic-mute LED over raw HID (macOS); a no-op for any other pad.
    func setMicLED(_ mode: UInt8) {
        #if os(macOS)
        queue.async { self.dualSenseHID?.micLED(mode: mode) }
        #endif
    }

    private func openHIDIfDualSense(_ c: GCController?) -> Bool {
        #if os(macOS)
        guard let c, c.extendedGamepad is GCDualSenseGamepad else { return false }
        let hid = DualSenseHID()
        // Ask for the device this renderer's controller actually is, so two attached DualSenses
        // do not both get driven through whichever one an unordered Set happened to yield first.
        guard hid.open(preferringLocationID: Self.hidLocationID(for: c)) else { return false }
        dualSenseHID = hid
        return true
        #else
        return false
        #endif
    }

    #if os(macOS)
    /// Correlate a `GCController` with an IOKit location id.
    ///
    /// GameController exposes no location id, so there is no direct mapping. What it does expose is
    /// a stable per-controller ordering, and IOKit's location ids are stable per port: pairing the
    /// two by rank makes each renderer pick a *distinct* device, which is the property that was
    /// missing. With one pad attached this is the same device it always was.
    static func hidLocationID(for c: GCController) -> UInt32? {
        let ids = DualSenseHID.attachedLocationIDs()
        guard ids.count > 1 else { return ids.first }
        let peers = GCController.controllers().filter { $0.extendedGamepad is GCDualSenseGamepad }
        guard let rank = peers.firstIndex(where: { $0 === c }), rank < ids.count else {
            return ids.first
        }
        return ids[rank]
    }
    #endif

    /// Write the target to the DualSense over HID if that's the active backend; false → not a
    /// HID pad, so the caller renders via CoreHaptics. Deduped on the pad's 0...255 resolution,
    /// with a periodic keepalive re-write while nonzero (the ticker calls back in here).
    ///
    /// **The impulse-trigger levels are deliberately dropped here, and there is no mapping to
    /// invent.** A DualSense has *adaptive* triggers — force resistance on a trigger you press,
    /// driven by the separate 0xCD `HidOutput.Trigger` plane — and no trigger *motors*. The two
    /// features are unrelated hardware that only share a word: an Xbox Series pad has trigger
    /// motors and no adaptive triggers, a DualSense has the reverse. Routing wire trigger rumble
    /// into either the DS5 rumble bytes (which are the two handles) or the adaptive-trigger
    /// parameter block would fabricate feedback the game never asked for. This path returning
    /// `true` also means a macOS DualSense never reaches the CoreHaptics trigger localities above,
    /// which is correct for the same reason.
    private func renderHID() -> Bool {
        #if os(macOS)
        guard let hid = dualSenseHID else { return false }
        let levels = (RumbleTuning.hidByte(target.low), RumbleTuning.hidByte(target.high))
        let keepalive = levels != (0, 0)
            && seconds(since: lastHidWrite.at) > RumbleTuning.hidKeepaliveSeconds
        if levels != lastHidWrite.levels || keepalive {
            if hid.rumble(low: levels.0, high: levels.1) {
                lastHidWrite = (levels, .now())
            } else {
                // The write did not reach the device. Do NOT stamp the clock — that would claim a
                // render that never happened, and for a stop there is nothing behind it: the
                // keepalive only re-writes non-zero levels and the ticker is cancelled once the
                // target is (0, 0), so the motors would keep running with nothing scheduled.
                // Drop the handle instead: the pad reverts to CoreHaptics, and a reconnect
                // rebuilds it. Health is reported so the state is visible rather than silent.
                log.error("rumble: HID write failed — dropping the handle, falling back")
                closeHID()
                reportHealth("Lost the direct connection to this DualSense; using the system path.")
                return false
            }
        }
        return true
        #else
        return false
        #endif
    }

    private func closeHID() {
        #if os(macOS)
        dualSenseHID?.close() // writes (0,0) before releasing
        dualSenseHID = nil
        lastHidWrite = ((0, 0), DispatchTime(uptimeNanoseconds: 0))
        #endif
    }

    private func backendNote(for c: GCController?) -> String {
        #if os(macOS)
        if let hid = dualSenseHID { return "DualSense HID · \(hid.transport)" }
        #endif
        return c == nil ? "—" : "CoreHaptics"
    }
}
