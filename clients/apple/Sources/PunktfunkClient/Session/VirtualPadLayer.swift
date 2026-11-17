// The virtual on-screen controller's layer (design/touch-client-overlay.md §4.2): the preset's
// controls placed over the stream, each a UIKit view that owns its own fingers — UIKit keeps a
// touch with the view it began in, which is the multi-touch SwiftUI has not got. The layer draws
// nothing between the controls, so a finger landing there never reaches it and falls through to
// `StreamLayerUIView` beneath: tap-to-click keeps working beside the pad. Same geometry, same
// input rules as the Android `VirtualPad.kt` (`padControls`, `dpadBits`, `stickWire`,
// `triggerWire` live in the kit).

#if os(iOS) || os(visionOS)
import PunktfunkKit
import PunktfunkShared
import SwiftUI
import UIKit

/// The controller over the stream. Mounted only while the pad is shown (tenet 1). `openRing`
/// takes the ring button's centre in the layer's points.
struct VirtualPadLayer: View {
    let config: PadConfig
    let wire: VirtualPadWire
    let openRing: (CGPoint) -> Void

    var body: some View {
        GeometryReader { geo in
            let scale = CGFloat(min(max(config.scale, VirtualPad.scaleRange.lowerBound), VirtualPad.scaleRange.upperBound))
            let opacity = CGFloat(min(max(config.opacity, VirtualPad.opacityRange.lowerBound), 1))
            let controls = padControls(pad: config,
                                       w: Float(geo.size.width / scale), h: Float(geo.size.height / scale))
                .filter { !$0.hidden }
            ForEach(controls, id: \.id) { c in
                let centre = CGPoint(x: (CGFloat(c.rect.x) + CGFloat(c.rect.w) / 2) * scale,
                                     y: (CGFloat(c.rect.y) + CGFloat(c.rect.h) / 2) * scale)
                PadControlHost(control: c, scale: scale, opacity: opacity, wire: wire,
                               openRing: { openRing(centre) })
                    .frame(width: CGFloat(c.rect.w) * scale, height: CGFloat(c.rect.h) * scale)
                    .position(centre)
                    .accessibilityLabel(c.label)
            }
        }
        .ignoresSafeArea()
    }
}

/// One control as the stream draws it. The layout editor mounts the same view with no wire and
/// `interactive: false`, so its own SwiftUI drag can own the fingers over identical pixels.
struct PadControlHost: UIViewRepresentable {
    let control: PadControl
    let scale: CGFloat
    let opacity: CGFloat
    let wire: VirtualPadWire?
    var interactive = true
    var openRing: (() -> Void)?

    func makeUIView(context: Context) -> PadControlUIView {
        PadControlUIView(control: control, scale: scale)
    }

    func updateUIView(_ view: PadControlUIView, context: Context) {
        view.control = control
        view.scale = scale
        view.wire = wire
        view.openRing = openRing
        view.baseAlpha = opacity
        view.isUserInteractionEnabled = interactive
        view.refresh()
    }
}

private let fill = UIColor(white: 1, alpha: 0.16)
private let fillOn = UIColor(white: 1, alpha: 0.55)
private let edge = UIColor(white: 1, alpha: 0.75)

/// One control: its fingers, its wire state, and its drawing. Buttons and the D-pad resolve to a
/// set of bits (the union over every finger, sent on change); a stick, a trigger and the ring
/// button are owned by their first finger, and the ring button fires on a lift inside it.
final class PadControlUIView: UIView {
    /// Refreshed by `updateUIView`: a tweak resizes a control mid-life without remaking it.
    var control: PadControl
    var scale: CGFloat
    var wire: VirtualPadWire?
    var openRing: (() -> Void)?
    var baseAlpha: CGFloat = 0.45

    private var fingers: [ObjectIdentifier: UInt32] = [:]
    private var held: UInt32 = 0
    private var owner: UITouch?
    private var origin = CGPoint.zero
    private var lastX: Int32 = 0
    private var lastY: Int32 = 0
    private var lastPull: Int32 = 0
    /// The knob's offset from the base centre, points.
    private var knob = CGPoint.zero
    /// 0…1, how far down the pill the finger is.
    private var pull: CGFloat = 0
    private var active = false

    #if os(visionOS)
    /// `UIImpactFeedbackGenerator` is unavailable on visionOS (XROS 27.0 SDK), so the pad's tick
    /// is silent there.
    private struct SilentTick {
        func prepare() {}
        func impactOccurred() {}
    }
    private static let tick = SilentTick()
    #else
    private static let tick = UIImpactFeedbackGenerator(style: .light)
    #endif

    /// Half the 1.5 pt stroke. A stroke is centred on its path, and the single discs, the
    /// D-pad cross and the trigger pill run their paths along the view's own edge — without
    /// this inset the outer half of the line falls outside the backing store and is clipped.
    private static let strokeInset: CGFloat = 0.75

    init(control: PadControl, scale: CGFloat) {
        self.control = control
        self.scale = scale
        super.init(frame: .zero)
        backgroundColor = .clear
        isOpaque = false
        isMultipleTouchEnabled = true
        contentMode = .redraw
        Self.tick.prepare()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    func refresh() {
        alpha = active ? min(1, baseAlpha + 0.35) : baseAlpha
        setNeedsDisplay()
    }

    // MARK: fingers

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches { down(t, t.location(in: self)) }
    }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches { move(t, t.location(in: self)) }
    }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches {
            if case .ring = control.kind, t === owner, bounds.contains(t.location(in: self)) { openRing?() }
            up(t)
        }
    }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        for t in touches { up(t) }
    }
    override func willMove(toSuperview newSuperview: UIView?) {
        // The layer left (hidden, rotated, or the session ended) mid-touch.
        if newSuperview == nil { reset() }
    }

    private func down(_ t: UITouch, _ p: CGPoint) {
        switch control.kind {
        case .buttons, .dpad:
            fingers[ObjectIdentifier(t)] = hit(p)
            sync()
        case .stick:
            guard owner == nil else { return }
            owner = t
            origin = p
            emitStick(.zero)
        case .trigger:
            guard owner == nil else { return }
            owner = t
            Self.tick.impactOccurred()
            emitTrigger(p.y)
        case .ring:
            guard owner == nil else { return }
            owner = t
            Self.tick.impactOccurred()
            active = true
            refresh()
        }
    }

    private func move(_ t: UITouch, _ p: CGPoint) {
        switch control.kind {
        case .buttons, .dpad:
            guard fingers[ObjectIdentifier(t)] != nil else { return }
            fingers[ObjectIdentifier(t)] = hit(p)
            sync()
        case .stick:
            guard t === owner else { return }
            emitStick(CGPoint(x: p.x - origin.x, y: p.y - origin.y))
        case .trigger:
            guard t === owner else { return }
            emitTrigger(p.y)
        case .ring:
            break
        }
    }

    private func up(_ t: UITouch) {
        switch control.kind {
        case .buttons, .dpad:
            fingers.removeValue(forKey: ObjectIdentifier(t))
            sync()
        case .stick:
            guard t === owner else { return }
            owner = nil
            emitStick(.zero)
        case .trigger:
            guard t === owner else { return }
            owner = nil
            emitTrigger(0)
        case .ring:
            guard t === owner else { return }
            owner = nil
            active = false
            refresh()
        }
    }

    private func reset() {
        fingers.removeAll()
        owner = nil
        switch control.kind {
        case .buttons, .dpad: sync()
        case .stick: emitStick(.zero)
        case .trigger: emitTrigger(0)
        case .ring: active = false
        }
    }

    /// The bits under a finger at `p`: the nearest disc within slop, or the D-pad's angle.
    private func hit(_ p: CGPoint) -> UInt32 {
        switch control.kind {
        case .buttons(let discs):
            var best: UInt32 = 0
            var bestD = CGFloat.greatestFiniteMagnitude
            for d in discs {
                let dist = hypot(p.x - CGFloat(d.cx) * scale, p.y - CGFloat(d.cy) * scale)
                if dist <= CGFloat(d.r) * scale * CGFloat(VirtualPad.hitSlop), dist < bestD {
                    best = d.bit
                    bestD = dist
                }
            }
            return best
        case .dpad:
            let c = bounds.width / 2
            return dpadBits(dx: Float(p.x - c), dy: Float(p.y - c), dead: VirtualPad.dpadDead * Float(scale) * control.sc)
        case .stick, .trigger, .ring:
            return 0
        }
    }

    private func sync() {
        let next = fingers.values.reduce(0) { $0 | $1 }
        guard next != held else { return }
        for bit in GamepadWire.allButtons where (next ^ held) & bit != 0 {
            let down = next & bit != 0
            wire?.button(bit, down: down)
            if down { Self.tick.impactOccurred() }
        }
        held = next
        active = next != 0
        refresh()
    }

    private func emitStick(_ d: CGPoint) {
        guard case .stick(let axisX, let axisY) = control.kind else { return }
        let radius = CGFloat(VirtualPad.stickRadius) * scale * CGFloat(control.sc)
        let (x, y) = stickWire(dx: Float(d.x), dy: Float(d.y), radius: Float(radius), dead: VirtualPad.stickDead * Float(scale) * control.sc)
        if x != lastX { wire?.axis(axisX, value: x); lastX = x }
        if y != lastY { wire?.axis(axisY, value: y); lastY = y }
        knob = CGPoint(x: CGFloat(x) / 32767 * radius, y: -CGFloat(y) / 32767 * radius)
        active = owner != nil
        refresh()
    }

    private func emitTrigger(_ y: CGFloat) {
        guard case .trigger(let axis) = control.kind else { return }
        let v = triggerWire(y: Float(y), h: Float(bounds.height))
        if v != lastPull { wire?.axis(axis, value: v); lastPull = v }
        pull = CGFloat(v) / 255
        active = v > 0
        refresh()
    }

    // MARK: drawing

    override func draw(_ rect: CGRect) {
        switch control.kind {
        case .buttons(let discs):
            for d in discs {
                let r = CGFloat(d.r) * scale
                let circle = UIBezierPath(ovalIn: CGRect(x: CGFloat(d.cx) * scale - r, y: CGFloat(d.cy) * scale - r, width: 2 * r, height: 2 * r)
                    .insetBy(dx: Self.strokeInset, dy: Self.strokeInset))
                (held & d.bit != 0 ? fillOn : fill).setFill()
                circle.fill()
                edge.setStroke()
                circle.lineWidth = 1.5
                circle.stroke()
                glyph(d.glyph, at: CGPoint(x: CGFloat(d.cx) * scale, y: CGFloat(d.cy) * scale), size: r * 0.7)
            }
        case .dpad:
            let inset = Self.strokeInset
            let s = min(bounds.width, bounds.height) - 2 * inset
            let arm = s * 0.34
            let c = min(bounds.width, bounds.height) / 2
            let vertical = UIBezierPath(roundedRect: CGRect(x: c - arm / 2, y: inset, width: arm, height: s), cornerRadius: arm / 4)
            let horizontal = UIBezierPath(roundedRect: CGRect(x: inset, y: c - arm / 2, width: s, height: arm), cornerRadius: arm / 4)
            fill.setFill()
            vertical.fill()
            horizontal.fill()
            edge.setStroke()
            vertical.lineWidth = 1.5
            horizontal.lineWidth = 1.5
            vertical.stroke()
            horizontal.stroke()
            let reach = c - arm / 2 - inset
            fillOn.setFill()
            if held & GamepadWire.dpadUp != 0 {
                UIBezierPath(roundedRect: CGRect(x: c - arm / 2, y: inset, width: arm, height: reach), cornerRadius: arm / 4).fill()
            }
            if held & GamepadWire.dpadDown != 0 {
                UIBezierPath(roundedRect: CGRect(x: c - arm / 2, y: c + arm / 2, width: arm, height: reach), cornerRadius: arm / 4).fill()
            }
            if held & GamepadWire.dpadLeft != 0 {
                UIBezierPath(roundedRect: CGRect(x: inset, y: c - arm / 2, width: reach, height: arm), cornerRadius: arm / 4).fill()
            }
            if held & GamepadWire.dpadRight != 0 {
                UIBezierPath(roundedRect: CGRect(x: c + arm / 2, y: c - arm / 2, width: reach, height: arm), cornerRadius: arm / 4).fill()
            }
        case .stick:
            let c = CGPoint(x: bounds.midX, y: bounds.midY)
            let r = CGFloat(VirtualPad.stickRadius) * scale * CGFloat(control.sc)
            let base = UIBezierPath(ovalIn: CGRect(x: c.x - r, y: c.y - r, width: 2 * r, height: 2 * r))
            UIColor(white: 1, alpha: 0.12).setFill()
            base.fill()
            edge.setStroke()
            base.lineWidth = 1.5
            base.stroke()
            let k = CGFloat(VirtualPad.stickKnobRadius) * scale * CGFloat(control.sc)
            fillOn.setFill()
            UIBezierPath(ovalIn: CGRect(x: c.x + knob.x - k, y: c.y + knob.y - k, width: 2 * k, height: 2 * k)).fill()
        case .trigger(let axis):
            let pillRect = bounds.insetBy(dx: Self.strokeInset, dy: Self.strokeInset)
            let pill = UIBezierPath(roundedRect: pillRect, cornerRadius: pillRect.width / 2)
            fill.setFill()
            pill.fill()
            if pull > 0, let ctx = UIGraphicsGetCurrentContext() {
                ctx.saveGState()
                pill.addClip()
                fillOn.setFill()
                UIRectFill(CGRect(x: 0, y: 0, width: bounds.width, height: bounds.height * pull))
                ctx.restoreGState()
            }
            edge.setStroke()
            pill.lineWidth = 1.5
            pill.stroke()
            glyph(axis == GamepadWire.axisLT ? "LT" : "RT", at: CGPoint(x: bounds.midX, y: bounds.midY), size: 15 * scale * CGFloat(control.sc))
        case .ring:
            let circle = UIBezierPath(ovalIn: bounds.insetBy(dx: Self.strokeInset, dy: Self.strokeInset))
            (active ? fillOn : fill).setFill()
            circle.fill()
            edge.setStroke()
            circle.lineWidth = 1.5
            circle.stroke()
            glyph("•••", at: CGPoint(x: bounds.midX, y: bounds.midY), size: bounds.width * 0.3)
        }
    }

    private func glyph(_ text: String, at centre: CGPoint, size: CGFloat) {
        let attrs: [NSAttributedString.Key: Any] = [
            .font: UIFont.systemFont(ofSize: size, weight: .bold),
            .foregroundColor: UIColor.white,
        ]
        let s = (text as NSString).size(withAttributes: attrs)
        (text as NSString).draw(at: CGPoint(x: centre.x - s.width / 2, y: centre.y - s.height / 2), withAttributes: attrs)
    }
}
#endif
