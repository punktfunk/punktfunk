// Local chords and the quick-action ring for ONE captured Steam Controller 2 pad — the state
// machine `Sc2Capture` runs per source, kept pure so tests can drive it (a
// capture needs a live connection and a radio, this needs neither).
//
// A captured SC2 never enters `GamepadCapture`, so the chords that belong to the CLIENT — the
// ring's `Select+A` opener and the stats overlay's `Select+X` — have to be read here or they are
// simply unreachable with that pad in your hands. They are read off the hardware buttons, before
// any gating: the exit chord's contract is that no client-side gate can hide the way out.
//
// What the client consumes must not also reach the game. Core gates the raw report by the same
// rules (`PunktfunkConnection.setSc2Gate`); `apply` gates the parsed state the typed mirror and
// the pad-mouse fold read. A swallowed button stays swallowed until the hardware releases it:
// its press never went out, so its release must not either.
//
// Call order per state report is `read` then `apply` — `read` sees the hardware, `apply` decides
// what is left of it.

import Foundation

final class Sc2RingGate {
    /// What one report asked the client to do. The capture delivers these on the main actor.
    enum Event: Equatable {
        case ringChord
        case statsChord
        case nav(RingNav)
    }

    /// The quick-action ring's opener, `Select+A`, as in GamepadCapture. This path runs no
    /// hold-Select guide gesture — an SC2's raw feed leaves the Steam and QAM buttons to the
    /// host's own Steam, so Select here is only ever Select.
    static let ringChord: UInt32 = GamepadWire.back | GamepadWire.a
    /// The stats-overlay chord, `Select+X` — equal to `GamepadCapture.statsChord` (pinned by
    /// `Sc2RingGateTests`, the escape chord's mirror rule) and disjoint from the other two.
    static let statsChord: UInt32 = GamepadWire.back | GamepadWire.x

    /// The ring owns the pad: presses become navigation, and nothing reaches the game.
    var ringOpen = false {
        didSet {
            guard ringOpen != oldValue else { return }
            sector = nil
        }
    }

    /// What the USER is holding, in wire bits and ungated — the escape chord reads this.
    private(set) var hardware: UInt32 = 0
    /// Buttons (DEVICE layout) held out of both planes until the hardware releases them.
    private(set) var swallow: UInt32 = 0
    /// The ring sector the left stick last aimed at.
    private var sector: Int?

    /// Read one state report's ungated buttons. Every chord is edge-triggered on the press that
    /// COMPLETES it (GamepadCapture's rule): a further button pressed on top finds the mask
    /// already complete and cannot re-fire it.
    func read(_ state: Sc2Device.State) -> [Event] {
        let held = Sc2Device.wireButtons(state.buttons)
        let was = hardware
        let pressed = held & ~was
        hardware = held
        if ringOpen { return nav(state, pressed: pressed) }
        if pressed & GamepadWire.a != 0, was & GamepadWire.back != 0 {
            // SELECT FIRST, then A — GamepadCapture's ordering, which is what keeps a game's own
            // Select+A combo out of the dial. Both buttons belong to the ring now, so they are
            // swallowed before the report they arrived on is forwarded; the ring opens a main
            // hop later and takes over.
            swallow |= Sc2Device.sc2Buttons(forWire: Self.ringChord)
            return [.ringChord]
        }
        if was & Self.statsChord != Self.statsChord, held & Self.statsChord == Self.statsChord {
            // The buttons still forward: a local overlay change is not input the host must not
            // see, and GamepadCapture forwards them too.
            return [.statsChord]
        }
        return []
    }

    /// Hold the client's own input out of the parsed state. Runs after `read`.
    func apply(_ state: inout Sc2Device.State) {
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
