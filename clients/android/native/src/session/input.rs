//! Input plane: Kotlin capture → `NativeClient::send_input`.
//!
//! All shims are `&self` on the `Sync` connector (send_input is a non-blocking datagram push), safe
//! from the Kotlin UI thread. NOT android-gated — send_input exists on the host build too, so these
//! compile everywhere (parity with nativeConnect/nativeClose). The wire codes are the GameStream
//! conventions: buttons 1=left/2=middle/3=right/4=X1/5=X2; scroll axis 0=vertical/1=horizontal,
//! signed 120-unit delta, +=up/right; keys are Windows VK (mapped from KEYCODE_* on the Kotlin side).

use jni::errors::LogErrorAndDefault;
use jni::objects::{JByteArray, JByteBuffer, JFloatArray, JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::EnvUnowned;
use punktfunk_core::input::scroll::ScrollEvent;
use punktfunk_core::input::{InputEvent, InputKind, SCROLL_FLAG_PRECISE};
use punktfunk_core::quic::{
    PenSample, PenTool, RichInput, HID_REPORT_MAX, HOST_CAP2_TOUCH, HOST_CAP_PEN,
    HOST_CAP_TEXT_INPUT, PEN_ANGLE_UNKNOWN, PEN_BATCH_MAX, PEN_DISTANCE_UNKNOWN, PEN_TILT_UNKNOWN,
};

use super::{jni_guard, SESSIONS};

/// Retain the keyed session for one non-blocking [`InputEvent`] send.
fn send_event(handle: jlong, kind: InputKind, code: u32, x: i32, y: i32, flags: u32) {
    let Some(h) = SESSIONS.get(handle) else {
        return;
    };
    let _ = h.client.send_input(&InputEvent {
        kind,
        _pad: [0; 3],
        code,
        x,
        y,
        flags,
    });
}

/// `NativeBridge.nativeSendPointerMove(handle, dx, dy)` — relative mouse motion (screen +y down).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPointerMove(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    dx: jint,
    dy: jint,
) {
    jni_guard((), || {
        send_event(handle, InputKind::MouseMove, 0, dx, dy, 0);
    })
}

/// `NativeBridge.nativeSendPointerAbs(handle, x, y, surfaceWidth, surfaceHeight)` — absolute cursor
/// position: the host moves the pointer to `x`/`y` in a `surfaceWidth`×`surfaceHeight` pixel space,
/// normalizing against the size packed into `flags` as `(w << 16) | h` and mapping into the output
/// region (it drops the event if that size is zero). This is the touch "direct pointing" path — the
/// cursor jumps to the finger — and matches the Apple client's absolute touch forwarding.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPointerAbs(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    x: jint,
    y: jint,
    surface_width: jint,
    surface_height: jint,
) {
    jni_guard((), || {
        let w = (surface_width.max(0) as u32) & 0xffff;
        let ht = (surface_height.max(0) as u32) & 0xffff;
        send_event(handle, InputKind::MouseMoveAbs, 0, x, y, (w << 16) | ht);
    })
}

/// `NativeBridge.nativeSendPointerButton(handle, button, down)` — one button transition.
/// `button`: GameStream id (1=left, 2=middle, 3=right, 4=X1, 5=X2). `down`: 1=press, 0=release.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPointerButton(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    button: jint,
    down: jboolean,
) {
    jni_guard((), || {
        let kind = if down {
            InputKind::MouseButtonDown
        } else {
            InputKind::MouseButtonUp
        };
        send_event(handle, kind, button as u32, 0, 0, 0);
    })
}

/// `NativeBridge.nativeSendScroll(handle, axis, delta, precise)` — one scroll step. `axis`:
/// 0=vertical, 1=horizontal. `delta`: signed, WHEEL_DELTA(120)-scaled, +=up/right. `precise`:
/// the delta was MEASURED off a trackpad, so the host travels that distance instead of pricing
/// each detent as a wheel click ([`SCROLL_FLAG_PRECISE`]).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendScroll(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    axis: jint,
    delta: jint,
    precise: jboolean,
) {
    jni_guard((), || {
        let flags = if precise { SCROLL_FLAG_PRECISE } else { 0 };
        send_event(handle, InputKind::MouseScroll, axis as u32, delta, 0, flags);
    })
}

/// `NativeBridge.nativeSendNormalizedScroll(handle, axis, delta, source, phase)` — one normalized
/// scroll event (`InputKind::Scroll`). `delta` is signed Q24.8 in the source's unit (v120 for
/// Wheel/Unknown, DIP for the rest); `source`/`phase` are the wire bytes packed into `flags`.
/// [`ScrollEvent::from_event`] drops a malformed pair — bad axis, a stop carrying distance, a
/// phased wheel — before anything is sent, so Kotlin constants are checked, not trusted.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendNormalizedScroll(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    axis: jint,
    delta: jint,
    source: jint,
    phase: jint,
) {
    jni_guard((), || {
        if !(0..=1).contains(&axis) || !(0..=5).contains(&source) || !(0..=7).contains(&phase) {
            return;
        }
        let ev = InputEvent {
            kind: InputKind::Scroll,
            _pad: [0; 3],
            code: axis as u32,
            x: delta,
            y: 0,
            flags: (source as u32) | ((phase as u32) << 8),
        };
        if ScrollEvent::from_event(&ev).is_none() {
            return;
        }
        let Some(h) = SESSIONS.get(handle) else {
            return;
        };
        let _ = h.client.send_input(&ev);
    })
}

/// `NativeBridge.nativeSetInvertScroll(handle, invert)` — the live natural-scroll toggle,
/// applied once at the core's outbound seam (wheel, continuous and controller scroll alike).
/// `false` when the session is gone.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetInvertScroll(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    invert: jboolean,
) -> jboolean {
    jni_guard(false, || {
        SESSIONS.get(handle).is_some_and(|h| {
            h.client.set_invert_scroll(invert);
            true
        })
    })
}

/// `NativeBridge.nativeSendTouch(handle, id, kind, x, y, surfaceWidth, surfaceHeight)` — one REAL
/// touchscreen transition (`kind`: 0=down 1=move 2=up), for the touch-passthrough input mode. `id`
/// distinguishes fingers (reusable after up); coordinates are pixels on the client's touch
/// surface, whose size rides in `flags` so the host can rescale into the output (identical
/// packing to MouseMoveAbs). On up only the id matters. The host injects a real touch contact
/// (libei touchscreen / wlroots / SendInput).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendTouch(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    id: jint,
    kind: jint,
    x: jint,
    y: jint,
    surface_width: jint,
    surface_height: jint,
) {
    jni_guard((), || {
        let kind = match kind {
            0 => InputKind::TouchDown,
            1 => InputKind::TouchMove,
            _ => InputKind::TouchUp,
        };
        let w = (surface_width.max(0) as u32) & 0xffff;
        let h = (surface_height.max(0) as u32) & 0xffff;
        send_event(handle, kind, id as u32, x, y, (w << 16) | h);
    })
}

/// `NativeBridge.nativeSendKey(handle, vk, down, mods)` — one key transition. `vk`: Windows
/// Virtual-Key code (0 = unmapped → dropped). `down`: 1=press, 0=release. `mods`: VK modifier
/// bitmask (0 for now — the host folds modifiers from the L/R modifier key events themselves).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendKey(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    vk: jint,
    down: jboolean,
    mods: jint,
) {
    jni_guard((), || {
        if vk == 0 {
            return;
        }
        let kind = if down {
            InputKind::KeyDown
        } else {
            InputKind::KeyUp
        };
        send_event(handle, kind, vk as u32, 0, 0, mods as u32);
    })
}

/// `NativeBridge.nativeTextInputSupported(handle)` — whether the host advertised
/// `HOST_CAP_TEXT_INPUT` (its inject backend types committed text), so the Kotlin side can pick
/// the real IME `InputConnection` over the TYPE_NULL raw-key fallback. `0` handle → false.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeTextInputSupported(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(false, || {
        SESSIONS
            .get(handle)
            .is_some_and(|h| h.client.host_caps() & HOST_CAP_TEXT_INPUT != 0)
    })
}

/// `NativeBridge.nativeHostSupportsPen(handle)` — the host advertised `HOST_CAP_PEN`, so the
/// Kotlin side splits stylus pointers out of the touch path onto the pen plane
/// (design/pen-tablet-input.md §7). `0` handle → false.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeHostSupportsPen(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(false, || {
        SESSIONS
            .get(handle)
            .is_some_and(|h| h.client.host_caps() & HOST_CAP_PEN != 0)
    })
}

/// `NativeBridge.nativeHostSupportsTouch(handle)` — the host advertised `HOST_CAP2_TOUCH`, so
/// passthrough contacts land somewhere. Without it Kotlin runs the trackpad model instead and
/// says so (design/touch-client-overlay.md §5.4). `0` handle → false.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeHostSupportsTouch(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(false, || {
        SESSIONS
            .get(handle)
            .is_some_and(|h| h.client.host_caps2() & HOST_CAP2_TOUCH != 0)
    })
}

/// Floats per sample in the `nativeSendPen` flat array.
const PEN_JNI_STRIDE: usize = 10;

/// Sample ceiling per `nativeSendPen` call: over-cap runs are SPLIT into consecutive ≤8-sample
/// `send_pen` batches (the send_pen contract — never truncated), so this only bounds the stack
/// buffer. 64 samples ≈ >250 ms of 240 Hz history = a pathological UI-thread stall.
const PEN_JNI_MAX_SAMPLES: usize = PEN_BATCH_MAX * 8;

/// `NativeBridge.nativeSendPen(handle, samples, count)` — one stylus emit of STATE-FULL
/// samples, `count` × [`PEN_JNI_STRIDE`] floats, oldest first:
/// `[state, tool, x, y, pressure, distance, tilt_deg, azimuth_deg, roll_deg, dt_us]`.
/// `state` = the wire `PEN_*` bits; `tool` 0=pen 1=eraser; `x`/`y`/`pressure`/`distance`
/// normalized 0..1; `distance`/`tilt_deg`/`azimuth_deg`/`roll_deg` < 0 = unknown. Call only
/// against a [`nativeHostSupportsPen`] host; the client heartbeats the last sample ≤100 ms
/// while in range (Kotlin side — see `StylusStream`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPen(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    samples: JFloatArray,
    count: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if handle == 0 || count <= 0 {
            return Ok(());
        }
        let count = (count as usize).min(PEN_JNI_MAX_SAMPLES);
        let mut buf = [0f32; PEN_JNI_MAX_SAMPLES * PEN_JNI_STRIDE];
        let flat = &mut buf[..count * PEN_JNI_STRIDE];
        if samples.get_region(env, 0, flat).is_err() {
            return Ok(()); // short array — a bridge bug, never worth a crash on the input path
        }
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(());
        };
        let mut batch = [PenSample::default(); PEN_BATCH_MAX];
        for run in flat.chunks(PEN_BATCH_MAX * PEN_JNI_STRIDE) {
            let n = run.len() / PEN_JNI_STRIDE;
            for (slot, s) in batch.iter_mut().zip(run.chunks_exact(PEN_JNI_STRIDE)) {
                if !s[2].is_finite() || !s[3].is_finite() {
                    return Ok(()); // never forward a NaN coordinate
                }
                *slot = PenSample {
                    state: s[0] as u8,
                    tool: if s[1] as u8 == 1 {
                        PenTool::Eraser
                    } else {
                        PenTool::Pen
                    },
                    x: s[2].clamp(0.0, 1.0),
                    y: s[3].clamp(0.0, 1.0),
                    pressure: (s[4].clamp(0.0, 1.0) * 65535.0) as u16,
                    distance: if s[5] < 0.0 {
                        PEN_DISTANCE_UNKNOWN
                    } else {
                        (s[5].clamp(0.0, 1.0) * 65534.0) as u16
                    },
                    tilt_deg: if s[6] < 0.0 {
                        PEN_TILT_UNKNOWN
                    } else {
                        (s[6].clamp(0.0, 90.0)) as u8
                    },
                    azimuth_deg: if s[7] < 0.0 {
                        PEN_ANGLE_UNKNOWN
                    } else {
                        (s[7] as u16) % 360
                    },
                    roll_deg: if s[8] < 0.0 {
                        PEN_ANGLE_UNKNOWN
                    } else {
                        (s[8] as u16) % 360
                    },
                    dt_us: s[9].clamp(0.0, 65535.0) as u16,
                };
            }
            let _ = h.client.send_pen(&batch[..n]);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSendText(handle, text)` — committed IME text, one `TextInput` event per
/// Unicode scalar (`code` = the scalar; multi-char commits are consecutive events in order).
/// Control characters are skipped — Enter/Backspace/Tab ride the VK key path. Call only when
/// [`Java_io_unom_punktfunk_kit_NativeBridge_nativeTextInputSupported`] returned true.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendText(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    text: JString,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if handle == 0 {
            return Ok(());
        }
        let Ok(s) = text.try_to_string(env) else {
            return Ok(());
        };
        for ch in s.chars().filter(|c| !c.is_control()) {
            send_event(handle, InputKind::TextInput, ch as u32, 0, 0, 0);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

// ---- Gamepad: Kotlin captures (KeyEvent/MotionEvent) → NativeClient::send_input ---------------
// Multi-pad model: each physical controller is forwarded on its own wire pad index (0..15), carried
// in the low byte of `flags` on every per-pad event — the Kotlin side (`GamepadRouter`) assigns a
// stable lowest-free index per Android device and threads it here. Buttons carry the gamepad::BTN_*
// bit in `code` and pressed/released in `x` (1/0); axes carry the gamepad::AXIS_* id in `code` and
// the value in `x` (sticks i16 −32768..32767, +y = up; triggers 0..255). The host accumulates the
// incremental events per pad into a matching virtual device. The core input task folds these into
// the seq'd GamepadState snapshots (keyed on this same `flags` index) and owns the per-pad seq — so
// the only thing this layer must get right is the index. Wire contract: input.rs::gamepad. A single
// controller lands on index 0, so its wire is byte-identical to the old single-pad path.

/// `NativeBridge.nativeSendGamepadButton(handle, bit, down, pad)` — one gamepad button transition on
/// wire pad index `pad`. `bit`: a `gamepad::BTN_*` bit (e.g. BTN_A = 0x1000). `down`: 1=press,
/// 0=release. `pad`: wire pad index 0..15 (rides `flags`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendGamepadButton(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    bit: jint,
    down: jboolean,
    pad: jint,
) {
    jni_guard((), || {
        send_event(
            handle,
            InputKind::GamepadButton,
            bit as u32,
            i32::from(down),
            0,
            pad as u32,
        );
    })
}

/// `NativeBridge.nativeSendGamepadAxis(handle, axisId, value, pad)` — one gamepad axis update on wire
/// pad index `pad`. `axisId`: a `gamepad::AXIS_*` id (LS_X=0..RT=5). `value`: stick i16
/// (−32768..32767, +y=up) or trigger 0..255. `pad`: wire pad index 0..15 (rides `flags`).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendGamepadAxis(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    axis_id: jint,
    value: jint,
    pad: jint,
) {
    jni_guard((), || {
        send_event(
            handle,
            InputKind::GamepadAxis,
            axis_id as u32,
            value,
            0,
            pad as u32,
        );
    })
}

/// `NativeBridge.nativePadMouseMode(handle, target)` — the controller-mouse mode the wire pads in
/// `target` share: `0` off, `1` touchpad, `2` full. `0` once the session is gone.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePadMouseMode(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    target: jint,
) -> jint {
    jni_guard(0, || {
        SESSIONS.get(handle).map_or(0, |h| {
            jint::from(h.client.pad_mouse_mode(target as u16) as u8)
        })
    })
}

/// `NativeBridge.nativeCyclePadMouse(handle, target)` — step the wire pads in `target` to the next
/// controller-mouse mode (off, touchpad, full) and return it. `-1` when the session is gone or the
/// host did not grant pointer input.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeCyclePadMouse(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    target: jint,
) -> jint {
    jni_guard(-1, || {
        SESSIONS
            .get(handle)
            .and_then(|h| h.client.cycle_pad_mouse(target as u16).ok())
            .map_or(-1, |mode| jint::from(mode as u8))
    })
}

/// `NativeBridge.nativeSendGamepadArrival(handle, pref, pad)` — declare the controller KIND presented
/// on wire pad index `pad` so the host builds a matching virtual device (mixed types — pad 0 a
/// DualSense, pad 1 an Xbox pad). `pref`: the `GamepadPref` wire byte (rides `code`). `pad`: wire pad
/// index 0..15 (rides `flags`). Sent ONCE when a pad opens, BEFORE any of its input; the core re-sends
/// it a few times against datagram loss, and an older host ignores the unknown tag (that pad then uses
/// the session-default kind from the handshake — the pre-existing single-pad behaviour on pad 0).
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendGamepadArrival(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pref: jint,
    pad: jint,
) {
    jni_guard((), || {
        send_event(
            handle,
            InputKind::GamepadArrival,
            pref as u32,
            0,
            0,
            pad as u32,
        );
    })
}

/// `NativeBridge.nativePadMotionReaches(handle, declaredPref)` — whether motion sent for a pad that
/// declared `declaredPref` (the `GamepadPref` wire byte it passed to `nativeSendGamepadArrival`) can
/// actually reach the game, or would be decoded and dropped by a host backend with no motion plane.
///
/// The whole question is answered here rather than in Kotlin so the reasoning lives in exactly one
/// place — [`punktfunk_core::config::pad_motion_reaches`], which carries the argument and the tests.
/// A third transcription of it would be a third thing to get subtly wrong, and every way of getting
/// it wrong is silent: too strict kills a working gyro, too lax keeps ~250 Hz of samples flowing
/// into a host that drops every one.
///
/// A `0` handle answers `true` — "don't suppress" is the safe answer when we cannot tell, matching
/// the `Auto` rule inside the predicate itself.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePadMotionReaches(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    declared_pref: jint,
) -> jboolean {
    jni_guard(false, || {
        let Some(h) = SESSIONS.get(handle) else {
            return true;
        };
        let declared = punktfunk_core::config::GamepadPref::from_u8(
            declared_pref.clamp(0, u8::MAX as jint) as u8,
        );
        punktfunk_core::config::pad_motion_reaches(
            declared,
            h.client.requested_gamepad,
            h.client.resolved_gamepad,
        )
    })
}

/// `NativeBridge.nativeSendGamepadRemove(handle, pad)` — signal that wire pad index `pad` was
/// unplugged so the host tears its virtual device down. `pad` (rides `flags`) is the only field; the
/// core stamps the per-pad seq (in the snapshot seq space, so a reordered snapshot can't resurrect the
/// pad) and arms a re-send burst against datagram loss. An older host ignores the unknown tag.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendGamepadRemove(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jint,
) {
    jni_guard((), || {
        send_event(handle, InputKind::GamepadRemove, 0, 0, 0, pad as u32);
    })
}

/// `NativeBridge.nativeSendPadHidReport(handle, pad, buf, len)` — one raw HID input report from a
/// client-captured controller (the as-is Steam Controller 2 passthrough), forwarded verbatim on
/// the rich-input plane (`RichInput::HidReport`, 0xCC). `buf` is a DIRECT ByteBuffer whose first
/// `len` bytes are the report, id byte first (`0x42`/`0x45`/`0x47` state, `0x43` battery, …);
/// `len` is clamped to the 64-byte wire body. Called from the capture thread at the controller's
/// own report rate (~250–500 Hz) — the direct-buffer read avoids a JNI array copy per report.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPadHidReport(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jint,
    buf: JByteBuffer,
    len: jint,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        if handle == 0 || len <= 0 {
            return Ok(());
        }
        let cap = match env.get_direct_buffer_capacity(&buf) {
            Ok(c) => c,
            Err(_) => return Ok(()),
        };
        let ptr = match env.get_direct_buffer_address(&buf) {
            Ok(p) if !p.is_null() => p,
            _ => return Ok(()),
        };
        let n = (len as usize).min(cap).min(HID_REPORT_MAX);
        let mut data = [0u8; HID_REPORT_MAX];
        // SAFETY: `ptr`/`cap` describe the direct ByteBuffer's backing store, valid for this call;
        // `n` is bounded by both the buffer capacity and the fixed wire body.
        data[..n].copy_from_slice(unsafe { std::slice::from_raw_parts(ptr, n) });
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(());
        };
        let _ = h.client.send_rich_input(RichInput::HidReport {
            pad: (pad as u32 & 0xF) as u8,
            len: n as u8,
            data,
        });
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSetSc2Gate(handle, pad, gate)` — what core holds back of a Steam
/// Controller 2's raw reports on wire pad `pad`: `PUNKTFUNK_SC2_GATE_*` bits, latest wins.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSetSc2Gate(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jint,
    gate: jint,
) {
    jni_guard((), || {
        if let Some(h) = SESSIONS.get(handle) {
            let gate = punktfunk_core::client::Sc2Gate::from_bits(gate as u32);
            h.client.set_sc2_gate((pad as u32 & 0xF) as u8, gate);
        }
    })
}

/// `NativeBridge.nativeSc2IdentityRequest(puck, index): ByteArray?` — the `index`th feature
/// query a Steam Controller 2's identity is read with; null past the last.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSc2IdentityRequest<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    puck: jboolean,
    index: jint,
) -> JByteArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JByteArray<'local>> {
        let nth = usize::try_from(index).ok();
        match nth.and_then(|i| punktfunk_core::client::sc2::identity_requests(puck).nth(i)) {
            Some(request) => env.byte_array_from_slice(request),
            None => Ok(JByteArray::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSendPadIdentity(handle, pad, serial, replies)` — a captured Steam
/// Controller 2's identity for wire pad `pad`: its USB serial (empty when unknown) and its replies
/// to the identity queries, packed `[len][request][len][reply]…`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPadIdentity(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jint,
    serial: JString,
    replies: JByteArray,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(());
        };
        let serial = serial.try_to_string(env).unwrap_or_default();
        let replies = env.convert_byte_array(&replies)?;
        let pad = (pad as u32 & 0xF) as u8;
        let id = punktfunk_core::quic::PadIdentity {
            pad,
            serial,
            replies,
            slot: 0,
        };
        if let Err(e) = h.client.send_pad_identity(id) {
            log::warn!("pad identity not sent: {e:#}");
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSendPadTouch(handle, pad, finger, active, x, y)` — one touchpad contact
/// from a client-captured controller (the Sony USB capture), forwarded on the rich-input plane
/// (`RichInput::Touchpad`, 0xCC). `finger`: contact slot 0/1; `x`/`y`: normalized 0..=65535 in
/// SCREEN convention (+y down — the wire's fixed meaning); `active` 0 lifts the finger. The
/// host's DualSense-family backends scale onto the virtual pad's touch surface. On-change only —
/// the capture diffs, the host holds per-slot state.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPadTouch(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jint,
    finger: jint,
    active: jboolean,
    x: jint,
    y: jint,
) {
    jni_guard((), || {
        let Some(h) = SESSIONS.get(handle) else {
            return;
        };
        let _ = h.client.send_rich_input(RichInput::Touchpad {
            pad: (pad as u32 & 0xF) as u8,
            finger: (finger as u32 & 0x1) as u8,
            active,
            x: (x as i64).clamp(0, 65535) as u16,
            y: (y as i64).clamp(0, 65535) as u16,
        });
    })
}

/// `NativeBridge.nativeSendPadMotion(handle, pad, gp, gy, gr, ax, ay, az)` — one motion sample
/// from a client-captured controller (`RichInput::Motion`, 0xCC): gyro pitch/yaw/roll + accel,
/// raw signed-16 values in the pad's own units, passed straight into the host's virtual
/// DualSense report (the wire is a unit passthrough). Called from the capture thread at the
/// controller's report rate.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPadMotion(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jint,
    gyro_pitch: jint,
    gyro_yaw: jint,
    gyro_roll: jint,
    accel_x: jint,
    accel_y: jint,
    accel_z: jint,
) {
    jni_guard((), || {
        let Some(h) = SESSIONS.get(handle) else {
            return;
        };
        let c = |v: jint| (v as i64).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16;
        let _ = h.client.send_rich_input(RichInput::Motion {
            pad: (pad as u32 & 0xF) as u8,
            gyro: [c(gyro_pitch), c(gyro_yaw), c(gyro_roll)],
            accel: [c(accel_x), c(accel_y), c(accel_z)],
        });
    })
}
