// Convenience constructors for the wire input events (field semantics match
// punktfunk_core::input::InputEvent; see punktfunk_core.h).

import Foundation
import PunktfunkCore

public extension PunktfunkInputEvent {
    /// `screen` rides the first pad byte: an absolute event names the screen it lands on, 0 the
    /// first.
    private static func make(
        _ kind: UInt32, code: UInt32, x: Int32, y: Int32, flags: UInt32 = 0, screen: UInt8 = 0
    ) -> PunktfunkInputEvent {
        PunktfunkInputEvent(
            kind: UInt8(kind), _pad: (screen, 0, 0), code: code, x: x, y: y, flags: flags)
    }
    static func mouseMove(dx: Int32, dy: Int32) -> PunktfunkInputEvent {
        make(PUNKTFUNK_INPUT_KIND_MOUSE_MOVE.rawValue, code: 0, x: dx, y: dy)
    }
    /// Absolute cursor position in client-surface pixels of screen `screen`'s picture — the host
    /// places its cursor there (same letterbox mapping and `flags` surface-dims packing as the
    /// touch events). The desktop mouse model and the iPad pointer fallback send it; the surface
    /// dimensions must each fit in 16 bits.
    static func mouseMoveAbs(
        x: Int32, y: Int32, surfaceWidth: UInt32, surfaceHeight: UInt32, screen: UInt8 = 0
    ) -> PunktfunkInputEvent {
        make(
            PUNKTFUNK_INPUT_KIND_MOUSE_MOVE_ABS.rawValue, code: 0, x: x, y: y,
            flags: ((surfaceWidth & 0xFFFF) << 16) | (surfaceHeight & 0xFFFF), screen: screen)
    }
    /// GameStream button ids: 1=left 2=middle 3=right 4=X1 5=X2 (host maps to evdev BTN_*).
    static func mouseButton(_ button: UInt32, down: Bool) -> PunktfunkInputEvent {
        make(
            (down ? PUNKTFUNK_INPUT_KIND_MOUSE_BUTTON_DOWN : PUNKTFUNK_INPUT_KIND_MOUSE_BUTTON_UP).rawValue,
            code: button, x: 0, y: 0)
    }
    /// `vk` is a Windows virtual-key code (the host's vk_to_evdev table consumes these).
    static func key(_ vk: UInt32, down: Bool) -> PunktfunkInputEvent {
        make((down ? PUNKTFUNK_INPUT_KIND_KEY_DOWN : PUNKTFUNK_INPUT_KIND_KEY_UP).rawValue, code: vk, x: 0, y: 0)
    }
    /// One Unicode scalar of committed text. Only toward a host with `HOST_CAP_TEXT_INPUT`.
    static func text(_ scalar: Unicode.Scalar) -> PunktfunkInputEvent {
        make(PUNKTFUNK_INPUT_KIND_TEXT_INPUT.rawValue, code: scalar.value, x: 0, y: 0)
    }
    /// Normalized scroll (`InputKind::Scroll`): `delta` is signed Q24.8 in `source`'s unit —
    /// v120 for `WHEEL`/`UNKNOWN`, DIP for the rest — `axis` 0=vertical/1=horizontal.
    /// A host without `HOST_CAP2_SCROLL` gets one `MouseScroll` conversion at the core's
    /// outbound seam, so every capture path feeds this shape.
    static func normalizedScroll(
        _ delta: Int32, axis: UInt32, source: PunktfunkScrollSource,
        phase: PunktfunkScrollPhase
    ) -> PunktfunkInputEvent {
        make(
            PUNKTFUNK_INPUT_KIND_SCROLL.rawValue,
            code: axis,
            x: delta,
            y: 0,
            flags: UInt32(source.rawValue) | (UInt32(phase.rawValue) << 8))
    }

    // Gamepad (wire contract in punktfunk_core::input::gamepad): one transition per event,
    // `pad` = controller index, accumulated host-side into a virtual Xbox 360 or DualSense
    // pad (the session's negotiated `GamepadType`).

    /// `button` is a GameStream buttonFlags bit (A=0x1000 B=0x2000 X=0x4000 Y=0x8000,
    /// dpad=0x1/2/4/8, start=0x10 back=0x20 LS=0x40 RS=0x80 LB=0x100 RB=0x200 guide=0x400,
    /// touchpad click=0x100000 — DualSense sessions only, the xpad has no such button).
    static func gamepadButton(_ button: UInt32, down: Bool, pad: UInt32 = 0) -> PunktfunkInputEvent {
        make(
            PUNKTFUNK_INPUT_KIND_GAMEPAD_BUTTON.rawValue,
            code: button, x: down ? 1 : 0, y: 0, flags: pad)
    }

    /// Axis ids: 0=LSX 1=LSY 2=RSX 3=RSY (−32768...32767, XInput convention: +y = UP —
    /// `GCControllerDirectionPad.yAxis` already matches, no flip), 4=LT 5=RT (0...255).
    static func gamepadAxis(_ axis: UInt32, value: Int32, pad: UInt32 = 0) -> PunktfunkInputEvent {
        make(PUNKTFUNK_INPUT_KIND_GAMEPAD_AXIS.rawValue, code: axis, x: value, y: 0, flags: pad)
    }

    /// Declare a pad's controller KIND (`InputKind::GamepadArrival`): `pref` is the
    /// `GamepadType` wire byte — `PunktfunkConnection.GamepadType` is the list, never restated
    /// here — and `pad` the wire index. Sent once when a controller slot
    /// opens — BEFORE that pad's first input — so the host builds a matching virtual device and a
    /// session can mix types (pad 0 a DualSense, pad 1 an Xbox pad). The core re-sends it a few
    /// times against datagram loss and folds per-pad state behind it; a host that predates the tag
    /// ignores it and uses the session-default kind from the handshake. Idempotent on the host.
    static func gamepadArrival(pref: UInt32, pad: UInt32) -> PunktfunkInputEvent {
        make(PUNKTFUNK_INPUT_KIND_GAMEPAD_ARRIVAL.rawValue, code: pref, x: 0, y: 0, flags: pad)
    }

    /// A pad disconnected (`InputKind::GamepadRemove`): `flags` = pad index. The client sends the
    /// bare index; the core stamps the per-pad removal seq (`encode_gamepad_remove`) in the shared
    /// snapshot seq space and arms a loss-resistant re-send burst, so the host tears the pad's
    /// virtual device down and no reordered snapshot can resurrect it. A host that predates the tag
    /// ignores it (the pad then lingers until session end — the pre-existing behaviour).
    static func gamepadRemove(pad: UInt32) -> PunktfunkInputEvent {
        make(PUNKTFUNK_INPUT_KIND_GAMEPAD_REMOVE.rawValue, code: 0, x: 0, y: 0, flags: pad)
    }

    // Touch (host-side: libei ei_touchscreen on the virtual output). `id` distinguishes
    // fingers and is reusable after touchUp; coordinates are absolute pixels on the
    // client's touch surface, whose size rides in `flags` so the host can rescale —
    // the surface dimensions must each fit in 16 bits. Built for the iOS variant
    // (UITouch → these); nothing on macOS emits them yet.

    static func touchDown(
        id: UInt32, x: Int32, y: Int32, surfaceWidth: UInt32, surfaceHeight: UInt32
    ) -> PunktfunkInputEvent {
        make(
            PUNKTFUNK_INPUT_KIND_TOUCH_DOWN.rawValue, code: id, x: x, y: y,
            flags: ((surfaceWidth & 0xFFFF) << 16) | (surfaceHeight & 0xFFFF))
    }

    static func touchMove(
        id: UInt32, x: Int32, y: Int32, surfaceWidth: UInt32, surfaceHeight: UInt32
    ) -> PunktfunkInputEvent {
        make(
            PUNKTFUNK_INPUT_KIND_TOUCH_MOVE.rawValue, code: id, x: x, y: y,
            flags: ((surfaceWidth & 0xFFFF) << 16) | (surfaceHeight & 0xFFFF))
    }

    static func touchUp(id: UInt32) -> PunktfunkInputEvent {
        make(PUNKTFUNK_INPUT_KIND_TOUCH_UP.rawValue, code: id, x: 0, y: 0)
    }
}
