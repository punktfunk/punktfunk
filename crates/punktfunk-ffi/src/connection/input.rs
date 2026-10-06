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

/// Tell the host who the Steam Controller 2 on `pad` is, before its arrival: `serial` is its USB
/// serial (UTF-8, NUL-terminated), `replies` the `(request, reply)` pairs packed as
/// `[len][request][len][reply]…`, each part at most 64 bytes. Too long or torn is `InvalidArg`.
///
/// # Safety
/// `c` is a valid connection handle; `serial` is NUL-terminated; `replies` points to `len` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_send_pad_identity(
    c: *mut PunktfunkConnection,
    pad: u8,
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
}
