// The gamepad wire contract shared by capture (GamepadCapture), feedback (GamepadFeedback),
// and the tests — the pad count, button bits, axis ids, and the touchpad/motion unit conversions.

import Foundation
import PunktfunkCore

/// The gamepad wire contract: `punktfunk_core::input::gamepad`, read from the C header.
public enum GamepadWire {
    /// Gamepads addressable on the wire — the pad index rides the low byte of `flags` on every
    /// per-pad event, 0...15.
    public static let maxPads = Int(PUNKTFUNK_MAX_PADS)

    public static let dpadUp = UInt32(PUNKTFUNK_BTN_DPAD_UP)
    public static let dpadDown = UInt32(PUNKTFUNK_BTN_DPAD_DOWN)
    public static let dpadLeft = UInt32(PUNKTFUNK_BTN_DPAD_LEFT)
    public static let dpadRight = UInt32(PUNKTFUNK_BTN_DPAD_RIGHT)
    public static let start = UInt32(PUNKTFUNK_BTN_START)
    public static let back = UInt32(PUNKTFUNK_BTN_BACK)
    public static let leftStickClick = UInt32(PUNKTFUNK_BTN_LS_CLICK)
    public static let rightStickClick = UInt32(PUNKTFUNK_BTN_RS_CLICK)
    public static let leftShoulder = UInt32(PUNKTFUNK_BTN_LB)
    public static let rightShoulder = UInt32(PUNKTFUNK_BTN_RB)
    public static let guide = UInt32(PUNKTFUNK_BTN_GUIDE)
    public static let a = UInt32(PUNKTFUNK_BTN_A)
    public static let b = UInt32(PUNKTFUNK_BTN_B)
    public static let x = UInt32(PUNKTFUNK_BTN_X)
    public static let y = UInt32(PUNKTFUNK_BTN_Y)
    /// DualSense touchpad click (Moonlight's extended-button bit position).
    public static let touchpadClick = UInt32(PUNKTFUNK_BTN_TOUCHPAD)
    /// Misc / capture button — Xbox-Series Share, DualSense Create, Steam-Deck quick-access. The
    /// host routes it to the DualSense mute / Steam quick-access menu; a plain virtual xpad has no
    /// such button.
    public static let misc1 = UInt32(PUNKTFUNK_BTN_MISC1)
    /// What a synthetic Quick Access tap presses, in order. A pad with the button takes
    /// `misc1`. Every other pad takes the chord Steam reads there: Guide held, then A.
    /// Mirrors punktfunk-core's `quick_access_presses`.
    public static func quickAccessPresses(hasButton: Bool) -> [UInt32] {
        hasButton ? [misc1] : [guide, a]
    }

    /// Back-grip paddles (Xbox Elite P1–P4 / DualSense Edge / Steam-Deck L4-L5-R4-R5, as
    /// R4/L4/R5/L5). `GamepadCapture.buttonMask` does not read them yet — the GameController
    /// `paddleButton1..4` ↔ BTN_PADDLE physical correspondence needs confirming on a real Elite
    /// pad first (see the gamepad-review-cleanup plan, G22), so they are intentionally absent
    /// from `allButtons` until that forwarding lands.
    public static let paddle1 = UInt32(PUNKTFUNK_BTN_PADDLE1)
    public static let paddle2 = UInt32(PUNKTFUNK_BTN_PADDLE2)
    public static let paddle3 = UInt32(PUNKTFUNK_BTN_PADDLE3)
    public static let paddle4 = UInt32(PUNKTFUNK_BTN_PADDLE4)

    /// Every button a capture can set — walked by `GamepadCapture.feed`'s transition diff and by
    /// each capture's `flush` on release. Paddles stay out until their capture lands (see above).
    public static let allButtons: [UInt32] = [
        dpadUp, dpadDown, dpadLeft, dpadRight, start, back,
        leftStickClick, rightStickClick, leftShoulder, rightShoulder, guide,
        a, b, x, y, touchpadClick, misc1,
    ]

    public static let axisLSX = UInt32(PUNKTFUNK_AXIS_LS_X)
    public static let axisLSY = UInt32(PUNKTFUNK_AXIS_LS_Y)
    public static let axisRSX = UInt32(PUNKTFUNK_AXIS_RS_X)
    public static let axisRSY = UInt32(PUNKTFUNK_AXIS_RS_Y)
    public static let axisLT = UInt32(PUNKTFUNK_AXIS_LT)
    public static let axisRT = UInt32(PUNKTFUNK_AXIS_RT)

    /// Raw DualSense gyro units per rad/s: hid-playstation's calibration over the host's
    /// fixed blob resolves to 20 LSB per deg/s.
    public static let gyroLSBPerRadS: Float = 20 * 180 / .pi
    /// Raw DualSense accelerometer units per g (same derivation).
    public static let accelLSBPerG: Float = 10_000

    /// GC touchpad coordinates (±1, +y up) → wire (0...65535, origin top-left, +y down).
    public static func touchpad(x: Float, y: Float) -> (x: UInt16, y: UInt16) {
        let wx = ((x.clamped(to: -1...1) + 1) / 2 * 65535).rounded()
        let wy = ((1 - y.clamped(to: -1...1)) / 2 * 65535).rounded()
        return (UInt16(wx), UInt16(wy))
    }

    /// Scale + clamp one motion component into the raw signed-16 sensor domain.
    public static func motionRaw(_ value: Float, scale: Float) -> Int16 {
        Int16((value * scale).rounded().clamped(to: Float(Int16.min)...Float(Int16.max)))
    }

    /// GameController's motion frame → the DualSense report frame the wire is defined in.
    ///
    /// The wire is a unit passthrough: the host writes these three components, in order, into the
    /// virtual DualSense's report bytes 16../22.. — the same slots a real pad fills. So the frame
    /// the wire is defined in is the pad's OWN report frame, and a client that forwards its
    /// platform's axes unconverted is simply speaking a different language.
    ///
    /// Both frames were measured on 2026-08-07 from ONE physical DualSense on one desk — the pad
    /// read twice, over raw HID and through GameController:
    ///
    ///   DualSense report frame: (Right, Up, Backward) — axis 0 carries pitch, 1 yaw, 2 roll
    ///   GameController frame:   (Right, Forward, Up)
    ///
    /// Matching them up: Right is already slot 0; Up is GC's z, so it moves to slot 1; and slot 2
    /// wants Backward, which is GC's y negated. Hence `(x, z, -y)`.
    ///
    /// Applied to gyro AND acceleration, because it is a change of basis and both are expressed in
    /// that basis. The negation `forwardMotion` already does for acceleration is a separate matter
    /// — that one converts Apple's gravity-VECTOR convention into the proper acceleration a real
    /// pad reports, and it composes with this rather than replacing it.
    public static func appleMotionToWire(_ v: (Float, Float, Float)) -> (Float, Float, Float) {
        (v.0, v.2, -v.1)
    }
}
