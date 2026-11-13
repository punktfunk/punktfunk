// DualSense adaptive-trigger effect parsing: the host forwards the raw 11-byte trigger
// parameter block a game wrote to its virtual DualSense (output report 0x02, bytes 11–21
// for R2 / 22–32 for L2: one mode byte + 10 parameter bytes). The mode values and layouts
// follow the community-established conventions (Nielk1's TriggerEffectGenerator, ds5w,
// inputtino) — Sony has never documented them. Parsing is TOTAL: any unknown or short
// block degrades to `.off`, never traps, so a game using an exotic raw mode can't break
// the session.
//
// `parse` is a pure function (unit-tested without a controller); `apply(to:)` maps the
// result onto Apple's GCDualSenseAdaptiveTrigger — exact for the 10-zone positional modes
// (0x21/0x26 → the positional resistiveStrengths/amplitudes APIs), best-effort for the
// composite ones (bow, galloping) that have no GC equivalent.

import Foundation
import GameController

/// A parsed DualSense trigger effect. Positions and strengths are normalized 0...1
/// (GCDualSenseAdaptiveTrigger's domain); `positional*` carry one value per trigger zone
/// (10 zones across the travel).
public enum DualSenseTriggerEffect: Equatable, Sendable {
    case off
    /// Constant resistance from `start` to the end of travel.
    case feedback(start: Float, strength: Float)
    /// Resistance from `start` that releases past `end` (trigger-break / weapon feel).
    case weapon(start: Float, end: Float, strength: Float)
    /// Vibration once the trigger passes `start`.
    case vibration(start: Float, amplitude: Float, frequency: Float)
    /// Per-zone resistance (10 zones).
    case positionalFeedback(strengths: [Float])
    /// Per-zone vibration amplitudes (10 zones) at `frequency`.
    case positionalVibration(amplitudes: [Float], frequency: Float)
    /// Resistance ramping `startStrength` → `endStrength` between `start` and `end`
    /// (the closest GC rendering of the bow effect).
    case slope(start: Float, end: Float, startStrength: Float, endStrength: Float)

    /// Parse a raw trigger parameter block (`[mode, p0...p9]`, ≤ 11 bytes — shorter blocks
    /// are zero-padded). Never fails: unknown modes are `.off`.
    public static func parse(_ block: [UInt8]) -> DualSenseTriggerEffect {
        guard let mode = block.first else { return .off }
        var p = [UInt8](block.dropFirst())
        if p.count < 10 { p.append(contentsOf: [UInt8](repeating: 0, count: 10 - p.count)) }

        // Helpers for the rich (0x2x) modes: a 10-bit active-zone mask in p0/p1 and 3-bit
        // per-zone values packed little-endian into the following 4 bytes.
        let zoneMask = UInt16(p[0]) | (UInt16(p[1]) << 8)
        let packed = UInt32(p[2]) | (UInt32(p[3]) << 8) | (UInt32(p[4]) << 16) | (UInt32(p[5]) << 24)
        func zoneValues() -> [UInt8] {
            (0..<10).map { i in
                zoneMask & (1 << i) != 0 ? UInt8((packed >> (3 * UInt32(i))) & 0x07) : 0
            }
        }
        // DualSense 3-bit strengths are 0...7 where an *active* zone's value v renders as
        // (v+1)/8 — a present-but-zero strength still resists slightly.
        func strength01(_ v: UInt8, active: Bool) -> Float {
            active ? Float(v + 1) / 8 : 0
        }
        func zone01(_ z: Int) -> Float { Float(z) / 9 }
        func firstActive() -> Int? { (0..<10).first { zoneMask & (1 << $0) != 0 } }
        func lastActive() -> Int? { (0..<10).last { zoneMask & (1 << $0) != 0 } }

        switch mode {
        case 0x00, 0x05, 0xF0...0xFF:
            // 0x00/0x05 are the documented off/reset modes; 0xFC/0xFB/0xFA show up from
            // calibration-adjacent writes — all render as off.
            return .off

        case 0x01:
            // Legacy continuous resistance: p0 = start position, p1 = force (both 0...255).
            return .feedback(start: Float(p[0]) / 255, strength: max(Float(p[1]) / 255, 1.0 / 8))

        case 0x02:
            // Legacy section: p0 = start, p1 = end (0...255); full-strength inside.
            let s = Float(p[0]) / 255
            let e = Float(p[1]) / 255
            return .weapon(start: min(s, e), end: max(max(s, e), min(s, e) + 0.01), strength: 1)

        case 0x06:
            // Legacy vibration ("automatic gun") — Nielk1's Simple_Vibration order:
            // p0 = frequency Hz, p1 = amplitude, p2 = start position.
            return .vibration(
                start: Float(p[2]) / 255,
                amplitude: max(Float(p[1]) / 255, 1.0 / 8),
                frequency: Float(p[0]) / 255)

        case 0x21:
            // Feedback: 10-bit zone mask + 3-bit strength per zone. A uniform suffix maps
            // exactly onto the simple feedback call; mixed strengths use the positional API.
            guard let first = firstActive() else { return .off }
            let values = zoneValues()
            let active = values.enumerated().filter { zoneMask & (1 << $0.offset) != 0 }
            if active.allSatisfy({ $0.element == active[0].element })
                && active.last?.offset == 9
                && active.map(\.offset) == Array(first...9)
            {
                return .feedback(
                    start: zone01(first), strength: strength01(active[0].element, active: true))
            }
            return .positionalFeedback(
                strengths: values.enumerated().map {
                    strength01($0.element, active: zoneMask & (1 << $0.offset) != 0)
                })

        case 0x22:
            // Bow (Nielk1 mode byte 0x22): start/end zones + draw strength and snap force
            // packed as a 3-bit pair in p2 (low bits draw, bits 3-5 snap; p3 is always 0).
            // No GC equivalent — render as a slope from draw resistance down to the snap.
            guard let s = firstActive(), let e = lastActive(), s < e else { return .off }
            let draw = strength01(p[2] & 0x07, active: true)
            let snap = strength01((p[2] >> 3) & 0x07, active: true)
            return .slope(start: zone01(s), end: zone01(e), startStrength: draw, endStrength: snap)

        case 0x25:
            // Weapon (Nielk1 mode byte 0x25): zone mask marks the start and end zones,
            // p2 = strength (3-bit, stored minus one).
            guard let s = firstActive(), let e = lastActive(), s < e else { return .off }
            return .weapon(start: zone01(s), end: zone01(e), strength: strength01(p[2] & 0x07, active: true))

        case 0x26:
            // Vibration: 10-bit zone mask + 3-bit amplitude per zone, p8 = frequency Hz.
            guard zoneMask != 0 else { return .off }
            let amplitudes = zoneValues().enumerated().map {
                strength01($0.element, active: zoneMask & (1 << $0.offset) != 0)
            }
            return .positionalVibration(amplitudes: amplitudes, frequency: Float(p[8]) / 255)

        case 0x23:
            // Galloping (Nielk1 mode byte 0x23): start/end zones, p2 = packed foot timing,
            // p3 = frequency Hz. The temporal hoofbeat pattern has no GC equivalent —
            // render as vibration across the active range.
            guard let s = firstActive(), let e = lastActive() else { return .off }
            var amplitudes = [Float](repeating: 0, count: 10)
            for z in s...e { amplitudes[z] = 0.5 }
            return .positionalVibration(amplitudes: amplitudes, frequency: Float(p[3]) / 255)

        case 0x27:
            // Machine (Nielk1 mode byte 0x27): start/end zones, p2 = two 3-bit amplitudes
            // (low bits A, bits 3-5 B — raw 0...7, no minus-one), p3 = frequency Hz. The
            // A/B alternation is temporal — render its stronger leg across the range.
            guard let s = firstActive(), let e = lastActive() else { return .off }
            let amp = Float(max(p[2] & 0x07, (p[2] >> 3) & 0x07)) / 7
            guard amp > 0 else { return .off }
            var amplitudes = [Float](repeating: 0, count: 10)
            for z in s...e { amplitudes[z] = amp }
            return .positionalVibration(amplitudes: amplitudes, frequency: Float(p[3]) / 255)

        default:
            return .off
        }
    }
}

extension DualSenseTriggerEffect {
    /// Replay this effect on a physical DualSense trigger. Main-thread only (GameController
    /// profile mutation). The GC `frequency` parameter is normalized 0...1 like ours.
    @MainActor
    public func apply(to trigger: GCDualSenseAdaptiveTrigger) {
        switch self {
        case .off:
            trigger.setModeOff()
        case let .feedback(start, strength):
            trigger.setModeFeedbackWithStartPosition(start, resistiveStrength: strength)
        case let .weapon(start, end, strength):
            trigger.setModeWeaponWithStartPosition(start, endPosition: end, resistiveStrength: strength)
        case let .vibration(start, amplitude, frequency):
            trigger.setModeVibrationWithStartPosition(start, amplitude: amplitude, frequency: frequency)
        case let .positionalFeedback(strengths):
            var s = GCDualSenseAdaptiveTrigger.PositionalResistiveStrengths(
                values: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0))
            withUnsafeMutableBytes(of: &s.values) { raw in
                let f = raw.bindMemory(to: Float.self)
                for (i, v) in strengths.prefix(10).enumerated() { f[i] = v }
            }
            trigger.setModeFeedback(resistiveStrengths: s)
        case let .positionalVibration(amplitudes, frequency):
            var a = GCDualSenseAdaptiveTrigger.PositionalAmplitudes(
                values: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0))
            withUnsafeMutableBytes(of: &a.values) { raw in
                let f = raw.bindMemory(to: Float.self)
                for (i, v) in amplitudes.prefix(10).enumerated() { f[i] = v }
            }
            trigger.setModeVibration(amplitudes: a, frequency: frequency)
        case let .slope(start, end, startStrength, endStrength):
            trigger.setModeSlopeFeedback(
                startPosition: start, endPosition: end,
                startStrength: startStrength, endStrength: endStrength)
        }
    }
}
