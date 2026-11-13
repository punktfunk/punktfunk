// Shortcuts and the quick-action ring for ONE captured Steam Controller 2 pad — the state machine
// `Sc2Capture` runs per source, kept pure so tests can drive it (a capture needs a live
// connection and a radio, this needs neither).
//
// A captured SC2 never enters `GamepadCapture`, so its shortcuts run here, through the same
// `Shortcuts.Engine`, off the hardware buttons and before any gating: the exit hold's contract
// is that no client-side gate can hide the way out.
//
// The typed plane carries what the engine forwards. Core gates the raw plane as
// `Sc2Capture.pushGateLocked` sets it (`PunktfunkConnection.setSc2Gate`): the open ring, and
// Select then A when that opens it; no other chord. While the ring is up a button held now
// stays off the typed plane until the hardware releases it.
//
// Call order per state report is `read` then `apply` — `read` sees the hardware, `apply` decides
// what is left of it. Every report ticks the engine, so no timer runs here.

import Foundation

final class Sc2RingGate {
    /// What one report asked the client to do. The capture delivers these on the main actor.
    enum Event: Equatable {
        case fire(Shortcuts.Action, Shortcuts.Phase)
        case nav(RingNav)
    }

    /// The ring owns the pad: presses become navigation, and nothing reaches the game.
    var ringOpen = false {
        didSet {
            guard ringOpen != oldValue else { return }
            sector = nil
        }
    }

    /// What the USER is holding, in wire bits and ungated.
    private(set) var hardware: UInt32 = 0
    /// What the engine has sent on, in wire bits: the typed plane's buttons.
    private(set) var forwarded: UInt32 = 0
    /// Buttons (DEVICE layout) held out of the typed plane until the hardware releases them.
    private(set) var swallow: UInt32 = 0
    private var engine: Shortcuts.Engine
    /// The ring sector the left stick last aimed at.
    private var sector: Int?

    init(_ bindings: [Shortcuts.Binding] = []) {
        engine = Shortcuts.Engine(bindings, device: .pad)
    }

    /// Read one state report's ungated buttons through the engine.
    func read(_ state: Sc2Device.State, at now: ContinuousClock.Instant = .now) -> [Event] {
        let held = Sc2Device.wireButtons(state.buttons)
        let changed = held ^ hardware
        hardware = held
        var outs = engine.tick(now)
        let bits = (0..<32).map { UInt32(1) << $0 }.filter { changed & $0 != 0 }
        for bit in bits where held & bit == 0 { outs += engine.step(.release(bit), at: now) }
        for bit in bits where held & bit != 0 { outs += engine.step(.press(bit), at: now) }
        var events: [Event] = []
        let was = forwarded
        for out in outs {
            switch out {
            case .forward(let bit, let down): forwarded = down ? forwarded | bit : forwarded & ~bit
            case .lift(let bits): forwarded &= ~bits.reduce(0, |)
            case .fire(let action, let phase): events.append(.fire(action, phase))
            }
        }
        if ringOpen { events += nav(state, pressed: forwarded & ~was) }
        return events
    }

    /// Hand the typed plane what the engine forwards. Runs after `read`.
    func apply(_ state: inout Sc2Device.State) {
        let held = Sc2Device.wireButtons(state.buttons)
        state.buttons = (state.buttons & ~Sc2Device.sc2Buttons(forWire: held))
            | Sc2Device.sc2Buttons(forWire: forwarded)
        swallow &= state.buttons
        if ringOpen { swallow |= state.buttons }
        guard swallow != 0 || ringOpen else { return }
        state.buttons &= ~swallow
        guard ringOpen else { return }
        state.lsX = 0
        state.lsY = 0
        state.rsX = 0
        state.rsY = 0
        state.lt = 0
        state.rt = 0
    }

    /// One report's worth of ring navigation: the D-pad and face buttons step the highlight on
    /// their press edges, the left stick aims it by sector.
    private func nav(_ state: Sc2Device.State, pressed: UInt32) -> [Event] {
        let map: [(UInt32, RingNav)] = [
            (GamepadWire.dpadUp, .up), (GamepadWire.dpadDown, .down),
            (GamepadWire.dpadLeft, .left), (GamepadWire.dpadRight, .right),
            (GamepadWire.a, .confirm), (GamepadWire.b, .back), (GamepadWire.y, .centre),
        ]
        var events = map.filter { pressed & $0.0 != 0 }.map { Event.nav($0.1) }
        // The stick AIMS, the D-pad steps: the sector is the slot, so the dial follows the thumb
        // the way a weapon wheel does. Emitted on every change, neutral included.
        let now = GamepadCapture.ringSector(
            Float(state.lsX) / 32767, Float(state.lsY) / 32767, sector)
        if now != sector {
            sector = now
            events.append(.nav(.sector(now)))
        }
        return events
    }
}
