//! Sending input: events, mic packets, rich input, raw HID reports and pen samples,
//! plus the mode and gamepad they are sent against.

#[cfg(feature = "quic")]
use crate::*;
#[cfg(feature = "quic")]
use punktfunk_core::input::InputEvent;

/// Send one input event to the host as a QUIC datagram (non-blocking enqueue).
/// `InvalidArg` if `ev->kind` is not a recognized event kind.
///
/// # Safety
/// `c` is a valid connection handle; `ev` points to a readable `InputEvent`-sized allocation.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_send_input(
    c: *mut PunktfunkConnection,
    ev: *const InputEvent,
) -> PunktfunkStatus {
    with_conn!(c => {
        // SAFETY: `read_input_event` validates the tag before forming `&InputEvent` (else UB).
        let ev = match unsafe { read_input_event(ev) } {
            Ok(e) => e,
            Err(status) => return status,
        };
        status_of(c.inner.send_input(ev))
    })
}

/// Whether this stream holds the screen awake right now. `keep_awake` is the "Keep screen
/// awake" setting: on, always. Off, only while the player's input is recent, so the device's
/// own idle timer runs over a game left alone. Poll every few seconds; `*out` is filled when
/// non-NULL.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `bool`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_holds_screen_awake(
    c: *const PunktfunkConnection,
    keep_awake: bool,
    out: *mut bool,
) -> PunktfunkStatus {
    conn_out!(c, out => {
        punktfunk_core::client::holds_screen_awake(keep_awake, c.inner.input_idle())
    })
}

/// Send one Opus mic frame (48 kHz) as a QUIC datagram. The host decodes it into
/// a virtual microphone. Non-blocking; `seq`/`pts_ns` are diagnostics only.
/// Empty `opus_data`/`len` is DTX. Data is copied before return.
///
/// # Safety
/// `c` is a valid connection handle. For a representable nonzero `len`, `opus_data`
/// points to that many readable bytes; it may be NULL when `len == 0`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_send_mic(
    c: *mut PunktfunkConnection,
    opus_data: *const u8,
    len: usize,
    seq: u32,
    pts_ns: u64,
) -> PunktfunkStatus {
    with_conn!(c => {
        // SAFETY: `opus_data` is null or readable for `len` bytes (this fn's contract).
        let opus = match unsafe { in_bytes(opus_data, len) } {
            Ok(b) => b.to_vec(),
            Err(s) => return s,
        };
        status_of(c.inner.send_mic(seq, pts_ns, opus))
    })
}

/// Send one rich input (DualSense touchpad contact or motion) as a QUIC datagram.
/// No-op unless the host runs the DualSense backend. `InvalidArg` on unknown `kind`.
///
/// # Safety
/// `c` is a valid connection handle; `rich` points to a valid [`PunktfunkRichInput`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_send_rich_input(
    c: *mut PunktfunkConnection,
    rich: *const PunktfunkRichInput,
) -> PunktfunkStatus {
    with_conn!(c => {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let rich = match unsafe { rich.as_ref() } {
            Some(r) => r,
            None => return PunktfunkStatus::NullPointer,
        };
        match rich.to_rich() {
            Some(r) => status_of(c.inner.send_rich_input(r)),
            None => PunktfunkStatus::InvalidArg,
        }
    })
}

/// Send rich input via [`PunktfunkRichInputEx`] — the C path for `TouchpadEx`
/// (second trackpad / signed coords / pressure). Set
/// `rich->struct_size = sizeof(PunktfunkRichInputEx)`; a smaller layout is rejected.
///
/// # Safety
/// `c` is a valid connection handle; `rich` is null or points to at least its declared
/// `struct_size` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_send_rich_input2(
    c: *mut PunktfunkConnection,
    rich: *const PunktfunkRichInputEx,
) -> PunktfunkStatus {
    with_conn!(c => {
        if rich.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        // Size prefix first so the full read is bounded by what the caller declared.
        // SAFETY: `addr_of!` does not form a `&`; the caller may have a smaller older layout.
        let declared = unsafe { std::ptr::addr_of!((*rich).struct_size).read_unaligned() } as usize;
        if declared < std::mem::size_of::<PunktfunkRichInputEx>() {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: pointers are caller-supplied and null-checked on this path.
        match unsafe { *rich }.to_rich() {
            Some(r) => status_of(c.inner.send_rich_input(r)),
            None => PunktfunkStatus::InvalidArg,
        }
    })
}

/// Clamp `pad` to 16 and the report to `HID_REPORT_MAX` — same rules as the Android shim.
#[cfg(feature = "quic")]
fn hid_report_rich_input(pad: u8, report: &[u8]) -> punktfunk_core::quic::RichInput {
    let n = report.len().min(punktfunk_core::quic::HID_REPORT_MAX);
    let mut data = [0u8; punktfunk_core::quic::HID_REPORT_MAX];
    data[..n].copy_from_slice(&report[..n]);
    punktfunk_core::quic::RichInput::HidReport {
        pad: pad & 0xF,
        len: n as u8,
        data,
    }
}

/// Send one raw HID input report (SC2 as-is, `[0xCC][0x04]`). `len` clamps to
/// `HID_REPORT_MAX`; `pad` masks to 16. Lossy snapshots; empty is `InvalidArg`.
///
/// # Safety
/// `c` is a valid connection handle; `data` points to `len` readable bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_send_hid_report(
    c: *mut PunktfunkConnection,
    pad: u8,
    data: *const u8,
    len: usize,
) -> PunktfunkStatus {
    with_conn!(c => {
        if data.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        if len == 0 {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: caller pointer/length; borrowed for this call only. The clamp copies.
        let report =
            unsafe { std::slice::from_raw_parts(data, len.min(punktfunk_core::quic::HID_REPORT_MAX)) };
        status_of(c.inner.send_rich_input(hid_report_rich_input(pad, report)))
    })
}

/// Gate what of a Steam Controller 2's raw reports on `pad` reaches the host: an OR of
/// `PUNKTFUNK_SC2_GATE_*`. Call when the client's overlay takes or returns the pad and when its
/// system-button policy changes. Latest wins; unknown bits are ignored; `pad` masks to 16.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_sc2_gate(
    c: *mut PunktfunkConnection,
    pad: u8,
    gate: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        c.inner.set_sc2_gate(pad & 0xF, punktfunk_core::client::Sc2Gate::from_bits(gate));
        PunktfunkStatus::Ok
    })
}

/// The `index`-th feature query a client reads off a Steam Controller 2 for
/// [`punktfunk_connection_send_pad_identity`], id first, copied into `out`. Returns its length;
/// 0 past the last one or when `cap` is short. `puck` adds a Puck slot's queries.
///
/// # Safety
/// `out` points to `cap` writable bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_sc2_identity_request(
    puck: bool,
    index: u32,
    out: *mut u8,
    cap: usize,
) -> usize {
    let Some(req) = punktfunk_core::client::sc2::identity_requests(puck).nth(index as usize) else {
        return 0;
    };
    if out.is_null() || cap < req.len() {
        return 0;
    }
    // SAFETY: the caller grants `cap` writable bytes at `out`; `req` fits.
    unsafe { std::ptr::copy_nonoverlapping(req.as_ptr(), out, req.len()) };
    req.len()
}

/// [`punktfunk_sc2_identity_request`] for a 2015 Steam Controller: the `index`-th query, report
/// id 0 first. Returns its length; 0 past the last one or when `cap` is short.
///
/// # Safety
/// `out` points to `cap` writable bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_sc1_identity_request(
    index: u32,
    out: *mut u8,
    cap: usize,
) -> usize {
    let Some(req) = punktfunk_core::client::sc2::classic_identity_requests().nth(index as usize)
    else {
        return 0;
    };
    if out.is_null() || cap < req.len() {
        return 0;
    }
    // SAFETY: the caller grants `cap` writable bytes at `out`; `req` fits.
    unsafe { std::ptr::copy_nonoverlapping(req.as_ptr(), out, req.len()) };
    req.len()
}

/// Bytes of state [`punktfunk_sc1_ble_input`] keeps per pad.
#[cfg(feature = "quic")]
pub const PUNKTFUNK_SC1_BLE_STATE_LEN: usize = 160;
#[cfg(feature = "quic")]
const _: () =
    assert!(PUNKTFUNK_SC1_BLE_STATE_LEN == punktfunk_core::client::sc1_ble::CLASSIC_BLE_STATE);

/// One segment of a 2015 Steam Controller's Bluetooth input, `[0x03, header, 18 bytes]`. True
/// when it completed a state message: `out` then holds the 64-byte wired frame to forward.
/// `state` is [`PUNKTFUNK_SC1_BLE_STATE_LEN`] bytes, zeroed once per link and kept.
///
/// # Safety
/// `state` points to that many writable bytes, `segment` to `len` readable ones, `out` to 64
/// writable ones.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_sc1_ble_input(
    state: *mut u8,
    segment: *const u8,
    len: usize,
    out: *mut u8,
) -> bool {
    use punktfunk_core::client::sc1_ble::CLASSIC_BLE_STATE;
    if state.is_null() || segment.is_null() || out.is_null() {
        return false;
    }
    // SAFETY: the caller grants each buffer at the size documented above.
    let (state, segment, out) = unsafe {
        (
            &mut *state.cast::<[u8; CLASSIC_BLE_STATE]>(),
            std::slice::from_raw_parts(segment, len),
            &mut *out.cast::<[u8; 64]>(),
        )
    };
    punktfunk_core::client::sc1_ble::classic_ble_input(state, segment, out)
}

/// A wired feature frame for a 2015 Steam Controller, report id 0 first, as the 20-byte
/// segments its Bluetooth link takes, back to back in `out`. Returns the bytes written; 0 when
/// `cap` is short.
///
/// # Safety
/// `frame` points to `len` readable bytes, `out` to `cap` writable ones.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_sc1_ble_segments(
    frame: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
) -> usize {
    use punktfunk_core::client::sc1_ble::{classic_ble_segments, SEGMENT};
    if frame.is_null() || out.is_null() {
        return 0;
    }
    // SAFETY: the caller grants `len` readable bytes at `frame`.
    let segs: Vec<_> =
        classic_ble_segments(unsafe { std::slice::from_raw_parts(frame, len) }).collect();
    if cap < segs.len() * SEGMENT {
        return 0;
    }
    // SAFETY: the caller grants `cap` writable bytes at `out`, and the segments fit.
    let out = unsafe { std::slice::from_raw_parts_mut(out, cap) };
    for (i, seg) in segs.iter().enumerate() {
        out[i * SEGMENT..(i + 1) * SEGMENT].copy_from_slice(seg);
    }
    segs.len() * SEGMENT
}

/// The segments read back for one 2015 Steam Controller feature query, back to back and in
/// order. Once the last one is in, the reply goes to `out`, report id 0 first, and this returns
/// its length; 0 before then or when `cap` is short.
///
/// # Safety
/// `segments` points to `len` readable bytes, `out` to `cap` writable ones.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_sc1_ble_reply(
    segments: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
) -> usize {
    if segments.is_null() || out.is_null() {
        return 0;
    }
    let mut reply = [0u8; 73];
    // SAFETY: the caller grants `len` readable bytes at `segments`.
    let segments = unsafe { std::slice::from_raw_parts(segments, len) };
    let Some(n) = punktfunk_core::client::sc1_ble::classic_ble_reply(segments, &mut reply) else {
        return 0;
    };
    if cap < n {
        return 0;
    }
    // SAFETY: the caller grants `cap` writable bytes at `out`; `n` fits.
    unsafe { std::ptr::copy_nonoverlapping(reply.as_ptr(), out, n) };
    n
}

/// Tell the host who the Steam Controller 2 on `pad` is, before its arrival: `serial` is its USB
/// serial (UTF-8, NUL-terminated), `replies` the `(request, reply)` pairs packed as
/// `[len][request][len][reply]…`, each part at most 64 bytes. `slot` is a Puck pad's slot (its USB
/// interface less 2), else 0. Too long, torn or a slot past 3 is `InvalidArg`.
///
/// # Safety
/// `c` is a valid connection handle; `serial` is NUL-terminated; `replies` points to `len` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_send_pad_identity(
    c: *mut PunktfunkConnection,
    pad: u8,
    slot: u8,
    serial: *const std::ffi::c_char,
    replies: *const u8,
    len: usize,
) -> PunktfunkStatus {
    with_conn!(c => {
        if serial.is_null() || (replies.is_null() && len > 0) {
            return PunktfunkStatus::NullPointer;
        }
        // SAFETY: caller-provided NUL-terminated string, borrowed for this call.
        let Ok(serial) = unsafe { std::ffi::CStr::from_ptr(serial) }.to_str() else {
            return PunktfunkStatus::InvalidArg;
        };
        let replies = if len == 0 {
            Vec::new()
        } else {
            // SAFETY: caller pointer/length, copied before returning.
            unsafe { std::slice::from_raw_parts(replies, len) }.to_vec()
        };
        status_of(c.inner.send_pad_identity(punktfunk_core::quic::PadIdentity {
            pad: pad & 0xF,
            serial: serial.to_string(),
            replies,
            slot,
        }))
    })
}

/// Send one stylus sample batch — `count` (`1..=PUNKTFUNK_PEN_BATCH_MAX`)
/// [`PunktfunkPenSample`]s, oldest first — as one `0xCC/0x05` pen datagram
/// (`design/pen-tablet-input.md`). Split longer runs. Gate on
/// `host_caps & PUNKTFUNK_HOST_CAP_PEN`; without it this is `Unsupported` (keep
/// pen-as-touch). `InvalidArg` on a bad count or sample.
///
/// # Safety
/// `c` is a valid connection handle; `samples` is null or points to `count` valid
/// [`PunktfunkPenSample`]s.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_send_pen(
    c: *mut PunktfunkConnection,
    samples: *const PunktfunkPenSample,
    count: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if samples.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        if count == 0 || count > PUNKTFUNK_PEN_BATCH_MAX {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: caller pointer/length; borrowed for this call only.
        let raw = unsafe { std::slice::from_raw_parts(samples, count as usize) };
        let mut batch = [punktfunk_core::quic::PenSample::default(); punktfunk_core::quic::PEN_BATCH_MAX];
        for (slot, s) in batch.iter_mut().zip(raw) {
            match s.to_sample() {
                Some(v) => *slot = v,
                None => return PunktfunkStatus::InvalidArg,
            }
        }
        status_of(c.inner.send_pen(&batch[..count as usize]))
    })
}

/// Send the whole touchpad as one `0xCC/0x06` frame (`design/touchpad-passthrough.md`). Send
/// one with no contacts when the source appears, and one on every change after. Core stamps
/// the sequence and repeats the last frame every 100 ms while a contact or the button is down.
/// Gate on [`punktfunk_connection_host_touchpad`]; without it this is `Unsupported`.
/// `InvalidArg` on a count above `PUNKTFUNK_TOUCHPAD_MAX_CONTACTS`, an unknown state bit, an id
/// above `PUNKTFUNK_TOUCHPAD_MAX_ID` or one id twice.
///
/// # Safety
/// `c` is a valid connection handle; `frame` is null or points to a readable
/// [`PunktfunkTouchpadFrame`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_send_touchpad(
    c: *mut PunktfunkConnection,
    frame: *const PunktfunkTouchpadFrame,
) -> PunktfunkStatus {
    with_conn!(c => {
        // SAFETY: caller pointer, null or readable; copied before use.
        let Some(frame) = (unsafe { frame.as_ref() }).copied() else {
            return PunktfunkStatus::NullPointer;
        };
        let Some((n, contacts)) = frame.to_contacts() else {
            return PunktfunkStatus::InvalidArg;
        };
        status_of(c.inner.send_touchpad(
            frame.button != 0,
            frame.width_dmm,
            frame.height_dmm,
            &contacts[..n],
        ))
    })
}

/// An OS pinch and rotation as two fingers on the host's touchpad, for a source that gives no
/// finger positions. `scale` and `rotation` (radians, clockwise) are cumulative since
/// `PUNKTFUNK_PINCH_BEGIN`; `PUNKTFUNK_PINCH_END` lifts both. `Unsupported` without
/// [`punktfunk_connection_host_touchpad`]; `InvalidArg` on an unknown phase or a value that is
/// not finite.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_send_touchpad_pinch(
    c: *mut PunktfunkConnection,
    phase: u8,
    scale: f32,
    rotation: f32,
) -> PunktfunkStatus {
    use punktfunk_core::quic::PinchPhase;
    with_conn!(c => {
        let phase = match phase {
            PUNKTFUNK_PINCH_BEGIN => PinchPhase::Begin,
            PUNKTFUNK_PINCH_UPDATE => PinchPhase::Update,
            PUNKTFUNK_PINCH_END => PinchPhase::End,
            _ => return PunktfunkStatus::InvalidArg,
        };
        status_of(c.inner.send_touchpad_pinch(phase, scale, rotation))
    })
}

/// Every touch on a touch surface now, on the host's touchpad. Core forwards up to
/// `max_contacts` (`1..=5`); more touches (a `count` above five is fine), or `keep` when the
/// client's OS or app claims the gesture, take a forwarded gesture back from the host.
/// `retract_taps`: this source makes its own clicks from taps, so the host must not. The
/// surface is `width_dmm` × `height_dmm` tenths of a millimetre. `time_us` is the platform's
/// capture time of these touches in µs, monotonic, on one epoch across calls; `0` takes core's
/// clock now. The host presents the frames at it. Call on every change, and once with
/// `count = 0` when the source appears.
///
/// `*forwarded` true: core owns this update; do not also turn these touches into pointer or
/// scroll. False: the gesture is the caller's until an update with no touches, and on any
/// error. `Unsupported` without [`punktfunk_connection_host_touchpad`]; `InvalidArg` on a
/// coordinate that is not finite or one `id` twice.
///
/// # Safety
/// `c` is a valid connection handle; `touches` points to `count` readable
/// [`PunktfunkSurfaceTouch`]es, or is null when `count` is 0; `forwarded` is null or writable.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connection_touchpad_surface(
    c: *mut PunktfunkConnection,
    touches: *const PunktfunkSurfaceTouch,
    count: u8,
    max_contacts: u8,
    button: bool,
    keep: bool,
    retract_taps: bool,
    width_dmm: u16,
    height_dmm: u16,
    time_us: u64,
    forwarded: *mut bool,
) -> PunktfunkStatus {
    // SAFETY: the caller passes `forwarded` null or writable for one value.
    unsafe { put(forwarded, false) };
    with_conn!(c => {
        if touches.is_null() && count != 0 {
            return PunktfunkStatus::NullPointer;
        }
        let raw = if count == 0 {
            &[][..]
        } else {
            // SAFETY: non-null here; the caller makes it readable for `count` touches, borrowed
            // for this call only.
            unsafe { std::slice::from_raw_parts(touches, usize::from(count)) }
        };
        let touches: Vec<_> = raw
            .iter()
            .map(|t| punktfunk_core::client::SurfaceTouch { id: t.id, x: t.x, y: t.y })
            .collect();
        match c.inner.touchpad_surface(
            &touches,
            max_contacts,
            button,
            keep,
            retract_taps,
            width_dmm,
            height_dmm,
            time_us,
        ) {
            Ok(owned) => {
                // SAFETY: as above.
                unsafe { put(forwarded, owned) };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Currently active session mode — Welcome's, until an accepted
/// [`punktfunk_connection_request_mode`] switches it. Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_mode(
    c: *const PunktfunkConnection,
    width: *mut u32,
    height: *mut u32,
    refresh_hz: *mut u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        let mode = c.inner.mode();
        // SAFETY: the caller passes each out-param null or writable for one value.
        unsafe {
            put(width, mode.width);
            put(height, mode.height);
            put(refresh_hz, mode.refresh_hz);
        }
        PunktfunkStatus::Ok
    })
}

/// The active session mode's exact rate in millihertz: `59940` at 59.94 Hz, `60000` at
/// 60 Hz. What the host delivers, so a host that takes no fraction answers the whole rate.
/// Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; `refresh_mhz` is null or writable.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_refresh_mhz(
    c: *const PunktfunkConnection,
    refresh_mhz: *mut u32,
) -> PunktfunkStatus {
    conn_out!(c, refresh_mhz => c.inner.mode().millihz())
}

/// Virtual gamepad the host resolved (`PUNKTFUNK_GAMEPAD_*`; Welcome echo of
/// [`punktfunk_connect_ex2`]). `AUTO` = a host that didn't say — assume X-Box 360,
/// no HID-output. Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; `gamepad` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_gamepad(
    c: *const PunktfunkConnection,
    gamepad: *mut u32,
) -> PunktfunkStatus {
    conn_out!(c, gamepad => c.inner.resolved_gamepad.to_u8() as u32)
}

#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::*;

    /// `punktfunk_connection_send_hid_report`'s clamp: `pad` masked to 16 and the
    /// report bounded to `HID_REPORT_MAX` — same rules as the Android JNI shim.
    #[test]
    fn send_hid_report_clamps_like_the_android_shim() {
        // A 46-byte state report (id 0x45) passes through unclamped.
        let mut state = vec![0u8; 46];
        state[0] = 0x45;
        state[1] = 0xE5; // seq
        match hid_report_rich_input(3, &state) {
            punktfunk_core::quic::RichInput::HidReport { pad, len, data } => {
                assert_eq!(pad, 3);
                assert_eq!(len, 46);
                assert_eq!(data[..46], state[..]);
                assert_eq!(data[46..], [0; punktfunk_core::quic::HID_REPORT_MAX - 46]);
            }
            other => panic!("expected HidReport, got {other:?}"),
        }
        // Oversize input truncates to the wire body; a pad above the wire space wraps into it.
        let big = vec![0xAB; 100];
        match hid_report_rich_input(0x17, &big) {
            punktfunk_core::quic::RichInput::HidReport { pad, len, data } => {
                assert_eq!(pad, 0x7);
                assert_eq!(len as usize, punktfunk_core::quic::HID_REPORT_MAX);
                assert_eq!(data, [0xAB; punktfunk_core::quic::HID_REPORT_MAX]);
            }
            other => panic!("expected HidReport, got {other:?}"),
        }
    }

    /// The C frame's caps are the wire's, and an embedder's frame is read strictly.
    #[test]
    fn a_touchpad_frame_is_read_strictly() {
        use punktfunk_core::quic::{TOUCHPAD_MAX_CONTACTS, TOUCHPAD_MAX_ID};
        assert_eq!(
            PUNKTFUNK_TOUCHPAD_MAX_CONTACTS as usize,
            TOUCHPAD_MAX_CONTACTS
        );
        assert_eq!(PUNKTFUNK_TOUCHPAD_MAX_ID, TOUCHPAD_MAX_ID);
        let finger = PunktfunkTouchpadContact {
            id: 4,
            state: PUNKTFUNK_TOUCHPAD_TIP,
            x: 7,
            y: 9,
        };
        let mut frame = PunktfunkTouchpadFrame {
            width_dmm: 700,
            height_dmm: 1500,
            button: 1,
            count: 1,
            contacts: [finger; 5],
        };
        let (n, contacts) = frame.to_contacts().unwrap();
        assert_eq!(n, 1);
        let c = contacts[0];
        assert_eq!((c.id, c.tip, c.confident, c.x, c.y), (4, true, false, 7, 9));
        frame.contacts[0].state = 0x04;
        assert!(frame.to_contacts().is_none(), "unknown state bit");
        frame.contacts[0].state = PUNKTFUNK_TOUCHPAD_CONFIDENT;
        frame.count = 6;
        assert!(frame.to_contacts().is_none(), "count above five");
        assert_eq!(std::mem::size_of::<PunktfunkTouchpadFrame>(), 36);
        assert_eq!(std::mem::size_of::<PunktfunkSurfaceTouch>(), 16);
    }
}
