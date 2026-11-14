// The virtual on-screen controller's model (design/touch-client-overlay.md §4): the preset
// geometry, the three pure input rules, and the wire slot it drives. The Swift twin of the
// Android `VirtualPad.kt` — same numbers, same rules, so the two pads feel the same. The layer
// that draws it and owns the fingers is UIKit in the client (`VirtualPadLayer.swift`).

import Foundation
import PunktfunkShared

/// A control's place in the layer, in points before the scale; the origin is the top-left.
public struct PadRect: Equatable, Sendable {
    public var x: Float
    public var y: Float
    public var w: Float
    public var h: Float

    public init(_ x: Float, _ y: Float, _ w: Float, _ h: Float) {
        self.x = x; self.y = y; self.w = w; self.h = h
    }

    public func overlaps(_ o: PadRect) -> Bool {
        x < o.x + o.w && o.x < x + w && y < o.y + o.h && o.y < y + h
    }
}

/// One disc in a `.buttons` group: its centre and radius relative to the group's rect.
public struct PadDisc: Equatable, Sendable {
    public let label: String
    public let glyph: String
    public let bit: UInt32
    public let cx: Float
    public let cy: Float
    public let r: Float
}

public enum PadControlKind: Equatable, Sendable {
    /// Discs that press while a finger is on them; a finger rolling from one to the next presses the next.
    case buttons([PadDisc])
    /// Eight directions by angle from the centre, as the D-pad bits.
    case dpad
    /// The first finger owns it; its travel from where it landed is the deflection.
    case stick(axisX: UInt32, axisY: UInt32)
    /// The finger's position down the pill is the pull: the top is 0, the bottom is full.
    case trigger(axis: UInt32)
    /// A tap opens the quick-action ring; nothing goes on the wire.
    case ring
}

/// `id` is the tweak key in the blob (`PadTweak`); `sc` and `hidden` are the applied tweak —
/// the geometry in `rect` and any discs is already sized by `sc`, and the touch and draw code
/// multiplies only its named constants (stick radius, dead zones, label sizes) by it.
public struct PadControl: Equatable, Sendable {
    public let id: String
    public let label: String
    public let rect: PadRect
    public let kind: PadControlKind
    public var sc: Float = 1
    public var hidden = false

    /// The same control at `rect`, sized by `sc`, possibly `hidden` — how a tweak is applied.
    func tweaked(rect: PadRect, sc: Float, hidden: Bool) -> PadControl {
        let kind: PadControlKind
        switch self.kind {
        case .buttons(let discs):
            kind = .buttons(discs.map {
                PadDisc(label: $0.label, glyph: $0.glyph, bit: $0.bit, cx: $0.cx * sc, cy: $0.cy * sc, r: $0.r * sc)
            })
        default:
            kind = self.kind
        }
        return PadControl(id: id, label: label, rect: rect, kind: kind, sc: sc, hidden: hidden)
    }
}

/// The pad's fixed numbers, in points before the scale.
public enum VirtualPad {
    public static let scaleRange: ClosedRange<Float> = 0.6...1.6
    public static let opacityRange: ClosedRange<Float> = 0.15...1
    /// A per-control tweak's scale bounds; what a blob claims is clamped here.
    public static let tweakScaleRange: ClosedRange<Float> = 0.5...2
    public static let stickRadius: Float = 60
    public static let stickKnobRadius: Float = 26
    /// A disc takes a finger this far past its edge, so a thumb need not be exact.
    public static let hitSlop: Float = 1.3
    // ponytail: fixed dead zones; a setting when a device disagrees with these.
    public static let stickDead: Float = 6
    public static let dpadDead: Float = 14

    static let margin: Float = 16
    /// The node is wider than the base: a thumb landing just outside the ring still takes the stick.
    static let stickHit: Float = 2 * (stickRadius + 20)
    static let faceRadius: Float = 24
    static let faceHit: Float = 152
    static let dpadHit: Float = 120
    static let smallRadius: Float = 18
    static let bumperRadius: Float = 22
    static let triggerW: Float = 56
    static let triggerH: Float = 84
    /// A layer narrower than this (a phone upright) stacks the clusters instead of spreading
    /// them — and keeps its own set of layout overrides (`PadConfig.controlsNarrow`).
    public static let narrow: Float = 720
}

private func disc(_ id: String, _ label: String, _ glyph: String, _ bit: UInt32, _ cx: Float, _ cy: Float, _ r: Float) -> PadControl {
    PadControl(id: id, label: label, rect: PadRect(cx - r, cy - r, 2 * r, 2 * r),
               kind: .buttons([PadDisc(label: label, glyph: glyph, bit: bit, cx: r, cy: r, r: r)]))
}

/// The preset's controls for a layer `w` × `h` points (the container divided by the scale).
/// Positions are fixed per preset (§4.3): sticks in the bottom corners, the D-pad beside the left
/// stick, the face buttons in the bottom-right corner with the right stick beside them, the
/// shoulders in the top corners with the stick clicks beside them, Select, Guide and Start along
/// the bottom edge, and the quick-action ring's button inboard of the left bumper. A narrow layer
/// lifts the D-pad and the right stick above their neighbours; the middle three keep the bottom
/// edge only while the clusters leave it free, and take the top edge when they do not. An unknown
/// preset is `full`.
public func padControls(layout: String, w: Float, h: Float) -> [PadControl] {
    typealias P = VirtualPad
    let narrow = w < P.narrow
    let sticks = layout != "dpad"
    let face = layout != "sticks"
    let bottom = h - P.margin
    var out: [PadControl] = []
    if sticks {
        out.append(disc("lb", String(localized: L10n.padControlLeftBumper), "LB", GamepadWire.leftShoulder, P.margin + P.bumperRadius, P.margin + P.bumperRadius, P.bumperRadius))
        out.append(PadControl(id: "lt", label: String(localized: L10n.padControlLeftTrigger),
                              rect: PadRect(P.margin, P.margin + 2 * P.bumperRadius + 8, P.triggerW, P.triggerH),
                              kind: .trigger(axis: GamepadWire.axisLT)))
        out.append(disc("rb", String(localized: L10n.padControlRightBumper), "RB", GamepadWire.rightShoulder, w - P.margin - P.bumperRadius, P.margin + P.bumperRadius, P.bumperRadius))
        out.append(PadControl(id: "rt", label: String(localized: L10n.padControlRightTrigger),
                              rect: PadRect(w - P.margin - P.triggerW, P.margin + 2 * P.bumperRadius + 8, P.triggerW, P.triggerH),
                              kind: .trigger(axis: GamepadWire.axisRT)))
        // The stick clicks sit inboard of the triggers, level with them: the index finger already
        // rests there, and the thumb keeps the stick — which is what L3-to-sprint asks for.
        let clickY = P.margin + 2 * P.bumperRadius + 8 + P.triggerH / 2
        let clickX = P.margin + P.triggerW + 8 + P.smallRadius
        out.append(disc("l3", String(localized: L10n.padControlLeftStickClick), "L3", GamepadWire.leftStickClick, clickX, clickY, P.smallRadius))
        out.append(disc("r3", String(localized: L10n.padControlRightStickClick), "R3", GamepadWire.rightStickClick, w - clickX, clickY, P.smallRadius))
        out.append(PadControl(id: "ls", label: String(localized: L10n.padControlLeftStick),
                              rect: PadRect(P.margin, bottom - P.stickHit, P.stickHit, P.stickHit),
                              kind: .stick(axisX: GamepadWire.axisLSX, axisY: GamepadWire.axisLSY)))
    }
    if face {
        let dpad: PadRect
        if !sticks {
            dpad = PadRect(P.margin + 20, bottom - 20 - P.dpadHit, P.dpadHit, P.dpadHit)
        } else if narrow {
            dpad = PadRect(P.margin + 20, bottom - P.stickHit - 8 - P.dpadHit, P.dpadHit, P.dpadHit)
        } else {
            dpad = PadRect(P.margin + P.stickHit + 10, bottom - 110 - P.dpadHit, P.dpadHit, P.dpadHit)
        }
        out.append(PadControl(id: "dpad", label: String(localized: L10n.padControlDpad), rect: dpad, kind: .dpad))
        let c = P.faceHit / 2
        let gap: Float = 40
        out.append(PadControl(
            id: "face",
            label: String(localized: L10n.padControlFace),
            rect: PadRect(w - P.margin - P.faceHit, bottom - P.faceHit, P.faceHit, P.faceHit),
            kind: .buttons([
                PadDisc(label: "Y", glyph: "Y", bit: GamepadWire.y, cx: c, cy: c - gap, r: P.faceRadius),
                PadDisc(label: "X", glyph: "X", bit: GamepadWire.x, cx: c - gap, cy: c, r: P.faceRadius),
                PadDisc(label: "B", glyph: "B", bit: GamepadWire.b, cx: c + gap, cy: c, r: P.faceRadius),
                PadDisc(label: "A", glyph: "A", bit: GamepadWire.a, cx: c, cy: c + gap, r: P.faceRadius),
            ])))
    }
    if sticks {
        let right: PadRect
        if !face {
            right = PadRect(w - P.margin - P.stickHit, bottom - P.stickHit, P.stickHit, P.stickHit)
        } else if narrow {
            right = PadRect(w - P.margin - P.stickHit, bottom - P.faceHit - 8 - P.stickHit, P.stickHit, P.stickHit)
        } else {
            right = PadRect(w - P.margin - P.faceHit - 16 - P.stickHit, bottom - P.stickHit, P.stickHit, P.stickHit)
        }
        out.append(PadControl(id: "rs", label: String(localized: L10n.padControlRightStick), rect: right,
                              kind: .stick(axisX: GamepadWire.axisRSX, axisY: GamepadWire.axisRSY)))
    }
    // Measured against the clusters, not guessed from the width: between `narrow` and about
    // 900 points the right stick still spreads into the bottom corner, near enough to sit on Start.
    func middle(_ y: Float) -> [PadControl] {
        [disc("select", "Select", "⧉", GamepadWire.back, w / 2 - 64, y, P.smallRadius),
         disc("guide", "Guide", "◎", GamepadWire.guide, w / 2, y, P.smallRadius),
         disc("start", "Start", "☰", GamepadWire.start, w / 2 + 64, y, P.smallRadius)]
    }
    let low = middle(bottom - P.smallRadius)
    let blocked = low.contains { m in out.contains { $0.rect.overlaps(m.rect) } }
    out += blocked ? middle(P.margin + P.smallRadius) : low
    let ringX = P.margin + 2 * P.bumperRadius + 8
    out.append(PadControl(id: "ring", label: String(localized: L10n.padControlRing),
                          rect: PadRect(ringX, P.margin + P.bumperRadius - P.smallRadius, 2 * P.smallRadius, 2 * P.smallRadius),
                          kind: .ring))
    return out
}

/// The preset with the user's overrides laid over it: each `PadTweak` moves a control's centre
/// to fractions of the layer, scales it about that centre, or marks it hidden. Scales clamp to
/// `VirtualPad.tweakScaleRange` and centres clamp so the control stays on the layer; a tweak
/// with no control in this preset is ignored.
public func applyPadTweaks(_ controls: [PadControl], tweaks: [String: PadTweak], w: Float, h: Float) -> [PadControl] {
    controls.map { c in
        guard let t = tweaks[c.id] else { return c }
        let sc = min(max(t.scale ?? 1, VirtualPad.tweakScaleRange.lowerBound), VirtualPad.tweakScaleRange.upperBound)
        let cw = c.rect.w * sc
        let ch = c.rect.h * sc
        let cx = min(max((t.x ?? ((c.rect.x + c.rect.w / 2) / w)) * w, min(cw / 2, w / 2)), max(w - cw / 2, w / 2))
        let cy = min(max((t.y ?? ((c.rect.y + c.rect.h / 2) / h)) * h, min(ch / 2, h / 2)), max(h - ch / 2, h / 2))
        return c.tweaked(rect: PadRect(cx - cw / 2, cy - ch / 2, cw, ch), sc: sc, hidden: t.hidden)
    }
}

/// The controls `pad` puts on a layer `w` × `h` points: the preset, then the overrides for the
/// layer's class — wide or narrow, the split `padControls` itself already makes — with the
/// hidden ones still in the list (the stream filters them; the editor ghosts them).
public func padControls(pad: PadConfig, w: Float, h: Float) -> [PadControl] {
    applyPadTweaks(padControls(layout: pad.layout, w: w, h: h),
                   tweaks: w < VirtualPad.narrow ? pad.controlsNarrow : pad.controls, w: w, h: h)
}

/// The D-pad bits for a finger `dx`, `dy` points from the centre (screen +y down): eight ways,
/// none inside `dead`.
public func dpadBits(dx: Float, dy: Float, dead: Float) -> UInt32 {
    if hypot(dx, dy) < dead { return 0 }
    let deg = Double(atan2(-dy, dx)) * 180 / .pi
    switch Int((deg + 22.5 + 360).truncatingRemainder(dividingBy: 360) / 45) {
    case 0: return GamepadWire.dpadRight
    case 1: return GamepadWire.dpadUp | GamepadWire.dpadRight
    case 2: return GamepadWire.dpadUp
    case 3: return GamepadWire.dpadUp | GamepadWire.dpadLeft
    case 4: return GamepadWire.dpadLeft
    case 5: return GamepadWire.dpadDown | GamepadWire.dpadLeft
    case 6: return GamepadWire.dpadDown
    default: return GamepadWire.dpadDown | GamepadWire.dpadRight
    }
}

/// A stick's wire pair for a travel of `dx`, `dy` points from where the finger landed: i16 with
/// +y up, nothing inside `dead`, the travel itself past it, full deflection at `radius` and beyond.
/// `dead` only gates: subtracting it would stack on the game's own dead zone.
public func stickWire(dx: Float, dy: Float, radius: Float, dead: Float) -> (x: Int32, y: Int32) {
    let mag = hypot(dx, dy)
    if mag <= dead { return (0, 0) }
    let v = min(mag / radius, 1) * 32767 / mag
    return (Int32((dx * v).rounded()), Int32((-dy * v).rounded()))
}

/// A trigger's wire value for a finger `y` points down a pill `h` tall: 0 at the top, 255 at the bottom.
public func triggerWire(y: Float, h: Float) -> Int32 {
    Int32((min(max(y / h, 0), 1) * 255).rounded())
}

/// The pad's wire slot (§4.2): the lowest free index beside any real controller from
/// `GamepadManager`'s one allocator, its Arrival before any input, held state flushed and its
/// Remove on `close` — a real controller's lifetime, the way `Sc2Capture` claims one. Declared an
/// Xbox 360 pad: the XInput identity every host builds, and the glyphs the layer draws. Main
/// actor, like the capture.
@MainActor
public final class VirtualPadWire {
    public let pad: UInt8
    private let connection: PunktfunkConnection
    private let manager: GamepadManager
    private var buttons: UInt32 = 0
    private var axes = [Int32](repeating: 0, count: 6)
    private var closed = false

    /// The ring owns the pad: everything held is released on the host now, and nothing is sent
    /// until it closes. On close nothing is replayed — the next change on a control re-establishes
    /// itself, exactly as a real pad's does.
    public var masked = false {
        didSet { if masked, !oldValue { flush() } }
    }

    /// Nil when all `GamepadWire.maxPads` indices are taken.
    public init?(connection: PunktfunkConnection, manager: GamepadManager) {
        guard let index = manager.reserveExternalPadIndex() else { return nil }
        pad = index
        self.connection = connection
        self.manager = manager
        connection.send(.gamepadArrival(pref: PunktfunkConnection.GamepadType.xbox360.rawValue, pad: UInt32(index)))
        // Wake the host pad: pads are created lazily from the first event.
        connection.send(.gamepadAxis(GamepadWire.axisLSX, value: 0, pad: UInt32(index)))
    }

    /// One button transition; only a change goes out.
    public func button(_ bit: UInt32, down: Bool) {
        guard !closed, !masked else { return }
        let next = down ? buttons | bit : buttons & ~bit
        guard next != buttons else { return }
        buttons = next
        connection.send(.gamepadButton(bit, down: down, pad: UInt32(pad)))
    }

    /// One axis value (stick i16 with +y up, trigger 0…255); only a change goes out.
    public func axis(_ id: UInt32, value: Int32) {
        guard !closed, !masked, Int(id) < axes.count, axes[Int(id)] != value else { return }
        axes[Int(id)] = value
        connection.send(.gamepadAxis(id, value: value, pad: UInt32(pad)))
    }

    /// Lift everything the host believes is held on this pad.
    private func flush() {
        for bit in GamepadWire.allButtons where buttons & bit != 0 {
            connection.send(.gamepadButton(bit, down: false, pad: UInt32(pad)))
        }
        buttons = 0
        for (i, v) in axes.enumerated() where v != 0 {
            connection.send(.gamepadAxis(UInt32(i), value: 0, pad: UInt32(pad)))
            axes[i] = 0
        }
    }

    /// Flush held state, signal the removal, and free the wire index. Idempotent.
    public func close() {
        guard !closed else { return }
        flush()
        closed = true
        connection.send(.gamepadRemove(pad: UInt32(pad)))
        manager.releaseExternalPadIndex(pad)
    }
}
