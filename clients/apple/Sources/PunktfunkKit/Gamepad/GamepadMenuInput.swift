// Left-stick/dpad menu navigation outside a stream: the console, and the trust card.
//
// Polls the active controller at 60 Hz rather than installing `valueChangedHandler`/
// `pressedChangedHandler` callbacks — mirroring `ControllerTestView`'s "Input" card (see its own
// comment: "Poll the live controller ... — no handlers installed"), the one thing in this codebase
// already confirmed on real hardware to read a controller reliably outside a streaming session. Two
// earlier versions of this class both installed handlers directly (first reading the dpad's combined
// `.xAxis`/`.yAxis`, then its discrete `.isPressed` states, matching `GamepadCapture`'s pattern) and
// neither one's callbacks fired on-device even though the SAME controller's input showed up correctly
// in `ControllerTestView`'s poll-based readout — so polling isn't just a style choice here, it's the
// only approach confirmed to actually work outside a stream. Being read-only, it also can't conflict
// with `GamepadCapture` installing its own handlers once a stream starts — there's nothing to hand
// off or race over.
//
// The button set mirrors a console launcher: A confirms, B backs out, Y is a screen's secondary
// action, X a tertiary one, and the shoulders (L1/R1) are optional fast "jump" steps. Directional
// moves auto-repeat on a held stick/dpad after an initial delay; every button is edge-triggered
// (fires once per press).
//
// A poll reads whichever source has a pad — GameController first, else the Steam Controller 2
// `Sc2MenuPad` reads directly on iOS and visionOS, where GameController surfaces no such
// device. Both arrive as a `Snapshot`, so the dead zone, hysteresis and repeat rules below are
// written once.

import Foundation
import GameController

@MainActor
public final class GamepadMenuInput {
    public enum Direction: Equatable, Sendable {
        case up, down, left, right
    }

    /// One poll's worth of pad, from either source. Only what a menu navigates with: the left
    /// stick, the dpad, and the six buttons the screens bind.
    struct Snapshot: Equatable {
        var confirm = false // A
        var back = false // B
        var secondary = false // Y
        var tertiary = false // X
        var leftShoulder = false
        var rightShoulder = false
        /// Left stick, −1…1, +y up (GameController's convention, and the SC2's).
        var x: Float = 0
        var y: Float = 0
        var up = false
        var down = false
        var left = false
        var right = false

        init(_ gamepad: GCExtendedGamepad) {
            confirm = gamepad.buttonA.isPressed
            back = gamepad.buttonB.isPressed
            secondary = gamepad.buttonY.isPressed
            tertiary = gamepad.buttonX.isPressed
            leftShoulder = gamepad.leftShoulder.isPressed
            rightShoulder = gamepad.rightShoulder.isPressed
            x = gamepad.leftThumbstick.xAxis.value
            y = gamepad.leftThumbstick.yAxis.value
            // Discrete `.isPressed`, never the dpad's combined axis — the first version of this
            // class read the axis and silently never registered a press on-device.
            up = gamepad.dpad.up.isPressed
            down = gamepad.dpad.down.isPressed
            left = gamepad.dpad.left.isPressed
            right = gamepad.dpad.right.isPressed
        }

        /// A parsed SC2 state report. The sticks are the device's raw i16, scaled to the same
        /// −1…1 the GameController path delivers.
        init(sc2 state: Sc2Device.State) {
            func held(_ bit: UInt32) -> Bool { state.buttons & bit != 0 }
            confirm = held(Sc2Device.btnA)
            back = held(Sc2Device.btnB)
            secondary = held(Sc2Device.btnY)
            tertiary = held(Sc2Device.btnX)
            leftShoulder = held(Sc2Device.btnLB)
            rightShoulder = held(Sc2Device.btnRB)
            x = Float(state.lsX) / 32767
            y = Float(state.lsY) / 32767
            up = held(Sc2Device.btnDpadUp)
            down = held(Sc2Device.btnDpadDown)
            left = held(Sc2Device.btnDpadLeft)
            right = held(Sc2Device.btnDpadRight)
        }
    }

    private let manager: GamepadManager
    private var pollTimer: Timer?
    private var isActive = false
    /// Seed the pressed-state trackers from the LIVE controller on the first poll after a
    /// (re)start, firing nothing. Screens hand the controller off (a keyboard closes, a cover
    /// dismisses) while the user is still holding the very button that triggered the handoff —
    /// without this, the next screen's first poll would read that held button as a fresh edge
    /// and act on the same press twice (e.g. the B that closed the keyboard also backing out
    /// of the screen underneath).
    private var needsSnapshot = false
    private var currentDirection: Direction?
    private var repeatTimer: Timer?
    private var wasConfirmPressed = false
    private var wasSecondaryPressed = false
    private var wasTertiaryPressed = false
    private var wasBackPressed = false
    private var wasLeftShoulderPressed = false
    private var wasRightShoulderPressed = false

    /// Discrete directional move — already debounced (fires once on a fresh press, then repeats
    /// on a hold after an initial delay, like a standard menu).
    public var onMove: ((Direction) -> Void)?
    /// Button A (or equivalent primary action) — edge-triggered, fires once per press.
    public var onConfirm: (() -> Void)?
    /// Button Y (or equivalent secondary action, e.g. "open library") — edge-triggered.
    public var onSecondary: (() -> Void)?
    /// Button X (or equivalent tertiary action, e.g. "settings" / "delete") — edge-triggered.
    public var onTertiary: (() -> Void)?
    /// Button B (or equivalent back/dismiss) — edge-triggered.
    public var onBack: (() -> Void)?
    /// Shoulder buttons (L1 `false` / R1 `true`) — edge-triggered fast-jump steps, optional per
    /// screen. Unset ⇒ the shoulders do nothing.
    public var onShoulder: ((Bool) -> Void)?

    /// Dead zone, per stick axis: an axis at or below this in size reads as neutral.
    private let deadzone: Float = 0.5
    static let initialRepeatDelay: TimeInterval = 0.38
    static let repeatInterval: TimeInterval = 0.16
    private let pollInterval: TimeInterval = 1.0 / 60.0

    public init(manager: GamepadManager) {
        self.manager = manager
    }

    public func start() {
        guard !isActive else { return }
        isActive = true
        needsSnapshot = true
        let timer = Timer(timeInterval: pollInterval, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.poll() }
        }
        RunLoop.main.add(timer, forMode: .common)
        pollTimer = timer
    }

    public func stop() {
        isActive = false
        pollTimer?.invalidate()
        pollTimer = nil
        repeatTimer?.invalidate()
        repeatTimer = nil
        currentDirection = nil
        wasConfirmPressed = false
        wasSecondaryPressed = false
        wasTertiaryPressed = false
        wasBackPressed = false
        wasLeftShoulderPressed = false
        wasRightShoulderPressed = false
    }

    /// Reads the live pad fresh every tick (no persistent binding to a specific controller
    /// needed) — a disconnect/reconnect or a controller switch is just picked up on the next poll.
    private func poll() {
        guard isActive else { return }
        // A stream in another window owns the pad. What is held when it hands back is adopted.
        guard !manager.streamOwnsPads else {
            needsSnapshot = true
            updateDirection(nil)
            return
        }
        guard let pad = livePad() else {
            // The controller went away mid-press. Returning here would leave a held direction's
            // repeat timer running forever, walking the list to its end and bumping there every
            // repeat interval — visible whenever the console UI stays up without a pad.
            updateDirection(nil)
            return
        }

        if needsSnapshot {
            // Adopt whatever is held right now without firing (see `needsSnapshot`): a button
            // must be RELEASED after a handoff before it can act here, and a held direction only
            // keeps moving once it changes or re-engages.
            needsSnapshot = false
            wasConfirmPressed = pad.confirm
            wasSecondaryPressed = pad.secondary
            wasTertiaryPressed = pad.tertiary
            wasBackPressed = pad.back
            wasLeftShoulderPressed = pad.leftShoulder
            wasRightShoulderPressed = pad.rightShoulder
            currentDirection = directionFrom(pad)
            return
        }

        edge(pad.confirm, &wasConfirmPressed) { onConfirm?() }
        edge(pad.secondary, &wasSecondaryPressed) { onSecondary?() }
        edge(pad.tertiary, &wasTertiaryPressed) { onTertiary?() }
        edge(pad.back, &wasBackPressed) { onBack?() }
        edge(pad.leftShoulder, &wasLeftShoulderPressed) { onShoulder?(false) }
        edge(pad.rightShoulder, &wasRightShoulderPressed) { onShoulder?(true) }

        updateDirection(directionFrom(pad))
    }

    /// The pad this tick, or nil when none is attached. A GameController one wins: on macOS an
    /// SC2 is one of those, and everywhere else the fallback is the pad GameController cannot
    /// see at all, so the two sources can never be the same device twice.
    private func livePad() -> Snapshot? {
        if let gamepad = manager.active?.controller.extendedGamepad { return Snapshot(gamepad) }
        if let sc2 = manager.sc2MenuState { return Snapshot(sc2: sc2) }
        return nil
    }

    /// Fire `action` on the rising edge of `pressed`, tracking the last state in `was`.
    private func edge(_ pressed: Bool, _ was: inout Bool, _ action: () -> Void) {
        if pressed, !was { action() }
        was = pressed
    }

    /// The current requested direction: the left stick is the primary/natural input; the dpad is
    /// an alternative.
    private func directionFrom(_ pad: Snapshot) -> Direction? {
        let x = pad.x
        let y = pad.y
        // HYSTERESIS: an engaged direction stays engaged until ITS OWN input releases, even if the
        // other axis is momentarily larger. Without this a single flick to the right passed through
        // samples where |y| > |x| on the way out of the dead zone and read as UP, then RIGHT — two
        // moves for one gesture. Invisible on the carousels (their vertical axis is inert or a
        // menu), a "random jump" on any 2-D field such as the library grid.
        if let current = currentDirection {
            let held: Bool
            switch current {
            case .left: held = (x < -deadzone && abs(x) >= abs(y) * 0.5) || pad.left
            case .right: held = (x > deadzone && abs(x) >= abs(y) * 0.5) || pad.right
            case .up: held = (y > deadzone && abs(y) >= abs(x) * 0.5) || pad.up
            case .down: held = (y < -deadzone && abs(y) >= abs(x) * 0.5) || pad.down
            }
            if held { return current }
        }
        // Horizontal wins an exact |x| == |y| diagonal tie (>=), matching the SDL core and Android
        // nav so a perfect 45° push resolves to the same direction on every client.
        if abs(x) >= abs(y), abs(x) > deadzone {
            return x > 0 ? .right : .left
        } else if abs(y) > deadzone {
            return y > 0 ? .up : .down
        }
        if pad.left { return .left }
        if pad.right { return .right }
        if pad.up { return .up }
        if pad.down { return .down }
        return nil
    }

    private func updateDirection(_ direction: Direction?) {
        guard direction != currentDirection else { return }
        repeatTimer?.invalidate()
        repeatTimer = nil
        currentDirection = direction
        guard let direction else { return }
        onMove?(direction)
        // First repeat after a longer delay (so a quick tap doesn't double-move), then steady.
        let timer = Timer(timeInterval: Self.initialRepeatDelay, repeats: false) { [weak self] _ in
            Task { @MainActor in
                // Re-checked after the hop: a `stop()` landing in this window has already
                // invalidated the one-shot, and without this it would install a repeat on a
                // stopped poller — which then drives a screen that is no longer on top.
                guard let self, self.isActive else { return }
                self.repeatTimer?.invalidate()
                let repeating = Timer(timeInterval: Self.repeatInterval, repeats: true) { [weak self] _ in
                    Task { @MainActor in self?.onMove?(direction) }
                }
                RunLoop.main.add(repeating, forMode: .common)
                self.repeatTimer = repeating
            }
        }
        RunLoop.main.add(timer, forMode: .common)
        repeatTimer = timer
    }
}
