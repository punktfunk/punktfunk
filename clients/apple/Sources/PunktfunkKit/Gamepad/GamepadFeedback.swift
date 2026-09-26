// Host→client gamepad feedback rendering: one drain thread polls the rumble (0xCA) and
// HID-output (0xCD) planes and replays each update on the forwarded physical controller it is
// ADDRESSED TO by wire pad index —
//
//   rumble        → CHHapticEngine players (per-handle localities when the pad has them,
//                   one combined engine otherwise), a RumbleRenderer per pad,
//   lightbar      → GCDeviceLight,
//   player LEDs   → GCController.playerIndex (the DS bit patterns map to player 1–4),
//   trigger FX    → DualSenseTriggerEffect.parse → GCDualSenseAdaptiveTrigger.
//
// Every forwarded controller gets a per-pad feedback slot (its RumbleRenderer + last light /
// player-LED / trigger state) keyed on the same wire index GamepadCapture streams it on, so a
// rumble the host aimed at pad 1 drives pad 1's actuator and nothing else. An update for a pad
// with no live slot (one that just closed) is dropped. HID-output traffic exists only on
// PlayStation-pad sessions (a DualSense, or a DualShock 4 = lightbar only); the drain always
// polls both planes with short timeouts and never spins, so an Xbox pad just renders rumble.
// GameController profile mutation happens on main; CHHapticEngine work on the renderer's serial
// queue; the drain thread itself touches neither (it routes rumble to the pad's renderer under a
// lock and hops HID to main). When a controller leaves the forwarded set the old pad is reset
// (triggers off, player index unset) and its renderer silenced.
//
// One 0xCD kind never reaches main at all: `.hidRaw` — Steam's raw writes to an as-is Steam
// Controller 2 passthrough pad — routes on the drain thread straight to the registered
// `setHidRawSink` (the SC2 capture's replay entry point), because no GameController profile is
// involved and the rumble replay cadence (25–40 ms) should not queue behind main.

import Combine
import CoreHaptics
import Foundation
import GameController
import PunktfunkShared

public final class GamepadFeedback {
    private let connection: PunktfunkConnection
    private let manager: GamepadManager
    private let flag = StopFlag()
    private let drainDone = DispatchSemaphore(value: 0)
    private var drainStarted = false
    private var forwardedSub: AnyCancellable?

    /// One forwarded controller's non-rumble feedback state (main-actor) — the GC target plus the
    /// last applied lightbar / player-LED / trigger, replayed if the controller on this pad swaps.
    @MainActor private final class Slot {
        var controller: GCController?
        var lastLight: (r: UInt8, g: UInt8, b: UInt8)?
        var lastPlayerBits: UInt8?
        var lastMicMode: UInt8?
        var lastTrigger: [DualSenseTriggerEffect?] = [nil, nil]
        init(controller: GCController?) { self.controller = controller }
    }
    /// HID / lightbar / player-LED slots, keyed by wire pad index. Main-actor only.
    @MainActor private var slots: [UInt8: Slot] = [:]

    /// Rumble renderers keyed by wire pad index, guarded by `routingLock` so the background drain
    /// thread can route an incoming envelope to the right pad's renderer while the main actor
    /// reconciles the set. RumbleRenderer serializes on its own queue, so calling `apply` from the
    /// drain thread is safe — only the map lookup needs the lock.
    private let routingLock = NSLock()
    private var rumbleByPad: [UInt8: RumbleRenderer] = [:]
    /// Another window's session owns the pads: rumble drops, and lightbar, player-LED and trigger
    /// writes are only cached until `setSilenced(false)` replays them. Guarded by `routingLock`.
    private var silenced = false

    /// The raw-report sink for `.hidRaw` events — the Steam Controller 2 capture's
    /// `Sc2Capture.onHidRaw(pad:kind:data:)`, registered by the session owner. Guarded by
    /// `routingLock` (set on the main actor, read on the drain thread). Routed WITHOUT the
    /// main-actor hop the led/trigger path takes: Steam replays rumble at a 25–40 ms cadence,
    /// the sink filters by pad itself, and its GATT write hops to the BLE queue anyway. nil
    /// (no capture registered) drops the event — the shape every unroutable feedback takes.
    private var hidRawSink: ((_ pad: UInt8, _ kind: UInt8, _ data: [UInt8]) -> Void)?

    /// Opt-in device mirror (`DefaultsKey.rumbleOnDevice`, iPhone only): rumble the host
    /// addresses to controller 1 (wire pad 0) is ALSO rendered on this device's own Taptic
    /// Engine — for phone-clip pads that ship without rumble motors, where the phone body is the
    /// only actuator in the player's hands. Session-scoped (the setting is read once here); nil
    /// when off or where the device has no haptic actuator.
    private let deviceRumble: RumbleRenderer?

    public init(connection: PunktfunkConnection, manager: GamepadManager) {
        self.connection = connection
        self.manager = manager
        #if os(iOS)
        if UserDefaults.standard.bool(forKey: DefaultsKey.rumbleOnDevice),
            CHHapticEngine.capabilitiesForHardware().supportsHaptics {
            deviceRumble = RumbleRenderer(actuator: .device)
        } else {
            deviceRumble = nil
        }
        #else
        deviceRumble = nil
        #endif
        // Capture self weakly in the hop too, so the inner sink's weak capture isn't shadowing
        // an implicit strong one — and the subscription (stored on self) never retain-cycles.
        Task { @MainActor [weak self] in
            guard let self else { return }
            self.forwardedSub = manager.$forwarded.sink { [weak self] list in
                MainActor.assumeIsolated { self?.reconcile(list) }
            }
        }
    }

    /// Safety net: the drain thread captures `connection` strongly and only `self` weakly, so if
    /// this is dropped without `stop()` (an abrupt teardown) the thread would poll forever and
    /// leak the connection — signal it to exit. (`stop()` is the normal path and also joins it.)
    deinit { flag.stop() }

    /// Map the DualSense player-LED bit patterns (5 LEDs, hid-playstation's player
    /// conventions) onto GCControllerPlayerIndex. Unknown patterns fall back to the lit
    /// count, clamped to the four indices GC offers.
    public static func playerIndex(forBits bits: UInt8) -> GCControllerPlayerIndex {
        switch bits & 0x1F {
        case 0: return .indexUnset
        case 0b00100: return .index1
        case 0b01010: return .index2
        case 0b10101: return .index3
        case 0b11011: return .index4
        default:
            let lit = (bits & 0x1F).nonzeroBitCount
            return GCControllerPlayerIndex(rawValue: min(lit, 4) - 1) ?? .index1
        }
    }

    /// Bring the per-pad feedback slots in line with the forwarded set: drop pads no longer
    /// forwarded (silence + release their renderer, reset their controller), add a slot +
    /// renderer for each new pad, and retarget a pad whose controller changed (a re-plug into the
    /// same freed index) — replaying its cached feedback onto the new device.
    @MainActor
    private func reconcile(_ forwarded: [GamepadManager.DiscoveredController]) {
        var want: [UInt8: GCController] = [:]
        for dc in forwarded {
            if let pad = manager.padIndex(for: dc) { want[pad] = dc.controller }
        }
        for (pad, slot) in slots where want[pad] == nil {
            reset(slot.controller)
            slots[pad] = nil
            let renderer = withRouting { rumbleByPad.removeValue(forKey: pad) }
            // OFF the main actor. `RumbleRenderer.stop()` is a `queue.sync`, and its body is a
            // per-motor `CHHapticEngine.stop()` — an XPC round trip to gamecontrollerd, which the
            // renderer's own notes record as able to hang — plus `DualSenseHID.close()`, whose
            // blocking `IOHIDDeviceSetReport` goes to a device that has just departed. It also
            // queues behind any in-flight `setup()`. This runs on every unplug and every pin
            // change, and the main thread is what drives the presenter's CADisplayLink, so
            // blocking here hitches the picture mid-stream. The renderer is already detached from
            // routing above, so nothing observes it after this point.
            if let renderer { Task.detached { renderer.stop() } }
        }
        for (pad, controller) in want {
            if let slot = slots[pad] {
                guard slot.controller !== controller else { continue }
                reset(slot.controller)
                slot.controller = controller
                withRouting { rumbleByPad[pad]?.retarget(controller) }
                replay(pad, slot)
            } else {
                slots[pad] = Slot(controller: controller)
                let renderer = RumbleRenderer()
                renderer.retarget(controller)
                withRouting { rumbleByPad[pad] = renderer }
            }
        }
    }

    public func start() {
        guard !drainStarted else { return }
        drainStarted = true
        // Hidout traffic (lightbar / player LEDs / triggers) only exists on a PlayStation-pad
        // session — a DualSense or a DualShock 4 (lightbar only). Block briefly on it there and
        // let rumble own the wait elsewhere; on an Xbox session it stays nonblocking.
        let thread = Thread { [connection, flag, drainDone, weak self] in
            // Per-iteration autorelease pool: no runloop on this thread, and the haptics/HID
            // rendering below autoreleases ObjC temporaries. `false` = session over.
            var alive = true
            while alive, !flag.isStopped {
                alive = autoreleasepool { () -> Bool in
                do {
                    // Poll the feedback planes NON-BLOCKING. A blocking poll (timeoutMs > 0) holds
                    // the connection's shared feedback lock for its whole wait; the video pump drains
                    // HDR mastering metadata (nextHdrMeta) on the SAME lock every frame, so a blocking
                    // poll here starved it and throttled HDR to ~1 fps (SDR, which never drains HDR
                    // meta, was unaffected). Pacing with a short sleep OUTSIDE the lock (below) keeps
                    // rumble/HID latency low while leaving the lock free between polls.
                    //
                    // Rumble arrives as EFFECTIVE commands from the core's shared policy engine
                    // (design/rumble-root-fix.md §D): the engine owns leases, legacy staleness,
                    // and close-drain zeros, and its per-pad mailbox already coalesces — a
                    // stalled drain wakes to ONE current-level command per pad, and a stop can
                    // never be shed by a queue. Apply verbatim, in order.
                    var rumbleBurst = 0
                    while rumbleBurst < 64, !flag.isStopped,
                          let c = try connection.nextRumbleCommand(timeoutMs: 0) {
                        self?.routeRumble(
                            pad: UInt8(truncatingIfNeeded: c.pad), low: c.low, high: c.high,
                            leftTrigger: c.leftTrigger, rightTrigger: c.rightTrigger)
                        rumbleBurst += 1
                    }
                    // Drain a BOUNDED burst of hidout events so sustained 0xCD traffic (a game writing
                    // per-frame LED/trigger reports) can't spin here or block stop() past one cycle.
                    var burst = 0
                    while burst < 64, !flag.isStopped,
                          let ev = try connection.nextHidOutput(timeoutMs: 0) {
                        self?.render(ev)
                        burst += 1
                    }
                    return true
                } catch {
                    return false // .closed (or fatal) — the session is over
                }
                }
                // ~8 ms poll cadence (≈125 Hz), slept OUTSIDE the feedback lock — low rumble/HID
                // latency without holding the lock the HDR-meta drain needs.
                if alive, !flag.isStopped { Thread.sleep(forTimeInterval: 0.008) }
            }
            drainDone.signal()
        }
        thread.name = "punktfunk-feedback"
        thread.qualityOfService = .userInteractive
        thread.start()
    }

    /// Stop the drain and silence every pad's motors. Blocks until the drain thread exits (≤ one
    /// poll cycle) — call off the main actor, before `connection.close()`.
    public func stop() {
        flag.stop()
        if drainStarted {
            drainDone.wait()
            drainStarted = false
        }
        let renderers = withRouting { () -> [RumbleRenderer] in
            let r = Array(rumbleByPad.values)
            rumbleByPad.removeAll()
            hidRawSink = nil // no raw replay onto a capture the session owner is tearing down
            return r
        }
        for r in renderers { r.stop() }
        deviceRumble?.stop()
        // Drop the subscription and every dead pad's cached feedback — a controller change after
        // teardown must not replay this session's triggers/LEDs.
        Task { @MainActor in
            self.forwardedSub = nil
            for slot in self.slots.values { self.reset(slot.controller) }
            self.slots.removeAll()
        }
    }

    /// Route one engine command to its pad's renderer (drain thread). A command for a pad with no
    /// live renderer — one that just left the forwarded set — is dropped.
    private func routeRumble(
        pad: UInt8, low: UInt16, high: UInt16, leftTrigger: UInt16, rightTrigger: UInt16
    ) {
        guard let renderer = withRouting({ silenced ? nil : rumbleByPad[pad] }) else { return }
        renderer.apply(low: low, high: high, leftTrigger: leftTrigger, rightTrigger: rightTrigger)
        // The opt-in device mirror follows controller 1 unconditionally — the pads it exists for
        // have no motors (their renderer above no-ops), and mirroring deliberately isn't gated on
        // that: capability probing can't see a motor-less MFi pad, and the user opted in.
        //
        // HANDLES ONLY, deliberately. A phone body is one actuator with no trigger analogue, so
        // the trigger levels would have to be folded to arrive at all — and folding continuous
        // impulse-trigger content (a racing title's engine RPM / tyre slip) onto the one motor
        // this mirror has would buzz the phone flat-out for the whole race at a level the game
        // never requested. Dropping them matches the core engine's policy for every pad without
        // trigger motors.
        if pad == 0 { deviceRumble?.apply(low: low, high: high) }
    }

    /// Quiet this session's feedback while another capture owns the pads, and put its lightbar,
    /// player LEDs and triggers back when it takes them again.
    @MainActor
    public func setSilenced(_ on: Bool) {
        let playing = withRouting { () -> [RumbleRenderer] in
            silenced = on
            return on ? Array(rumbleByPad.values) : []
        }
        for renderer in playing { renderer.apply(low: 0, high: 0, leftTrigger: 0, rightTrigger: 0) }
        if !on { for (pad, slot) in slots { replay(pad, slot) } }
    }

    private func withRouting<R>(_ body: () -> R) -> R {
        routingLock.lock()
        defer { routingLock.unlock() }
        return body()
    }

    /// Register (or clear) the `.hidRaw` sink — the session owner wires `Sc2Capture.onHidRaw`
    /// here beside starting the capture, and clears it BEFORE stopping either side (the
    /// Android teardown order: unhook → feedback.stop → capture.stop).
    public func setHidRawSink(
        _ sink: ((_ pad: UInt8, _ kind: UInt8, _ data: [UInt8]) -> Void)?
    ) {
        withRouting { hidRawSink = sink }
    }

    private func render(_ ev: PunktfunkConnection.HidOutputEvent) {
        // Raw SC2 reports stay on the drain thread — no GameController profile is touched, and
        // the main-actor hop would only add latency to Steam's 25–40 ms rumble resends.
        if case let .hidRaw(pad, kind, data) = ev {
            let sink = withRouting { hidRawSink }
            sink?(pad, kind, data)
            return
        }
        DispatchQueue.main.async {
            MainActor.assumeIsolated { self.apply(ev) }
        }
    }

    @MainActor
    private func apply(_ ev: PunktfunkConnection.HidOutputEvent) {
        switch ev {
        case let .led(pad, r, g, b):
            guard let slot = slots[pad] else { return }
            slot.lastLight = (r, g, b)
            guard !withRouting({ silenced }) else { return }
            slot.controller?.light?.color = GCColor(
                red: Float(r) / 255, green: Float(g) / 255, blue: Float(b) / 255)
        case let .playerLEDs(pad, bits):
            guard let slot = slots[pad] else { return }
            slot.lastPlayerBits = bits
            guard !withRouting({ silenced }) else { return }
            slot.controller?.playerIndex = Self.playerIndex(forBits: bits)
        case let .micLED(pad, mode):
            // No GameController API: only the macOS raw-HID path lights it.
            guard let slot = slots[pad] else { return }
            slot.lastMicMode = mode
            guard !withRouting({ silenced }) else { return }
            withRouting { rumbleByPad[pad] }?.setMicLED(mode)
        case let .triggerEffect(pad, which, effect):
            guard which < 2, let slot = slots[pad] else { return }
            let parsed = DualSenseTriggerEffect.parse(effect)
            slot.lastTrigger[Int(which)] = parsed
            if !withRouting({ silenced }), let trigger = adaptiveTrigger(slot.controller, which) {
                parsed.apply(to: trigger)
            }
        case .hidRaw:
            // Routed on the drain thread (`render`) straight to the registered sink — a raw
            // report never touches a GameController profile, so it never reaches this actor.
            break
        }
    }

    /// Replay a pad's cached feedback onto its (swapped-in) controller so a re-plug looks the same.
    @MainActor
    private func replay(_ pad: UInt8, _ slot: Slot) {
        if let (r, g, b) = slot.lastLight {
            slot.controller?.light?.color = GCColor(
                red: Float(r) / 255, green: Float(g) / 255, blue: Float(b) / 255)
        }
        if let bits = slot.lastPlayerBits {
            slot.controller?.playerIndex = Self.playerIndex(forBits: bits)
        }
        if let mode = slot.lastMicMode {
            withRouting { rumbleByPad[pad] }?.setMicLED(mode)
        }
        for which in 0..<2 {
            if let effect = slot.lastTrigger[which],
               let trigger = adaptiveTrigger(slot.controller, UInt8(which)) {
                effect.apply(to: trigger)
            }
        }
    }

    @MainActor
    private func reset(_ controller: GCController?) {
        guard let c = controller else { return }
        c.playerIndex = .indexUnset
        // Put the lightbar out too. This class is what turned it on (see the `Led` and
        // `PlayerLeds` arms), and every DS write is valid-flag-selective, so a colour the game
        // set stays lit in firmware after the stream ends — back at the launcher, or for a pad
        // that merely left the forwarded set. A DS4 is cleared incidentally because its player
        // indicator IS the lightbar; a DualSense is not.
        c.light?.color = GCColor(red: 0, green: 0, blue: 0)
        if let ds = c.extendedGamepad as? GCDualSenseGamepad {
            ds.leftTrigger.setModeOff()
            ds.rightTrigger.setModeOff()
        }
    }

    @MainActor
    private func adaptiveTrigger(_ controller: GCController?, _ which: UInt8) -> GCDualSenseAdaptiveTrigger? {
        guard let ds = controller?.extendedGamepad as? GCDualSenseGamepad else { return nil }
        return which == 0 ? ds.leftTrigger : ds.rightTrigger
    }
}
