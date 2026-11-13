// Apple Pencil → state-full wire pen samples (design/pen-tablet-input.md §7).
//
// Every sample carries the COMPLETE pen state (in-range/touching/buttons + all axes) — the
// host diffs consecutive samples and synthesizes down/up/button transitions itself, so a lost
// datagram self-heals and this file never sends edge events. Three sources feed one stream:
// UITouch contacts (with coalesced samples for full 240 Hz fidelity), the view's
// Pencil-only hover recognizer, and
// UIPencilInteraction (squeeze held → barrel 1, double-tap → a momentary barrel 2 —
// Apple Pencil has no hardware eraser end or barrel buttons, so these mappings are how
// host-side apps get their stylus button/eraser affordances).
//
// HEARTBEAT (wire contract — see `PunktfunkPenSample` in punktfunk_core.h): while the pen is
// in range or touching, the last sample repeats every ≤100 ms even when nothing changed.
// UIKit is silent for a stationary Pencil, and the host force-releases the stroke after
// 200 ms without samples (its dead-client failsafe) — the timer keeps a held stroke alive.

#if os(iOS) || os(visionOS)
import PunktfunkCore
import UIKit

final class PencilStream: NSObject {
    enum Phase { case down, move, up, cancel }

    /// One assembled batch (≤ `PUNKTFUNK_PEN_BATCH_MAX` samples) ready for the connection.
    var send: (([PunktfunkPenSample]) -> Void)?
    /// View-space point → normalized [0,1] video coordinates (the letterbox mapping the
    /// touch path already uses). nil until a mode is negotiated — samples are dropped then.
    var videoNorm: ((CGPoint) -> (Float, Float)?)?

    private var inRange = false
    private var touching = false
    /// Squeeze held (mapped to wire BARREL1).
    private var squeezeHeld = false
    /// Whether this device/Pencil pair has demonstrated hover — decides what a lift means:
    /// hover-capable hardware keeps proximity (the hover recognizer owns the exit), anything
    /// else leaves range on lift so the host never parks a phantom hovering pen.
    private var sawHover = false
    private var last = PencilStream.idleSample()
    private var heartbeat: Timer?
    /// When the last batch (event or keepalive) went out — the heartbeat tick's idle test.
    private var lastSendTs: TimeInterval = 0

    // MARK: - Contact path (UITouch, `.pencil` only)

    func touches(_ touches: Set<UITouch>, event: UIEvent?, phase: Phase, in view: UIView) {
        // At most one Pencil exists; a set with several is UIKit batching phases of the same
        // stylus — the last one carries the freshest state.
        guard let touch = touches.max(by: { $0.timestamp < $1.timestamp }) else { return }
        switch phase {
        case .down, .move:
            touching = true
            inRange = true
            // Coalesced samples restore the Pencil's full capture rate (UIKit delivers at
            // display cadence); oldest first, `dt_us` preserving their spacing. The whole run
            // is kept — emit() splits anything over the wire's batch cap.
            let raw = event?.coalescedTouches(for: touch) ?? [touch]
            var batch: [PunktfunkPenSample] = []
            var prevTs: TimeInterval?
            for t in raw {
                guard let s = contactSample(t, in: view, prevTs: prevTs) else { continue }
                prevTs = t.timestamp
                batch.append(s)
            }
            emit(batch)
        case .up:
            touching = false
            // Hover-capable hardware: lift back to hover, the recognizer exits range later.
            // Otherwise a lift IS the range exit (mirror of the host's GameStream heuristic).
            inRange = sawHover
            var s = last
            s.pressure = 0
            s.state = stateBits()
            if let posSample = contactSample(touch, in: view, prevTs: nil) {
                s.x = posSample.x
                s.y = posSample.y
            }
            emit([s])
        case .cancel:
            release()
        }
    }

    // MARK: - Hover path (the view's Pencil-only hover recognizer)

    /// The recognizer admits `.pencil` only, so every event is the Pencil. `zOffset` is just
    /// the distance: it never decides whose hover this is.
    func hover(_ r: UIHoverGestureRecognizer, in view: UIView) {
        switch r.state {
        case .began, .changed:
            sawHover = true
            inRange = true
            touching = false
            guard let (x, y) = videoNorm?(r.location(in: view)) else { return }
            var s = PencilStream.idleSample()
            s.state = stateBits()
            s.x = x
            s.y = y
            s.distance = UInt16((r.zOffset.clamped(to: 0...1) * 65534).rounded())
            s.tilt_deg = Self.tiltDeg(altitude: r.altitudeAngle)
            s.azimuth_deg = Self.azimuthDeg(r.azimuthAngle(in: view))
            if #available(iOS 17.5, *) { s.roll_deg = Self.rollDeg(r.rollAngle) }
            emit([s])
        case .ended, .cancelled, .failed:
            // A hover that ends on contact leaves the stroke alone.
            if inRange, !touching { release() }
        default:
            break
        }
    }

    // MARK: - Lifecycle

    /// Session stop / view teardown: leave range so the host lifts anything held.
    func reset() {
        if inRange || touching { release() }
        sawHover = false
        squeezeHeld = false
    }

    private func release() {
        touching = false
        inRange = false
        var s = last
        s.pressure = 0
        s.state = 0
        emit([s])
    }

    // MARK: - Sample assembly

    private func contactSample(
        _ t: UITouch, in view: UIView, prevTs: TimeInterval?
    ) -> PunktfunkPenSample? {
        guard let (x, y) = videoNorm?(t.location(in: view)) else { return nil }
        var s = PencilStream.idleSample()
        s.state = stateBits()
        s.x = x
        s.y = y
        // maximumPossibleForce is 0 until the system knows the stylus — full force then
        // (binary-stylus semantics, matching the host's unknown-pressure rule).
        let maxForce = t.maximumPossibleForce
        s.pressure =
            maxForce > 0
            ? UInt16((Double(t.force / maxForce).clamped(to: 0...1) * 65535).rounded())
            : UInt16.max
        s.distance = 0
        s.tilt_deg = Self.tiltDeg(altitude: t.altitudeAngle)
        s.azimuth_deg = Self.azimuthDeg(t.azimuthAngle(in: view))
        if #available(iOS 17.5, *) { s.roll_deg = Self.rollDeg(t.rollAngle) }
        if let prevTs {
            s.dt_us = UInt16(((t.timestamp - prevTs) * 1_000_000).clamped(to: 0...65535))
        }
        return s
    }

    private func stateBits() -> UInt8 {
        var bits: UInt8 = 0
        if inRange || touching { bits |= UInt8(PUNKTFUNK_PEN_IN_RANGE) }
        if touching { bits |= UInt8(PUNKTFUNK_PEN_TOUCHING) }
        if squeezeHeld { bits |= UInt8(PUNKTFUNK_PEN_BARREL1) }
        return bits
    }

    private func emit(_ batch: [PunktfunkPenSample]) {
        guard !batch.isEmpty else { return }
        last = batch[batch.count - 1]
        last.dt_us = 0
        // A run longer than the wire's batch cap is SPLIT into consecutive sends, never
        // truncated (the send_pen contract): an over-cap run means the main thread hitched
        // past a frame of 240 Hz samples, and dropping its head would notch exactly the
        // stroke geometry a drawing app can least afford to lose.
        var start = 0
        while start < batch.count {
            let end = min(start + Int(PUNKTFUNK_PEN_BATCH_MAX), batch.count)
            send?(Array(batch[start..<end]))
            start = end
        }
        lastSendTs = CACurrentMediaTime()
        syncHeartbeat()
    }

    /// The ≤100 ms keepalive while in range (see the file header): ONE long-lived 50 ms timer
    /// in `.common` run-loop mode, armed on range entry and torn down on exit. `.default`-mode
    /// scheduling pauses during run-loop tracking, where a stationary pen would silently cross
    /// the host's 200 ms force-release mid-stroke; the old per-emit re-arm also churned the run
    /// loop once per event during a stroke. The tick resends only after ≥50 ms of send silence,
    /// so an active stroke costs nothing and the worst-case gap stays ≈100 ms — the wire bound.
    private func syncHeartbeat() {
        guard inRange || touching else {
            heartbeat?.invalidate()
            heartbeat = nil
            return
        }
        guard heartbeat == nil else { return }
        let timer = Timer(timeInterval: 0.05, repeats: true) { [weak self] t in
            guard let self, self.inRange || self.touching else {
                t.invalidate()
                self?.heartbeat = nil
                return
            }
            if CACurrentMediaTime() - self.lastSendTs >= 0.05 {
                self.send?([self.last])
                self.lastSendTs = CACurrentMediaTime()
            }
        }
        heartbeat = timer
        RunLoop.main.add(timer, forMode: .common)
    }

    // MARK: - Angle conversions

    /// Altitude (π/2 = perpendicular) → wire tilt-from-normal in degrees, 0...90.
    private static func tiltDeg(altitude: CGFloat) -> UInt8 {
        UInt8((90 - altitude * 180 / .pi).rounded().clamped(to: 0...90))
    }

    /// Apple azimuth (0 along the view's +x axis, clockwise, y-down) → wire azimuth
    /// (0 = north/up on screen, clockwise): +90° offset.
    private static func azimuthDeg(_ apple: CGFloat) -> UInt16 {
        let deg = (apple * 180 / .pi + 90).truncatingRemainder(dividingBy: 360)
        return UInt16((deg + 360).truncatingRemainder(dividingBy: 360).rounded()) % 360
    }

    /// Pencil Pro roll (radians, −π...π) → wire barrel roll 0...359°.
    private static func rollDeg(_ roll: CGFloat) -> UInt16 {
        let deg = (roll * 180 / .pi).truncatingRemainder(dividingBy: 360)
        return UInt16(((deg + 360).truncatingRemainder(dividingBy: 360)).rounded()) % 360
    }

    /// All-unknown baseline: sentinel angles/distance, tool = pen (the eraser is host-side
    /// state driven by the squeeze/tap mappings, not a hardware end).
    private static func idleSample() -> PunktfunkPenSample {
        PunktfunkPenSample(
            x: 0, y: 0, pressure: 0,
            distance: UInt16(PUNKTFUNK_PEN_DISTANCE_UNKNOWN),
            azimuth_deg: UInt16(PUNKTFUNK_PEN_ANGLE_UNKNOWN),
            roll_deg: UInt16(PUNKTFUNK_PEN_ANGLE_UNKNOWN),
            dt_us: 0, state: 0,
            tool: UInt8(PUNKTFUNK_PEN_TOOL_PEN),
            tilt_deg: UInt8(PUNKTFUNK_PEN_TILT_UNKNOWN),
            _reserved: (0, 0, 0))
    }
}

#if os(iOS)
// Squeeze → barrel 1 held, tap → barrel 2 click. visionOS has no Apple Pencil interaction.
extension PencilStream: UIPencilInteractionDelegate {
    @available(iOS 17.5, *)
    func pencilInteraction(
        _ interaction: UIPencilInteraction, didReceiveSqueeze squeeze: UIPencilInteraction.Squeeze
    ) {
        switch squeeze.phase {
        case .began:
            squeezeHeld = true
        case .ended, .cancelled:
            squeezeHeld = false
        default:
            return
        }
        guard inRange || touching else { return }
        var s = last
        s.state = stateBits()
        emit([s])
    }

    func pencilInteractionDidTap(_ interaction: UIPencilInteraction) {
        guard inRange || touching else { return }
        // A momentary barrel-2 click: press + release as two state-full samples in ONE batch
        // — the host's tracker emits the button press and release in order.
        var press = last
        press.state = stateBits() | UInt8(PUNKTFUNK_PEN_BARREL2)
        var releaseS = last
        releaseS.state = stateBits()
        emit([press, releaseS])
    }
}
#endif
#endif
