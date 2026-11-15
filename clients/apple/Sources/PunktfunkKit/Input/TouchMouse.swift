// Finger touches → host mouse, for the touchscreen devices: a port of the Android client's
// touch gesture model (clients/android .../TouchInput.kt) so the two touch clients feel
// identical. Two mouse modes share one gesture vocabulary — tap = left click · two-finger
// tap = right click · two-finger drag = scroll · tap-then-press-and-drag OR press-and-hold =
// held left drag (text selection / window moves) · three-finger tap = cycles the stats overlay tiers
// (off → compact → normal → detailed, matching Android) · three-finger swipe up/down =
// summon/dismiss the local soft keyboard for typing on the host (`onKeyboardGesture`):
//
//  * trackpad (default): the cursor STAYS PUT on touch-down and moves by the finger's
//    relative delta with mild acceleration — swipe to nudge, lift and re-swipe to walk it
//    across, tap to click where it is. This is what makes the cursor reachable on a small
//    screen.
//  * pointer: the cursor jumps to the finger and follows it (absolute moves through the
//    aspect-fit letterbox) — direct pointing for desktop-style use.
//
// `touch` never reaches this type: `StreamLayerUIView` forwards those fingers as REAL wire
// touches (multi-touch passthrough) instead. `off` runs a second instance with no `send`.

import Foundation
import PunktfunkShared

/// How touchscreen fingers drive the host — persisted under `DefaultsKey.touchMode`, latched
/// per gesture by `StreamLayerUIView` (a Settings change applies from the NEXT touch, and a
/// gesture never splits across models). `trackpad` is the default: a cursor is the
/// universally workable model; passthrough only helps hosts/apps that actually speak touch.
/// Every platform (the ring's Touch mode slot names it on tvOS, dimmed); the machine below
/// is iOS-only.
public enum TouchInputMode: String, CaseIterable, Sendable {
    case trackpad
    case pointer
    case touch
    /// Fingers reach the host as nothing; the twist, keyboard swipe and stats tap still run.
    case off

    /// The session's setting, defaulting to trackpad when unknown — unless the ring's Touch mode
    /// slot cycled it for this session (`sessionOverride`, cleared at session end).
    public static func current(_ settings: EffectiveSettings) -> TouchInputMode {
        sessionOverride ?? TouchInputMode(rawValue: settings.touchMode) ?? .trackpad
    }

    /// A mid-stream switch from the quick-action ring: session-scoped, never persisted, and
    /// applied from the NEXT gesture (the stream view latches the route per gesture).
    public static var sessionOverride: TouchInputMode?
}

/// Gesture/ballistics tuning. Distances are in points where they gate gestures; the
/// relative ballistics work in PHYSICAL pixels (point deltas × screen scale) so the
/// acceleration curve matches the Android client's pixel-based constants 1:1, which
/// `TouchVectorTests` checks against `clients/shared/touch-vectors.json`.
enum TouchTuning {
    /// Movement under this (pt) still counts as a tap, not a drag.
    static let tapSlop: CGFloat = 8
    /// A new touch this soon (s) after a tap, near it, starts a held left-button drag.
    static let tapDragWindow: TimeInterval = 0.25
    /// One finger held still this long (s) presses the left button and drags until it
    /// lifts — the touch idiom for "pick this up".
    static let longPress: TimeInterval = 0.5
    /// Wire scroll units per point of two-finger pan: points are DIP, so the Q24.8
    /// distance is the centroid travel × 256 — the content travels with the fingers.
    static let scrollUnitsPerPt: CGFloat = 256
    /// Base finger-px → host-px gain (~1:1, never twitchy). The acceleration below lets a
    /// flick cross the screen while a slow drag stays precise.
    static let pointerSens: CGFloat = 1.3
    /// Above `accelSpeedFloor` px/ms the gain ramps by `accelGain` per px/ms, capped at
    /// `accelMax` (so a fast swipe can't fling the cursor uncontrollably).
    static let accelGain: CGFloat = 0.6
    static let accelSpeedFloor: CGFloat = 0.3
    static let accelMax: CGFloat = 3.0
    /// Three-finger vertical swipe: the fraction of the view height the centroid must
    /// travel to summon (up) / dismiss (down) the local soft keyboard.
    static let keyboardSwipeFraction: CGFloat = 0.10
    /// The dial (design/touch-client-overlay.md §2.1): a two-finger TWIST opens the
    /// quick-action ring. Below `dialArmDeg` the gesture is still a scroll candidate —
    /// natural scrolls rotate a few degrees, and this absorbs them; at `dialCommitDeg` the
    /// ring commits and stays open after the fingers lift. The scroll never waits: until
    /// the centroid travels `dialSlop` (pt) the twist can still arm, past it never.
    static let dialArmDeg: CGFloat = 10
    static let dialCommitDeg: CGFloat = 30
    static let dialSlop: CGFloat = 2 * tapSlop

    /// Acceleration multiplier for a finger speed in physical px per ms.
    static func accel(forSpeed speed: CGFloat) -> CGFloat {
        min(1 + accelGain * max(speed - accelSpeedFloor, 0), accelMax)
    }

    /// The pair's turn from `a` to `b` in degrees, signed: + is clockwise on a y-down screen.
    static func twistDegrees(from a: CGVector, to b: CGVector) -> CGFloat {
        atan2(a.dx * b.dy - a.dy * b.dx, a.dx * b.dx + a.dy * b.dy) * 180 / .pi
    }

    /// An undecided pair that turned `degrees` with its centroid `travel` pt from where it formed
    /// becomes the twist. The twist's fingers are the client's, never the host touchpad's.
    static func twistArms(degrees: CGFloat, travel: CGFloat) -> Bool {
        travel < dialSlop && abs(degrees) >= dialArmDeg
    }
}

#if os(iOS) || os(visionOS)
import PunktfunkCore
import UIKit

/// The gesture state machine behind the two mouse modes. One instance per stream view, fed
/// only the DIRECT touches (fingers/Pencil — indirect pointers have their own path). Runs
/// entirely on the main thread (UIKit touch delivery). Touches are tracked by identity key
/// with positions cached per event — `UITouch` objects are never retained.
final class TouchMouse {
    typealias Tuning = TouchTuning

    /// Wire events out (the owner gates them on its capture state).
    var send: ((PunktfunkInputEvent) -> Void)?
    /// View-space point → host-mode pixels through the letterbox (pointer mode's moves).
    var hostPoint: ((CGPoint) -> StreamLayerUIView.HostPoint?)?
    /// Three-finger vertical swipe crossed the threshold: `true` = show the local soft
    /// keyboard (swipe up), `false` = dismiss it (swipe down). Fires at most once per gesture.
    var onKeyboardGesture: ((Bool) -> Void)?
    /// The two-finger twist turning the quick-action ring (`DialEvent`).
    var onDial: ((DialEvent) -> Void)?

    /// The twist: the finger-to-finger vector when the pair formed, the centroid then, and
    /// whether it has armed (owns the gesture) / committed (the ring stays open).
    private struct Dial {
        let keys: (ObjectIdentifier, ObjectIdentifier)
        let vec: CGVector
        let anchor: CGPoint
        var armed = false
        var committed = false
    }

    private var dial: Dial?
    /// The pair travelled past `dialSlop` unarmed: a scroll for the gesture's lifetime.
    private var scrollLocked = false
    /// Wire axes an in-flight scroll has opened — Begin'd and closed (End/Cancel) here.
    private var scrollOpen = (v: false, h: false)
    /// Units scrolled while the pair was undecided, sent back if it turns out a twist or a tap.
    private var provisional = (x: Int32(0), y: Int32(0))
    /// Sub-unit scroll remainder, so a slow pan is not lost to truncation.
    private var scrollCarry = CGPoint.zero

    /// No gesture in flight (all fingers up) — the view uses this to release its mode latch.
    var isIdle: Bool { !sessionActive && lastPos.isEmpty }
    /// Every finger down now, by identity, in view points: the view's touchpad contacts.
    var fingers: [ObjectIdentifier: CGPoint] { lastPos }
    /// The twist armed, so this gesture's fingers are the client's, not the host touchpad's.
    var holdsFingers: Bool { dial?.armed == true }

    private var trackpad = true
    /// Last known position per active finger (identity key) — kept because moved events only
    /// carry the CHANGED touches while the scroll centroid needs every finger.
    private var lastPos: [ObjectIdentifier: CGPoint] = [:]
    private var sessionActive = false
    private var startPoint = CGPoint.zero
    private var maxFingers = 0
    private var moved = false
    private var scrolling = false
    private var dragHeld = false
    // Trackpad relative-motion state: the tracked finger, its last position/time, and the
    // sub-pixel remainder so a slow drag isn't lost to integer truncation.
    private var trackKey: ObjectIdentifier?
    private var prevPoint = CGPoint.zero
    private var prevTime: TimeInterval = 0
    private var carryX: CGFloat = 0
    private var carryY: CGFloat = 0
    /// Centroid at the last scroll step.
    private var scrollAnchor = CGPoint.zero
    // Keyboard-swipe state: the 3+-finger centroid anchor (per finger count, like the scroll
    // anchor) and a once-per-gesture latch.
    private var kbCount = 0
    private var kbAnchor = CGPoint.zero
    private var kbFired = false
    // Tap-drag arming: a quick tap leaves a window in which the next nearby touch drags.
    private var lastTapUp: TimeInterval = 0
    private var lastTapPoint = CGPoint.zero
    /// The pending long-press arm — a still finger raises no touch event, so the hold is a
    /// main-queue timer, cancelled by motion past the slop, a second finger, or any lift.
    private var longPress: DispatchWorkItem?

    /// GameStream mouse button ids.
    private enum Button { static let left: UInt32 = 1; static let right: UInt32 = 3 }

    func began(_ touches: Set<UITouch>, in view: UIView, trackpad: Bool) {
        let starting = lastPos.isEmpty
        for touch in touches {
            lastPos[ObjectIdentifier(touch)] = touch.location(in: view)
        }
        if starting, let first = touches.first {
            self.trackpad = trackpad
            sessionActive = true
            startPoint = first.location(in: view)
            maxFingers = 0
            moved = false
            scrolling = false
            scrollLocked = false
            scrollClose(PUNKTFUNK_SCROLL_PHASE_CANCEL) // a leak can't survive the gesture break
            provisional = (0, 0)
            scrollCarry = .zero
            dial = nil
            kbCount = 0
            kbFired = false
            // A touch landing just after a quick tap nearby = tap-and-drag: hold the left
            // button for this whole gesture (laptop-trackpad convention).
            dragHeld = first.timestamp - lastTapUp < Tuning.tapDragWindow
                && abs(startPoint.x - lastTapPoint.x) < Tuning.tapSlop
                && abs(startPoint.y - lastTapPoint.y) < Tuning.tapSlop
            lastTapUp = 0 // consume the arming either way
            // Pointer mode jumps the cursor to the finger; trackpad leaves it put (the whole
            // point — you nudge it with swipes instead).
            if !trackpad, let h = hostPoint?(startPoint) {
                send?(.mouseMoveAbs(x: h.x, y: h.y, surfaceWidth: h.w, surfaceHeight: h.h))
            }
            if dragHeld { send?(.mouseButton(Button.left, down: true)) }
            trackKey = ObjectIdentifier(first)
            prevPoint = startPoint
            prevTime = first.timestamp
            carryX = 0
            carryY = 0
            if !dragHeld { scheduleLongPress() }
        }
        maxFingers = max(maxFingers, lastPos.count)
        if lastPos.count > 1 { cancelLongPress() }
        switch lastPos.count {
        case 2 where !scrollLocked && dial == nil:
            // The second finger: remember the finger-to-finger vector — the dial's reference.
            let keys = Array(lastPos.keys)
            let (a, b) = (lastPos[keys[0]]!, lastPos[keys[1]]!)
            dial = Dial(
                keys: (keys[0], keys[1]),
                vec: CGVector(dx: b.x - a.x, dy: b.y - a.y),
                anchor: CGPoint(x: (a.x + b.x) / 2, y: (a.y + b.y) / 2))
        case 3...:
            endDial() // a third finger ends the twist
            endScroll() // and the in-flight scroll: the pair that drove it is gone
        default:
            break
        }
    }

    /// The twist is over (a finger lifted or a third landed): an armed-but-uncommitted ring
    /// winds back in; a committed ring stays open for the UI.
    private func endDial() {
        guard let d = dial else { return }
        dial = nil
        if d.armed, !d.committed { onDial?(.cancel) }
    }

    /// The dial's share of a two-finger move; `true` when the twist owns the gesture (the
    /// scroll path then never runs). Rules, in order (design §2.1): centroid travel past
    /// `dialSlop` before arming locks a scroll; rotation ≥ `dialArmDeg` arms the twist, which
    /// takes back the provisional scroll; progress `(|Δφ| − arm) / (commit − arm)` feeds the
    /// ring; at 1 it commits, and winding back to 0 after a commit closes it again.
    private func dialStep() -> Bool {
        guard var d = dial, d.armed || !scrollLocked,
              let a = lastPos[d.keys.0], let b = lastPos[d.keys.1]
        else { return false }
        let phi = Tuning.twistDegrees(from: d.vec, to: CGVector(dx: b.x - a.x, dy: b.y - a.y))
        let c = CGPoint(x: (a.x + b.x) / 2, y: (a.y + b.y) / 2)
        if !d.armed {
            let travel = hypot(c.x - d.anchor.x, c.y - d.anchor.y)
            if travel >= Tuning.tapSlop { moved = true }
            if travel >= Tuning.dialSlop {
                scrollLocked = true
                provisional = (0, 0)
                return false
            }
            // Undecided: the scroll goes out now, provisionally.
            if !Tuning.twistArms(degrees: phi, travel: travel) { return false }
            rollBackProvisional()
            d.armed = true
            moved = true // a twist is never a tap…
            scrolling = true // …and dropping to one finger must not jerk the cursor
        }
        let progress = min(max((abs(phi) - Tuning.dialArmDeg) / (Tuning.dialCommitDeg - Tuning.dialArmDeg), 0), 1)
        onDial?(.turn(progress: progress, clockwise: phi > 0, at: c))
        if progress >= 1, !d.committed {
            d.committed = true
            onDial?(.commit)
        } else if progress <= 0, d.committed {
            d.committed = false
            onDial?(.cancel)
        }
        dial = d
        return true
    }

    private func scheduleLongPress() {
        cancelLongPress()
        let item = DispatchWorkItem { [weak self] in self?.longPressFired() }
        longPress = item
        DispatchQueue.main.asyncAfter(deadline: .now() + Tuning.longPress, execute: item)
    }

    private func cancelLongPress() {
        longPress?.cancel()
        longPress = nil
    }

    /// The hold elapsed: one finger, never a second, still under the slop → press and hold
    /// the left button; `ended` releases it exactly like a tap-then-drag.
    private func longPressFired() {
        longPress = nil
        guard sessionActive, lastPos.count == 1, maxFingers == 1, !moved, !dragHeld else { return }
        dragHeld = true
        send?(.mouseButton(Button.left, down: true))
    }

    func moved(_ touches: Set<UITouch>, in view: UIView) {
        guard sessionActive else { return }
        for touch in touches where lastPos[ObjectIdentifier(touch)] != nil {
            lastPos[ObjectIdentifier(touch)] = touch.location(in: view)
        }
        // Dropping below three fingers forgets the keyboard-swipe anchor, so a 3→2→3 bounce
        // re-anchors instead of reading the count change as swipe travel.
        if lastPos.count < 3 { kbCount = 0 }
        if lastPos.count == 2 {
            if !dialStep() { scrollByCentroid() }
        } else if lastPos.count >= 3 {
            endScroll() // 3+ is the keyboard swipe — a committed scroll's axes end here
            keyboardSwipe(in: view)
        } else if !scrolling, let touch = touches.first(where: {
            lastPos[ObjectIdentifier($0)] != nil
        }) {
            singleFinger(touch, in: view)
        }
        if moved { cancelLongPress() }
    }

    func ended(_ touches: Set<UITouch>, in view: UIView) {
        guard sessionActive || !lastPos.isEmpty else { return }
        cancelLongPress()
        var upTime: TimeInterval = 0
        for touch in touches {
            lastPos.removeValue(forKey: ObjectIdentifier(touch))
            if trackKey == ObjectIdentifier(touch) { trackKey = nil }
            upTime = max(upTime, touch.timestamp)
        }
        endDial() // any lift ends the twist
        // A committed scroll's axes end with the pair that drove them — the last lift below
        // decides between the tap's rollback and the closing End for everything still open.
        if lastPos.count < 2, scrollLocked { endScroll() }
        guard lastPos.isEmpty, sessionActive else { return }
        sessionActive = false
        if dragHeld {
            dragHeld = false
            send?(.mouseButton(Button.left, down: false)) // end the drag
            endScroll()
        } else if !moved {
            rollBackProvisional() // a tap's jitter must not leave the page nudged
            switch maxFingers {
            case 3...:
                Self.cycleStats() // in-stream stats-tier cycle, same as Android
            case 2: // two-finger tap → right click
                send?(.mouseButton(Button.right, down: true))
                send?(.mouseButton(Button.right, down: false))
            default: // tap → left click (at the cursor's current spot), arm tap-drag
                send?(.mouseButton(Button.left, down: true))
                send?(.mouseButton(Button.left, down: false))
                lastTapUp = upTime
                lastTapPoint = startPoint
            }
        } else {
            endScroll()
        }
    }

    /// System-cancelled touches (incoming call, gesture takeover): release anything held but
    /// never synthesize a click out of a cancellation.
    func cancelled(_ touches: Set<UITouch>) {
        cancelLongPress()
        endDial()
        for touch in touches {
            lastPos.removeValue(forKey: ObjectIdentifier(touch))
            if trackKey == ObjectIdentifier(touch) { trackKey = nil }
        }
        if lastPos.isEmpty { abortSession() }
    }

    /// Session teardown: release anything held on the wire and forget all gesture state.
    func reset() {
        lastPos.removeAll()
        trackKey = nil
        abortSession()
        lastTapUp = 0
    }

    private func abortSession() {
        cancelLongPress()
        endDial()
        if dragHeld {
            dragHeld = false
            send?(.mouseButton(Button.left, down: false))
        }
        scrollClose(PUNKTFUNK_SCROLL_PHASE_CANCEL)
        sessionActive = false
        scrolling = false
        moved = false
    }

    // MARK: - Per-event work

    /// Two fingers → scroll by the centroid delta as a Touch distance; never move the cursor.
    /// While the pair is undecided the units are provisional (`rollBackProvisional`). Finger
    /// up scrolls up, finger right scrolls right (the wire's positive convention). Inversion
    /// is the connection's outbound seam — nothing here flips a sign.
    private func scrollByCentroid() {
        let n = CGFloat(lastPos.count)
        let cx = lastPos.values.reduce(0) { $0 + $1.x } / n
        let cy = lastPos.values.reduce(0) { $0 + $1.y } / n
        if !scrolling {
            // From the pair's landing, so the first move's travel scrolls too.
            scrolling = true
            scrollAnchor = dial?.anchor ?? CGPoint(x: cx, y: cy)
        }
        scrollCarry.y += (scrollAnchor.y - cy) * Tuning.scrollUnitsPerPt
        scrollCarry.x += (cx - scrollAnchor.x) * Tuning.scrollUnitsPerPt
        scrollAnchor = CGPoint(x: cx, y: cy)
        let dy = Int32(scrollCarry.y) // truncates toward zero → remainder kept with its sign
        let dx = Int32(scrollCarry.x)
        scrollCarry.y -= CGFloat(dy)
        scrollCarry.x -= CGFloat(dx)
        guard dx != 0 || dy != 0 else { return }
        if !scrollLocked, dial?.armed == false {
            provisional.x += dx
            provisional.y += dy
        } else {
            scrollLocked = true
            moved = true
        }
        scrollEmit(axis: 0, delta: dy)
        scrollEmit(axis: 1, delta: dx)
    }

    /// One scroll delta on an axis — the first opens it (`Begin`), later ones `Update`.
    private func scrollEmit(axis: UInt32, delta: Int32) {
        guard delta != 0 else { return }
        let opening = axis == 0 ? !scrollOpen.v : !scrollOpen.h
        if axis == 0 { scrollOpen.v = true } else { scrollOpen.h = true }
        send?(.normalizedScroll(
            delta, axis: axis, source: PUNKTFUNK_SCROLL_SOURCE_TOUCH,
            phase: opening ? PUNKTFUNK_SCROLL_PHASE_BEGIN : PUNKTFUNK_SCROLL_PHASE_UPDATE))
    }

    /// Every open axis stops at zero distance — `End` when a gesture ran its course,
    /// `Cancel` on a rollback or teardown.
    private func scrollClose(_ phase: PunktfunkScrollPhase) {
        if scrollOpen.v {
            scrollOpen.v = false
            scrollCarry.y = 0
            send?(.normalizedScroll(
                0, axis: 0, source: PUNKTFUNK_SCROLL_SOURCE_TOUCH, phase: phase))
        }
        if scrollOpen.h {
            scrollOpen.h = false
            scrollCarry.x = 0
            send?(.normalizedScroll(
                0, axis: 1, source: PUNKTFUNK_SCROLL_SOURCE_TOUCH, phase: phase))
        }
    }

    /// A committed scroll closes cleanly (`End`); a rolled-back or torn-down one cancels.
    private func endScroll() { scrollClose(PUNKTFUNK_SCROLL_PHASE_END) }

    /// The undecided pair became a twist or a tap: send back what it scrolled, then cancel.
    private func rollBackProvisional() {
        if provisional.y != 0 { scrollEmit(axis: 0, delta: -provisional.y) }
        if provisional.x != 0 { scrollEmit(axis: 1, delta: -provisional.x) }
        provisional = (0, 0)
        scrollCarry = .zero
        scrollClose(PUNKTFUNK_SCROLL_PHASE_CANCEL)
    }

    /// Three+ fingers → the keyboard swipe, never scroll (the documented vocabulary is
    /// TWO-finger scroll; 3+ only fell into the scroll path as an accident of its old `>= 2`
    /// bound). The centroid is anchored per finger count — real fingers never land or lift in
    /// the same event, so a count change must re-anchor rather than read as travel — and the
    /// gesture fires at most once, when the vertical travel crosses the threshold: up = show
    /// the local soft keyboard, down = dismiss it.
    private func keyboardSwipe(in view: UIView) {
        let n = CGFloat(lastPos.count)
        let cx = lastPos.values.reduce(0) { $0 + $1.x } / n
        let cy = lastPos.values.reduce(0) { $0 + $1.y } / n
        if lastPos.count != kbCount {
            kbCount = lastPos.count
            kbAnchor = CGPoint(x: cx, y: cy)
        } else {
            let dy = cy - kbAnchor.y
            // Real centroid travel disqualifies the tap classification in `ended` (else a
            // sub-threshold swipe would still fire the three-finger stats tap).
            if abs(dy) > Tuning.tapSlop || abs(cx - kbAnchor.x) > Tuning.tapSlop { moved = true }
            if !kbFired, abs(dy) >= view.bounds.height * Tuning.keyboardSwipeFraction {
                kbFired = true
                onKeyboardGesture?(dy < 0) // finger up → show, finger down → dismiss
            }
        }
        // Leaving the scroll state stale would read the 3→2 centroid jump as a wheel notch;
        // clearing it makes a return to two fingers re-anchor fresh. Same for the trackpad's
        // tracked finger: its prev position froze while 3+ fingers were down, so dropping
        // straight back to one finger must re-anchor (zero delta), not replay the whole
        // 3-finger phase as one cursor jump.
        scrolling = false
        trackKey = nil
    }

    /// One finger (and the gesture never became a scroll — dropping back from two fingers to
    /// one must not jerk the cursor).
    private func singleFinger(_ touch: UITouch, in view: UIView) {
        let loc = touch.location(in: view)
        if abs(loc.x - startPoint.x) > Tuning.tapSlop || abs(loc.y - startPoint.y) > Tuning.tapSlop {
            moved = true
        }
        guard trackpad else {
            if let h = hostPoint?(loc) { // pointer mode: the cursor follows the finger
                send?(.mouseMoveAbs(x: h.x, y: h.y, surfaceWidth: h.w, surfaceHeight: h.h))
            }
            return
        }
        // Relative: move by the finger delta × (sensitivity × acceleration), carrying the
        // sub-pixel remainder. Re-anchor (zero delta this frame) if the tracked finger
        // changed, so lifting one of several fingers never jumps the cursor.
        let key = ObjectIdentifier(touch)
        if key != trackKey {
            trackKey = key
            prevPoint = loc
            prevTime = touch.timestamp
            return
        }
        // Ballistics in physical pixels so the curve matches the Android tuning exactly.
        #if os(visionOS)
        let scale = view.traitCollection.displayScale
        #else
        let scale = view.window?.screen.scale ?? view.traitCollection.displayScale
        #endif
        let dx = (loc.x - prevPoint.x) * scale
        let dy = (loc.y - prevPoint.y) * scale
        let dtMs = max((touch.timestamp - prevTime) * 1000, 1)
        prevPoint = loc
        prevTime = touch.timestamp
        let gain = Tuning.pointerSens * Tuning.accel(forSpeed: hypot(dx, dy) / dtMs)
        carryX += dx * gain
        carryY += dy * gain
        let outX = Int32(carryX) // truncates toward zero → remainder kept with its sign
        let outY = Int32(carryY)
        if outX != 0 || outY != 0 {
            send?(.mouseMove(dx: outX, dy: outY))
            carryX -= CGFloat(outX)
            carryY -= CGFloat(outY)
        }
    }

    /// Three-finger tap cycles the stats overlay tiers (off → compact → normal → detailed). Same
    /// cycle as Android's triple-tap.
    private static func cycleStats() {
        StatsVerbosity.requestCycle(for: nil)
    }
}
#endif
