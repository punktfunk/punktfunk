//! Shared clipboard (`design/clipboard-and-file-transfer.md`). All poll/serve
//! bytes ride the mTLS-pinned QUIC session; nothing here opens a new listener.

#[cfg(feature = "quic")]
use crate::*;

/// [`PunktfunkClipEvent::kind`]: host announced clipboard content
/// (`transfer_id` = offer `seq`; `data`/`len` = `\n`-separated `"<mime>\t<size_hint>"`).
/// Fetch lazily on local paste via [`punktfunk_connection_clipboard_fetch`].
pub const PUNKTFUNK_CLIP_REMOTE_OFFER: u8 = 1;
/// [`PunktfunkClipEvent::kind`]: host ack / policy / backend update
/// (`enabled`/`policy`/`reason` valid). Reflect it in the toggle UI.
pub const PUNKTFUNK_CLIP_STATE: u8 = 2;
/// [`PunktfunkClipEvent::kind`]: host is pasting our offered data. Answer with
/// [`punktfunk_connection_clipboard_serve`] (`transfer_id` = `req_id`;
/// `seq`/`file_index` valid; `data`/`len` = requested MIME).
pub const PUNKTFUNK_CLIP_FETCH_REQUEST: u8 = 3;
/// [`PunktfunkClipEvent::kind`]: bytes for a fetch we started (`transfer_id` = `xfer_id`;
/// `data`/`len` borrowed until the next `next_clipboard`; `last` = final chunk).
pub const PUNKTFUNK_CLIP_DATA: u8 = 4;
/// [`PunktfunkClipEvent::kind`]: a transfer was cancelled (`transfer_id` = the id).
pub const PUNKTFUNK_CLIP_CANCELLED: u8 = 5;
/// [`PunktfunkClipEvent::kind`]: a transfer failed (`transfer_id` = the id; `status` = a
/// `PunktfunkStatus` code).
pub const PUNKTFUNK_CLIP_ERROR: u8 = 6;

/// One advertised clipboard format passed to [`punktfunk_connection_clipboard_offer`].
#[cfg(feature = "quic")]
#[repr(C)]
pub struct PunktfunkClipKind {
    /// NUL-terminated UTF-8 wire MIME (e.g. `text/plain;charset=utf-8`). ≤ 128 bytes on the wire.
    pub mime: *const std::os::raw::c_char,
    /// Best-effort size in bytes; `0` = unknown.
    pub size_hint: u64,
}

/// Shared-clipboard event from [`punktfunk_connection_next_clipboard`]. Flat tagged
/// struct: read the fields named in the `kind`'s doc; the rest are 0.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkClipEvent {
    /// One of `PUNKTFUNK_CLIP_*`.
    pub kind: u8,
    /// `State`: 1 = enabled, 0 = disabled.
    pub enabled: u8,
    /// `State`: bitfield of `quic::CLIP_POLICY_*` — what the host currently permits.
    pub policy: u8,
    /// `State`: one of `quic::CLIP_REASON_*`.
    pub reason: u8,
    /// `Data`: 1 = final chunk of this transfer.
    pub last: u8,
    /// Per-transfer id: offer `seq` (RemoteOffer), `req_id` (FetchRequest), or
    /// `xfer_id` (Data/Cancelled/Error).
    pub transfer_id: u32,
    /// `FetchRequest`: the offer `seq` the request is against.
    pub seq: u32,
    /// `FetchRequest`: file index, or `quic::CLIP_FILE_INDEX_NONE`.
    pub file_index: u32,
    /// `Error`: a `PunktfunkStatus` code (negative); 0 otherwise.
    pub status: i32,
    /// RemoteOffer/FetchRequest/Data: pointer into a per-connection slot, valid
    /// until the next `next_clipboard`; NULL for the other kinds.
    pub data: *const u8,
    /// Byte length of `data` (0 when `data` is NULL).
    pub len: usize,
}

/// Fill a [`PunktfunkClipEvent`] from a core event, parking variable-length bytes
/// in `slot` (borrow-until-next-call) and pointing `data`/`len` at them.
#[cfg(feature = "quic")]
fn build_clip_event(
    ev: punktfunk_core::clipboard::ClipEventCore,
    slot: &mut Option<Vec<u8>>,
) -> PunktfunkClipEvent {
    use punktfunk_core::clipboard::ClipEventCore as E;
    let mut out = PunktfunkClipEvent {
        kind: 0,
        enabled: 0,
        policy: 0,
        reason: 0,
        last: 0,
        transfer_id: 0,
        seq: 0,
        file_index: 0,
        status: 0,
        data: std::ptr::null(),
        len: 0,
    };
    *slot = None;
    match ev {
        E::RemoteOffer { seq, kinds } => {
            out.kind = PUNKTFUNK_CLIP_REMOTE_OFFER;
            out.transfer_id = seq;
            let mut blob = String::new();
            for k in &kinds {
                blob.push_str(&k.mime);
                blob.push('\t');
                blob.push_str(&k.size_hint.to_string());
                blob.push('\n');
            }
            *slot = Some(blob.into_bytes());
        }
        E::State {
            enabled,
            policy,
            reason,
        } => {
            out.kind = PUNKTFUNK_CLIP_STATE;
            out.enabled = enabled as u8;
            out.policy = policy;
            out.reason = reason;
        }
        E::FetchRequest {
            req_id,
            seq,
            file_index,
            mime,
        } => {
            out.kind = PUNKTFUNK_CLIP_FETCH_REQUEST;
            out.transfer_id = req_id;
            out.seq = seq;
            out.file_index = file_index;
            *slot = Some(mime.into_bytes());
        }
        E::Data {
            xfer_id,
            bytes,
            last,
        } => {
            out.kind = PUNKTFUNK_CLIP_DATA;
            out.transfer_id = xfer_id;
            out.last = last as u8;
            *slot = Some(bytes);
        }
        E::Cancelled { id } => {
            out.kind = PUNKTFUNK_CLIP_CANCELLED;
            out.transfer_id = id;
        }
        E::Error { id, code } => {
            out.kind = PUNKTFUNK_CLIP_ERROR;
            out.transfer_id = id;
            out.status = code;
        }
    }
    if let Some(v) = slot.as_ref() {
        out.data = v.as_ptr();
        out.len = v.len();
    }
    out
}

/// Enable or disable the shared clipboard. Opt-in: nothing is announced or served
/// until `enabled = true`. `flags` carries `quic::CLIP_FLAG_FILES`. The host
/// replies with a `State` event.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_clipboard_control(
    c: *const PunktfunkConnection,
    enabled: bool,
    flags: u8,
) -> PunktfunkStatus {
    with_conn!(c => {
        status_of(c.inner.clip_control(enabled, flags))
    })
}

/// Announce that the local clipboard changed — the lazy format-list offer. `seq`
/// is monotonic per sender (newest wins); `kinds`/`n` is the advertised formats
/// (≤ 16). Bytes cross only if the host later fetches.
///
/// # Safety
/// `c` is a valid connection handle. For `n <= 16`, `kinds` points to `n`
/// `PunktfunkClipKind`s (NULL only for zero), each with a NUL-terminated UTF-8 `mime`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_clipboard_offer(
    c: *const PunktfunkConnection,
    seq: u32,
    kinds: *const PunktfunkClipKind,
    n: usize,
) -> PunktfunkStatus {
    with_conn!(c => {
        if kinds.is_null() && n != 0 {
            return PunktfunkStatus::NullPointer;
        }
        if n > punktfunk_core::quic::CLIP_MAX_KINDS || ffi_slice_bytes::<PunktfunkClipKind>(n).is_none() {
            return PunktfunkStatus::InvalidArg;
        }
        let mut out = Vec::with_capacity(n);
        if n != 0 {
            // SAFETY: `n` is capped and `ffi_slice_bytes`-checked; borrowed for this call.
            let slice = unsafe { std::slice::from_raw_parts(kinds, n) };
            for k in slice {
                // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
                let Ok(mime) = (unsafe { opt_cstr(k.mime) }) else {
                    return PunktfunkStatus::InvalidArg;
                };
                out.push(punktfunk_core::quic::ClipKind {
                    mime: mime.unwrap_or_default().to_string(),
                    size_hint: k.size_hint,
                });
            }
        }
        status_of(c.inner.clip_offer(seq, out))
    })
}

/// Start pulling one format (`mime`) of the host's current offer `seq` — lazily,
/// on a local paste. `file_index` selects a file, or `quic::CLIP_FILE_INDEX_NONE`.
/// Writes the transfer id to `xfer_id_out`.
///
/// # Safety
/// `c` is a valid connection handle; `mime` is a NUL-terminated UTF-8 string;
/// `xfer_id_out` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_clipboard_fetch(
    c: *const PunktfunkConnection,
    seq: u32,
    mime: *const std::os::raw::c_char,
    file_index: u32,
    xfer_id_out: *mut u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
        let mime = match unsafe { opt_cstr(mime) } {
            Ok(Some(s)) => s.to_string(),
            Ok(None) => return PunktfunkStatus::NullPointer,
            Err(()) => return PunktfunkStatus::InvalidArg,
        };
        match c.inner.clip_fetch(seq, mime, file_index) {
            Ok(xfer_id) => {
                // SAFETY: the caller passes `xfer_id_out` null or writable for one value.
                unsafe { put(xfer_id_out, xfer_id) };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Provide bytes answering a `FetchRequest` (the host is pasting our offered data).
/// Call repeatedly to stream; `last = true` completes. `data` may be NULL only when
/// `len == 0`. `punktfunk_connection_clipboard_cancel(req_id)` aborts.
///
/// # Safety
/// `c` is a valid connection handle. For a representable nonzero `len`, `data` points
/// to that many readable bytes; it may be NULL when `len == 0`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_clipboard_serve(
    c: *const PunktfunkConnection,
    req_id: u32,
    data: *const u8,
    len: usize,
    last: bool,
) -> PunktfunkStatus {
    with_conn!(c => {
        // SAFETY: `data` is null or readable for `len` bytes (this fn's contract).
        let bytes = match unsafe { in_bytes(data, len) } {
            Ok(b) => b.to_vec(),
            Err(s) => return s,
        };
        status_of(c.inner.clip_serve(req_id, bytes, last))
    })
}

/// Cancel a clipboard transfer by id — outbound fetch (`xfer_id` from
/// [`punktfunk_connection_clipboard_fetch`]) or inbound serve (`req_id` from a `FetchRequest`).
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_clipboard_cancel(
    c: *const PunktfunkConnection,
    id: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        status_of(c.inner.clip_cancel(id))
    })
}

/// Pull the next shared-clipboard event into `*out`. [`PunktfunkStatus::NoFrame`]
/// on timeout, [`PunktfunkStatus::Closed`] once ended. `data`/`len` (when non-NULL)
/// borrows until the next `next_clipboard` on this handle.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkClipEvent`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_clipboard(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkClipEvent,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_clip(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(ev) => {
                let mut slot = lock_recover(&c.last_clip);
                let out_ev = build_clip_event(ev, &mut slot);
                // SAFETY: caller out-param, non-null on this path, written once.
                unsafe { *out = out_ev };
                PunktfunkStatus::Ok
            }
            Err(e) => {
                // Drop the parked payload: no other release, and a 50 MiB paste would linger.
                *lock_recover(&c.last_clip) = None;
                e.status()
            }
        }
    })
}
