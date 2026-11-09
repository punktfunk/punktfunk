//! Input plane: Kotlin capture → `NativeClient::send_input`.
//!
//! All shims are `&self` on the `Sync` connector (send_input is a non-blocking datagram push), safe
//! from the Kotlin UI thread. NOT android-gated — send_input exists on the host build too, so these
//! compile everywhere (parity with nativeConnect/nativeClose). The wire codes are the GameStream
//! conventions: buttons 1=left/2=middle/3=right/4=X1/5=X2; scroll axis 0=vertical/1=horizontal,
//! +=up/right; keys are Windows VK (mapped from KEYCODE_* on the Kotlin side).

use jni::errors::LogErrorAndDefault;
use jni::objects::{JByteArray, JByteBuffer, JFloatArray, JObject, JString};
use jni::sys::{jboolean, jint, jlong};
use jni::EnvUnowned;
use punktfunk_core::input::scroll::ScrollEvent;
use punktfunk_core::input::{InputEvent, InputKind};
use punktfunk_core::quic::{
    PenSample, PenTool, RichInput, HID_REPORT_MAX, HOST_CAP2_TOUCH, HOST_CAP_PEN,
    HOST_CAP_TEXT_INPUT, PEN_ANGLE_UNKNOWN, PEN_BATCH_MAX, PEN_DISTANCE_UNKNOWN, PEN_TILT_UNKNOWN,
};

use super::{jni_guard, SESSIONS};

/// `(width << 16) | height` for the `flags` of an absolute pointer or touch event. Each side is
/// floored at zero and kept to 16 bits; the host drops the event when a side is zero.
fn surface_flags(width: jint, height: jint) -> u32 {
    let w = (width.max(0) as u32) & 0xffff;
    let h = (height.max(0) as u32) & 0xffff;
    (w << 16) | h
}

/// The wire pad index of Kotlin's pad number: its low four bits, 0..=15.
fn wire_pad(pad: jint) -> u8 {
    (pad as u32 & 0xF) as u8
}

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
        let flags = surface_flags(surface_width, surface_height);
        send_event(handle, InputKind::MouseMoveAbs, 0, x, y, flags);
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

/// The scroll event for Kotlin's constants, or `None` when the wire refuses the pair. `axis`,
/// `source` and `phase` must be in range before [`ScrollEvent::from_event`] judges the rest: a
/// stop carrying distance, a phased wheel. Kotlin's constants are checked, not trusted.
fn scroll_event(axis: jint, delta: jint, source: jint, phase: jint) -> Option<InputEvent> {
    if !(0..=1).contains(&axis) || !(0..=5).contains(&source) || !(0..=7).contains(&phase) {
        return None;
    }
    let ev = InputEvent {
        kind: InputKind::Scroll,
        _pad: [0; 3],
        code: axis as u32,
        x: delta,
        y: 0,
        flags: (source as u32) | ((phase as u32) << 8),
    };
    ScrollEvent::from_event(&ev).map(|_| ev)
}

/// `NativeBridge.nativeSendNormalizedScroll(handle, axis, delta, source, phase)` — one normalized
/// scroll event (`InputKind::Scroll`). `delta` is signed Q24.8 in the source's unit (v120 for
/// Wheel/Unknown, DIP for the rest); `source`/`phase` are the wire bytes packed into `flags`.
/// A pair [`scroll_event`] refuses is dropped before the session is looked up.
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
        let Some(ev) = scroll_event(axis, delta, source, phase) else {
            return;
        };
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

/// `NativeBridge.nativePathChanged(handle)` — the default network or its addresses changed. The
/// core shows the host where this device is now, so a roam heals within a round trip. `false`
/// when the session is gone.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePathChanged(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
) -> jboolean {
    jni_guard(false, || {
        SESSIONS.get(handle).is_some_and(|h| {
            h.client.path_changed();
            true
        })
    })
}

/// The wire kind of Kotlin's touch `kind`: `0` down, `1` move, anything else up.
fn touch_kind(kind: jint) -> InputKind {
    match kind {
        0 => InputKind::TouchDown,
        1 => InputKind::TouchMove,
        _ => InputKind::TouchUp,
    }
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
        let flags = surface_flags(surface_width, surface_height);
        send_event(handle, touch_kind(kind), id as u32, x, y, flags);
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

/// How many samples to read for Kotlin's `count`: none unless positive, at most
/// [`PEN_JNI_MAX_SAMPLES`].
fn pen_count(count: jint) -> usize {
    usize::try_from(count).unwrap_or(0).min(PEN_JNI_MAX_SAMPLES)
}

/// One sample from [`PEN_JNI_STRIDE`] floats. `None` for a non-finite `x`/`y`, which is never
/// forwarded, or a slice of another length. A negative `distance`, `tilt_deg`, `azimuth_deg` or
/// `roll_deg` is the wire's "unknown".
fn pen_sample(s: &[f32]) -> Option<PenSample> {
    let Ok(&[state, tool, x, y, pressure, distance, tilt, azimuth, roll, dt_us]) =
        <&[f32; PEN_JNI_STRIDE]>::try_from(s)
    else {
        return None;
    };
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    Some(PenSample {
        state: state as u8,
        tool: if tool as u8 == 1 {
            PenTool::Eraser
        } else {
            PenTool::Pen
        },
        x: x.clamp(0.0, 1.0),
        y: y.clamp(0.0, 1.0),
        pressure: (pressure.clamp(0.0, 1.0) * 65535.0) as u16,
        distance: if distance < 0.0 {
            PEN_DISTANCE_UNKNOWN
        } else {
            (distance.clamp(0.0, 1.0) * 65534.0) as u16
        },
        tilt_deg: if tilt < 0.0 {
            PEN_TILT_UNKNOWN
        } else {
            (tilt.clamp(0.0, 90.0)) as u8
        },
        azimuth_deg: if azimuth < 0.0 {
            PEN_ANGLE_UNKNOWN
        } else {
            (azimuth as u16) % 360
        },
        roll_deg: if roll < 0.0 {
            PEN_ANGLE_UNKNOWN
        } else {
            (roll as u16) % 360
        },
        dt_us: dt_us.clamp(0.0, 65535.0) as u16,
    })
}

/// The `send_pen` batches of `flat`, oldest first: [`PEN_BATCH_MAX`] samples each, the last
/// possibly fewer, as `(samples, len)`. The run ends before the batch holding a sample
/// [`pen_sample`] refuses; the batches ahead of it still go.
fn pen_batches(flat: &[f32]) -> impl Iterator<Item = ([PenSample; PEN_BATCH_MAX], usize)> + '_ {
    flat.chunks(PEN_BATCH_MAX * PEN_JNI_STRIDE)
        .map_while(|run| {
            let mut batch = [PenSample::default(); PEN_BATCH_MAX];
            let mut len = 0;
            for (slot, s) in batch.iter_mut().zip(run.chunks_exact(PEN_JNI_STRIDE)) {
                *slot = pen_sample(s)?;
                len += 1;
            }
            Some((batch, len))
        })
}

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
        let count = pen_count(count);
        if handle == 0 || count == 0 {
            return Ok(());
        }
        let mut buf = [0f32; PEN_JNI_MAX_SAMPLES * PEN_JNI_STRIDE];
        let flat = &mut buf[..count * PEN_JNI_STRIDE];
        if samples.get_region(env, 0, flat).is_err() {
            return Ok(()); // short array — a bridge bug, never worth a crash on the input path
        }
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(());
        };
        for (batch, len) in pen_batches(flat) {
            let _ = h.client.send_pen(&batch[..len]);
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// The Unicode scalars of `text` worth typing. Control characters are skipped: Enter, Backspace
/// and Tab ride the VK key path.
fn text_scalars(text: &str) -> impl Iterator<Item = u32> + '_ {
    text.chars().filter(|c| !c.is_control()).map(|c| c as u32)
}

/// `NativeBridge.nativeSendText(handle, text)` — committed IME text, one `TextInput` event per
/// Unicode scalar (`code` = the scalar; multi-char commits are consecutive events in order),
/// control characters left out ([`text_scalars`]). Call only when
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
        for code in text_scalars(&s) {
            send_event(handle, InputKind::TextInput, code, 0, 0, 0);
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

/// `NativeBridge.nativePadHasQuickAccess(handle, declaredPref)` — whether the host's device for a
/// pad that declared `declaredPref` has a Quick Access button, so a synthetic Quick Access tap
/// presses `MISC1` there and Steam's chord everywhere else. A negative `declaredPref` is a pad
/// that never declared: the session default answers for it.
///
/// Answered here for the reason `nativePadMotionReaches` is:
/// [`punktfunk_core::config::pad_has_quick_access`] carries the rule and its tests. A `0`
/// handle answers `false`, which sends the chord: every pad Steam reads takes it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativePadHasQuickAccess(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    declared_pref: jint,
) -> jboolean {
    jni_guard(false, || {
        let Some(h) = SESSIONS.get(handle) else {
            return false;
        };
        let declared = u8::try_from(declared_pref).map_or(
            h.client.requested_gamepad,
            punktfunk_core::config::GamepadPref::from_u8,
        );
        punktfunk_core::config::pad_has_quick_access(
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

/// How many bytes of a HID report to forward: Kotlin's `len` bounded by the buffer's capacity
/// and the wire body, none unless positive.
fn hid_report_len(len: jint, capacity: usize) -> usize {
    usize::try_from(len)
        .unwrap_or(0)
        .min(capacity)
        .min(HID_REPORT_MAX)
}

/// `NativeBridge.nativeSendPadHidReport(handle, pad, buf, len)` — one raw HID input report from a
/// client-captured controller (the as-is Steam Controller 2 passthrough), forwarded verbatim on
/// the rich-input plane (`RichInput::HidReport`, 0xCC). `buf` is a DIRECT ByteBuffer whose first
/// `len` bytes are the report, id byte first (`0x42`/`0x45`/`0x47` state, `0x43` battery, …);
/// `len` is bounded by [`hid_report_len`]. Called from the capture thread at the controller's
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
        let n = hid_report_len(len, cap);
        let mut data = [0u8; HID_REPORT_MAX];
        // SAFETY: `ptr`/`cap` describe the direct ByteBuffer's backing store, valid for this call;
        // `n` is bounded by both the buffer capacity and the fixed wire body.
        data[..n].copy_from_slice(unsafe { std::slice::from_raw_parts(ptr, n) });
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(());
        };
        let _ = h.client.send_rich_input(RichInput::HidReport {
            pad: wire_pad(pad),
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
            h.client.set_sc2_gate(wire_pad(pad), gate);
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

/// `NativeBridge.nativeSc1IdentityRequest(index): ByteArray?` — the `index`th feature query a
/// 2015 Steam Controller's identity is read with; null past the last.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSc1IdentityRequest<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    index: jint,
) -> JByteArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JByteArray<'local>> {
        let nth = usize::try_from(index).ok();
        match nth.and_then(|i| punktfunk_core::client::sc2::classic_identity_requests().nth(i)) {
            Some(request) => env.byte_array_from_slice(request),
            None => Ok(JByteArray::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSc1BleState(): ByteArray` — a fresh state for one 2015 Steam Controller's
/// Bluetooth input, for [`nativeSc1BleInput`].
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSc1BleState<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
) -> JByteArray<'local> {
    use punktfunk_core::client::sc1_ble::CLASSIC_BLE_STATE;
    env.with_env(|env| env.byte_array_from_slice(&[0u8; CLASSIC_BLE_STATE]))
        .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSc1BleInput(state, segment): ByteArray?` — one segment of a 2015 Steam
/// Controller's Bluetooth input, `[0x03, header, 18 bytes]`; the 64-byte wired frame when it
/// completed a state message, else null. `state` is updated in place.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSc1BleInput<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    state: JByteArray<'local>,
    segment: JByteArray<'local>,
) -> JByteArray<'local> {
    use punktfunk_core::client::sc1_ble::{classic_ble_input, CLASSIC_BLE_STATE};
    env.with_env(|env| -> jni::errors::Result<JByteArray<'local>> {
        let mut held = [0i8; CLASSIC_BLE_STATE];
        state.get_region(env, 0, &mut held)?;
        let mut st = held.map(|b| b as u8);
        let seg = env.convert_byte_array(&segment)?;
        let mut out = [0u8; 64];
        let done = classic_ble_input(&mut st, &seg, &mut out);
        state.set_region(env, 0, &st.map(|b| b as i8))?;
        if done {
            env.byte_array_from_slice(&out)
        } else {
            Ok(JByteArray::default())
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSc1BleSegments(frame): ByteArray` — a wired feature frame for a 2015
/// Steam Controller, report id 0 first, as its Bluetooth segments back to back.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSc1BleSegments<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    frame: JByteArray<'local>,
) -> JByteArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JByteArray<'local>> {
        let frame = env.convert_byte_array(&frame)?;
        let segs: Vec<u8> = punktfunk_core::client::sc1_ble::classic_ble_segments(&frame)
            .flatten()
            .collect();
        env.byte_array_from_slice(&segs)
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSc1BleReply(segments): ByteArray?` — the segments read back for one 2015
/// Steam Controller feature query, in order; the reply, report id 0 first, once the last is in.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSc1BleReply<'local>(
    mut env: EnvUnowned<'local>,
    _this: JObject<'local>,
    segments: JByteArray<'local>,
) -> JByteArray<'local> {
    env.with_env(|env| -> jni::errors::Result<JByteArray<'local>> {
        let segments = env.convert_byte_array(&segments)?;
        let mut reply = [0u8; 73];
        match punktfunk_core::client::sc1_ble::classic_ble_reply(&segments, &mut reply) {
            Some(n) => env.byte_array_from_slice(&reply[..n]),
            None => Ok(JByteArray::default()),
        }
    })
    .resolve::<LogErrorAndDefault>()
}

/// `NativeBridge.nativeSendPadIdentity(handle, pad, slot, serial, replies)` — a captured Steam
/// Controller 2's identity for wire pad `pad`: its Puck slot (0 off a Puck), its USB serial (empty
/// when unknown) and its replies to the identity queries, packed `[len][request][len][reply]…`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeSendPadIdentity(
    mut env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    pad: jint,
    slot: jint,
    serial: JString,
    replies: JByteArray,
) {
    env.with_env(|env| -> jni::errors::Result<()> {
        let Some(h) = SESSIONS.get(handle) else {
            return Ok(());
        };
        let serial = serial.try_to_string(env).unwrap_or_default();
        let replies = env.convert_byte_array(&replies)?;
        let pad = wire_pad(pad);
        let slot = u8::try_from(slot).unwrap_or(0);
        let id = punktfunk_core::quic::PadIdentity {
            pad,
            serial,
            replies,
            slot,
        };
        if let Err(e) = h.client.send_pad_identity(id) {
            log::warn!("pad identity not sent: {e:#}");
        }
        Ok(())
    })
    .resolve::<LogErrorAndDefault>()
}

/// The touchpad contact for Kotlin's ints: `finger` is the low bit (slot 0 or 1), `x`/`y` clamp
/// to the wire's 0..=65535.
fn pad_touch(pad: jint, finger: jint, active: bool, x: jint, y: jint) -> RichInput {
    RichInput::Touchpad {
        pad: wire_pad(pad),
        finger: (finger as u32 & 0x1) as u8,
        active,
        x: (x as i64).clamp(0, 65535) as u16,
        y: (y as i64).clamp(0, 65535) as u16,
    }
}

/// The motion sample for Kotlin's ints: each gyro and accel value clamps to signed 16 bits.
fn pad_motion(pad: jint, gyro: [jint; 3], accel: [jint; 3]) -> RichInput {
    let c = |v: jint| (v as i64).clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16;
    RichInput::Motion {
        pad: wire_pad(pad),
        gyro: gyro.map(c),
        accel: accel.map(c),
    }
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
        let _ = h
            .client
            .send_rich_input(pad_touch(pad, finger, active, x, y));
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
        let gyro = [gyro_pitch, gyro_yaw, gyro_roll];
        let accel = [accel_x, accel_y, accel_z];
        let _ = h.client.send_rich_input(pad_motion(pad, gyro, accel));
    })
}

/// `NativeBridge.nativeHoldsScreenAwake(handle, keepAwake)` — whether this stream holds the
/// screen awake right now: always with the "Keep screen awake" setting on, else only while the
/// player's input is recent.
///
/// Answered here because every input this client forwards goes through the session's
/// `send_input`, which is the one clock for "recent"; the rule is
/// [`punktfunk_core::client::holds_screen_awake`]. A `0` handle answers `true`: a screen that
/// stays on is the safe answer when the session cannot be asked.
#[unsafe(no_mangle)]
pub extern "system" fn Java_io_unom_punktfunk_kit_NativeBridge_nativeHoldsScreenAwake(
    _env: EnvUnowned,
    _this: JObject,
    handle: jlong,
    keep_awake: jboolean,
) -> jboolean {
    jni_guard(true, || {
        let Some(h) = SESSIONS.get(handle) else {
            return true;
        };
        punktfunk_core::client::holds_screen_awake(keep_awake, h.client.input_idle())
    })
}
