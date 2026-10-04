//! Stable `extern "C"` surface. `cbindgen` emits `include/punktfunk_core.h`
//! (`cargo run -p gen-headers`). Pin with [`punktfunk_abi_version`] and `struct_size`.
//!
//! Opaque handles only. Cross-boundary structs are `#[repr(C)]`; buffers are
//! pointer + length. Every handle from `*_new` / `*_pair` must reach
//! [`punktfunk_session_free`]. A [`PunktfunkFrame`]'s `data` is borrowed until
//! the next `poll`/`free` on that session — copy it out.
//!
//! Callers own every pointer. Handles stay valid until `*_free` (`as_mut` /
//! `as_ref` turn null into `None`). Out-params are writable slots; C strings
//! are NUL-terminated or null (`opt_cstr`). Nothing is retained past the call.
//!
//! Panics never cross: [`guard`] maps them to `PunktfunkStatus::Panic`;
//! [`guard_void`] swallows teardown panics. Bare entry points cannot panic.
//! Evidence: `include/punktfunk_core.h`.
//!
//! Everything behind the surface is `punktfunk-core`. This crate holds only the
//! boundary, so core's Rust consumers never build the cdylib or staticlib.

// An `unsafe fn` body still scopes each unsafe op in a block with its own proof.
#![forbid(unsafe_op_in_unsafe_fn)]

// Declaration order is header order: cbindgen emits this file's items, then each
// module in the order below. A blank line between them keeps rustfmt from sorting.
mod session;

mod connection;

mod bitstream;

mod reanchor;

mod demo;

// The loopback host behind `punktfunk_demo_host_*`. Safe Rust, like core.
/// cbindgen:ignore
#[cfg(feature = "quic")]
#[deny(unsafe_code)]
pub mod demo_host;

pub use bitstream::*;
pub use connection::*;
#[cfg(feature = "quic")]
pub use demo::*;
pub use reanchor::*;
pub use session::*;

use punktfunk_core::error::PunktfunkStatus;
use std::ffi::{c_void, CStr};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;
use std::ptr;

/// Recover a poisoned mutex. Slots are last-value caches; a poisoned writer
/// still left structurally valid data.
fn lock_recover<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[inline]
fn guard<F: FnOnce() -> PunktfunkStatus>(f: F) -> PunktfunkStatus {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or(PunktfunkStatus::Panic)
}

fn ffi_slice_bytes<T>(len: usize) -> Option<usize> {
    len.checked_mul(std::mem::size_of::<T>())
        .filter(|&bytes| bytes <= isize::MAX as usize)
}

/// Fill an optional out-param; null is a no-op. `write` never reads or drops the old value,
/// so the slot may be uninitialised caller memory.
///
/// # Safety
/// `p` is null or valid for a write of one aligned `T`.
unsafe fn put<T>(p: *mut T, v: T) {
    if let Some(p) = ptr::NonNull::new(p) {
        // SAFETY: non-null here; the caller guarantees a non-null `p` is valid for writes.
        unsafe { p.write(v) };
    }
}

/// `Ok(())` is `Ok`; an error is its status.
fn status_of(r: punktfunk_core::Result<()>) -> PunktfunkStatus {
    match r {
        Ok(()) => PunktfunkStatus::Ok,
        Err(e) => e.status(),
    }
}

/// Borrow `len` caller bytes at `p` for this call. Null with a non-zero `len` is
/// `NullPointer`, a length no slice can hold is `InvalidArg`, and `len == 0` never reads `p`.
///
/// # Safety
/// `p` is null or readable for `len` bytes that stay unmodified while the slice lives.
unsafe fn in_bytes<'a>(p: *const u8, len: usize) -> Result<&'a [u8], PunktfunkStatus> {
    if p.is_null() && len != 0 {
        return Err(PunktfunkStatus::NullPointer);
    }
    if ffi_slice_bytes::<u8>(len).is_none() {
        return Err(PunktfunkStatus::InvalidArg);
    }
    if len == 0 {
        return Ok(&[]);
    }
    // SAFETY: `p` is non-null here and readable for `len` bytes (caller contract);
    // `ffi_slice_bytes` proved the extent fits a Rust slice.
    Ok(unsafe { std::slice::from_raw_parts(p, len) })
}

/// [`guard`] around a borrowed connection handle `$c`; null returns `NullPointer`. A shared
/// borrow only: plane threads pull concurrently and must never alias a `&mut`.
#[cfg(feature = "quic")]
macro_rules! with_conn {
    ($c:ident => $body:block) => {
        $crate::guard(|| {
            // SAFETY: the calling entry point's `# Safety` makes the handle null or live;
            // `as_ref` maps null to `None` and never dereferences it.
            let Some($c) = (unsafe { $c.as_ref() }) else {
                return punktfunk_core::error::PunktfunkStatus::NullPointer;
            };
            $body
        })
    };
}
#[cfg(feature = "quic")]
pub(crate) use with_conn;

/// [`with_conn!`] that writes `$value` into the nullable out-param `$out` and returns `Ok`.
#[cfg(feature = "quic")]
macro_rules! conn_out {
    ($c:ident, $out:ident => $value:expr) => {
        $crate::with_conn!($c => {
            let v = $value;
            // SAFETY: the calling entry point's `# Safety` makes `$out` null or writable for
            // one value.
            unsafe { $crate::put($out, v) };
            punktfunk_core::error::PunktfunkStatus::Ok
        })
    };
}
#[cfg(feature = "quic")]
pub(crate) use conn_out;

/// Copy a SHA-256 into an optional 32-byte caller buffer; null is a no-op. Writes the array by
/// value, never forming a slice over memory C may leave uninitialised.
///
/// # Safety
/// `out` is null or writable for 32 bytes.
#[cfg(feature = "quic")]
unsafe fn put_sha256(out: *mut u8, fp: [u8; 32]) {
    // SAFETY: `[u8; 32]` has alignment 1, so the caller's 32 writable bytes satisfy `put`.
    unsafe { put(out.cast::<[u8; 32]>(), fp) };
}

/// Copy `s` and a NUL into `out` when both fit in `cap` bytes. `false`, with nothing written,
/// when `out` is null or `s.len() + 1 > cap`.
///
/// # Safety
/// `out` is null or writable for `cap` bytes.
#[cfg(feature = "quic")]
unsafe fn write_cstr(out: *mut c_char, cap: usize, s: &str) -> bool {
    if out.is_null() || s.len() >= cap {
        return false;
    }
    // SAFETY: `out` is non-null and writable for `cap` > `s.len()` bytes; `s` is Rust memory
    // and cannot overlap it. `.cast()`: `c_char` is i8 on x86_64 and u8 on aarch64.
    unsafe {
        ptr::copy_nonoverlapping(s.as_ptr(), out.cast::<u8>(), s.len());
        out.add(s.len()).write(0);
    }
    true
}

/// [`guard`] for teardown with no status: swallow the panic. Unwinding into C
/// aborts the embedder; the object is being dropped either way.
fn guard_void<F: FnOnce()>(f: F) {
    if std::panic::catch_unwind(AssertUnwindSafe(f)).is_err() {
        tracing::error!("panic escaped a punktfunk_* teardown entry point; swallowed at the C ABI");
    }
}

/// Current ABI version. Mismatch with [`punktfunk_core::ABI_VERSION`] is an incompatible core.
#[unsafe(no_mangle)]
pub extern "C" fn punktfunk_abi_version() -> u32 {
    punktfunk_core::ABI_VERSION
}

/// Log sink for [`punktfunk_set_log_callback`]. `level` 1..=5 (error…trace).
/// `target` and `message` are NUL-terminated UTF-8, borrowed for this call —
/// copy them out. Any thread may call; thread-safe, non-blocking, no re-entry.
pub type PunktfunkLogCb = Option<
    unsafe extern "C" fn(
        level: u8,
        target: *const c_char,
        message: *const c_char,
        user: *mut c_void,
    ),
>;

/// `user` is kept as an address: an opaque token handed back to `cb`, never dereferenced here.
#[derive(Clone, Copy)]
struct LogSink {
    cb: unsafe extern "C" fn(u8, *const c_char, *const c_char, *mut c_void),
    user: usize,
}

static LOG_SINK: std::sync::Mutex<Option<LogSink>> = std::sync::Mutex::new(None);
static LOG_INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// `log` backend for [`punktfunk_set_log_callback`]. Installed once; the sink slot swaps.
struct CallbackLogger;

impl log::Log for CallbackLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        // Level gating is `log::set_max_level` in `punktfunk_set_log_callback`.
        true
    }

    fn log(&self, record: &log::Record) {
        // Drop the lock before the callback: a re-entrant log must duplicate, not deadlock.
        let Some(sink) = *lock_recover(&LOG_SINK) else {
            return;
        };
        let cstr = |s: String| {
            // Interior NUL cannot be a C string; drop that byte, keep the line.
            let mut bytes = s.into_bytes();
            bytes.retain(|&b| b != 0);
            std::ffi::CString::new(bytes).unwrap_or_default()
        };
        let target = cstr(record.target().to_string());
        let message = cstr(record.args().to_string());
        // SAFETY: sink matches this signature; both strings are locals and live for this call.
        unsafe {
            (sink.cb)(
                record.level() as u8,
                target.as_ptr(),
                message.as_ptr(),
                sink.user as *mut c_void,
            )
        };
    }

    fn flush(&self) {}
}

/// Route core `log`/`tracing` lines to `cb`. `max_level` 1..=5 (error…trace), 0 = off.
/// 3 (info) is the usual default; debug/trace is per-packet. `cb == NULL` detaches.
/// `Unsupported` if another `log` backend is already installed. Idempotent.
///
/// Detaching or replacing stops new callbacks but does not wait for ones already running.
///
/// # Safety
/// `cb` and `user` stay valid until every thread that may log has stopped, not just until
/// the next call here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_set_log_callback(
    max_level: u8,
    cb: PunktfunkLogCb,
    user: *mut c_void,
) -> PunktfunkStatus {
    guard(|| {
        let installed = *LOG_INSTALLED.get_or_init(|| log::set_logger(&CallbackLogger).is_ok());
        if !installed {
            return PunktfunkStatus::Unsupported;
        }
        *lock_recover(&LOG_SINK) = cb.map(|cb| LogSink {
            cb,
            user: user as usize,
        });
        log::set_max_level(match (cb.is_some(), max_level) {
            (false, _) | (_, 0) => log::LevelFilter::Off,
            (_, 1) => log::LevelFilter::Error,
            (_, 2) => log::LevelFilter::Warn,
            (_, 3) => log::LevelFilter::Info,
            (_, 4) => log::LevelFilter::Debug,
            _ => log::LevelFilter::Trace,
        });
        PunktfunkStatus::Ok
    })
}

/// Wake-on-LAN magic packet. `macs` is `mac_count` contiguous 6-byte MACs.
/// `last_known_ip` is an optional unicast target, used only when it is an IPv4
/// dotted quad. Broadcasts subnet-directed and `255.255.255.255` on ports 9
/// and 7. No session needed.
/// `Ok` if at least one datagram was sent. Call off the UI thread.
///
/// # Safety
/// Nonzero representable `mac_count`: `macs` is `mac_count * 6` readable bytes.
/// `last_known_ip`, if non-NULL, is a NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_wake_on_lan(
    macs: *const u8,
    mac_count: usize,
    last_known_ip: *const c_char,
) -> PunktfunkStatus {
    guard(|| {
        if macs.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let Some(byte_len) = ffi_slice_bytes::<punktfunk_core::wol::Mac>(mac_count) else {
            return PunktfunkStatus::InvalidArg;
        };
        if byte_len == 0 {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: `ffi_slice_bytes` proved `mac_count` MACs fit a Rust slice; borrowed for this call.
        let bytes = unsafe { std::slice::from_raw_parts(macs, byte_len) };
        let mac_vec: Vec<punktfunk_core::wol::Mac> = bytes
            .chunks_exact(6)
            .map(|c| {
                let mut m = [0u8; 6];
                m.copy_from_slice(c);
                m
            })
            .collect();
        // A hostname or IPv6 address skips the unicast; the broadcasts still go.
        // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
        let ip = unsafe { opt_cstr(last_known_ip) }
            .ok()
            .flatten()
            .and_then(|s| s.parse::<std::net::Ipv4Addr>().ok());
        match punktfunk_core::wol::send_magic_packet(&mac_vec, ip) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(_) => PunktfunkStatus::Io,
        }
    })
}

/// Read an optional NUL-terminated string: `Ok(None)` for null, `Err` for invalid UTF-8.
///
/// # Safety
/// `p` is null or a NUL-terminated string that outlives `'a` and is not written meanwhile.
unsafe fn opt_cstr<'a>(p: *const std::os::raw::c_char) -> std::result::Result<Option<&'a str>, ()> {
    if p.is_null() {
        return Ok(None);
    }
    // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map(Some)
        .map_err(|_| ())
}

#[cfg(test)]
mod abi_version_tests {
    /// Pin [`punktfunk_core::ABI_VERSION`]. A bump must update this test in the same change.
    #[test]
    fn abi_version_is_pinned() {
        // Current ABI. A bump must update this pin.
        assert_eq!(punktfunk_core::ABI_VERSION, 43);
        assert_eq!(super::punktfunk_abi_version(), 43);
    }

    /// The library writes this whole into the caller's buffer; growing it bumps the ABI.
    #[cfg(all(feature = "quic", target_pointer_width = "64"))]
    #[test]
    fn probe_result_size_is_pinned() {
        assert_eq!(std::mem::size_of::<super::PunktfunkProbeResult>(), 72);
    }

    #[test]
    fn ffi_slice_extents_must_fit_isize() {
        assert_eq!(super::ffi_slice_bytes::<u8>(0), Some(0));
        assert_eq!(
            super::ffi_slice_bytes::<u8>(isize::MAX as usize),
            Some(isize::MAX as usize)
        );
        assert_eq!(super::ffi_slice_bytes::<u8>(isize::MAX as usize + 1), None);
        assert_eq!(super::ffi_slice_bytes::<[u8; 6]>(usize::MAX / 6 + 1), None);
    }

    #[test]
    fn wake_rejects_an_unrepresentable_mac_array_before_reading_it() {
        let pointer = std::ptr::NonNull::<u8>::dangling().as_ptr();
        // SAFETY: an unrepresentable count is rejected before `pointer` is read, so the
        // readable-region precondition does not apply.
        let status =
            unsafe { super::punktfunk_wake_on_lan(pointer, usize::MAX / 6 + 1, std::ptr::null()) };
        assert_eq!(status, punktfunk_core::error::PunktfunkStatus::InvalidArg);
    }
}

#[cfg(test)]
mod log_sink_tests {
    use super::*;
    use std::sync::Mutex;

    /// `(level, target, message, user token)` per delivered line. The collector
    /// asserts nothing (`extern "C"` must not panic); the test body checks what landed.
    static LINES: Mutex<Vec<(u8, String, String, usize)>> = Mutex::new(Vec::new());

    unsafe extern "C" fn collect(
        level: u8,
        target: *const c_char,
        message: *const c_char,
        user: *mut c_void,
    ) {
        // SAFETY: the core hands NUL-terminated strings valid for this call, per the callback contract.
        let (t, m) = unsafe { (CStr::from_ptr(target), CStr::from_ptr(message)) };
        lock_recover(&LINES).push((
            level,
            t.to_string_lossy().into_owned(),
            m.to_string_lossy().into_owned(),
            user as usize,
        ));
    }

    /// A `log` record and a `tracing` event reach the C callback with level, target,
    /// message and user token; an interior NUL is dropped; the level ceiling is
    /// honoured; NULL detaches.
    #[test]
    fn callback_receives_log_and_tracing_lines() {
        // SAFETY: `collect` is a valid fn for the life of the test binary; the user token is an
        // opaque integer.
        let st = unsafe { punktfunk_set_log_callback(3, Some(collect), 0x5151 as *mut c_void) };
        assert_eq!(st, PunktfunkStatus::Ok);

        log::warn!(target: "quinn::connection", "handshake \0 done");
        tracing::info!(target: "punktfunk_core::transport", buf = 4096, "socket buffer clamped");
        log::debug!(target: "quinn::connection", "must not arrive (above the ceiling)");

        let lines = lock_recover(&LINES).clone();
        let warn = lines
            .iter()
            .find(|l| l.1 == "quinn::connection")
            .expect("log record delivered");
        assert_eq!(warn.0, 2);
        assert_eq!(warn.2, "handshake  done", "interior NUL dropped, line kept");
        assert_eq!(warn.3, 0x5151, "the user token must come back unchanged");
        let info = lines
            .iter()
            .find(|l| l.1 == "punktfunk_core::transport")
            .expect("tracing event delivered through the log bridge");
        assert_eq!(info.0, 3);
        assert!(
            info.2.contains("socket buffer clamped") && info.2.contains("buf=4096"),
            "{}",
            info.2
        );
        assert!(!lines.iter().any(|l| l.2.contains("must not arrive")));

        // SAFETY: NULL callback detaches; no pointer is retained.
        let detached = unsafe { punktfunk_set_log_callback(3, None, ptr::null_mut()) };
        assert_eq!(detached, PunktfunkStatus::Ok);
        // By this line, not by a count: the sink is process-wide, so any
        // other test emitting while it was attached grows the same vector.
        log::error!(target: "quinn::connection", "after detach");
        assert!(
            !lock_recover(&LINES).iter().any(|l| l.2 == "after detach"),
            "a detached sink hears nothing"
        );
    }
}

#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::*;

    #[test]
    fn null_session_and_connection_handles_return_status() {
        // SAFETY: null handles are the documented reported-not-UB case.
        let session = unsafe { punktfunk_get_stats(std::ptr::null_mut(), std::ptr::null_mut()) };
        assert_eq!(session, PunktfunkStatus::NullPointer);

        // SAFETY: null handles are the documented reported-not-UB case.
        let connection = unsafe {
            punktfunk_connection_audio_channels(std::ptr::null_mut(), std::ptr::null_mut())
        };
        assert_eq!(connection, PunktfunkStatus::NullPointer);
    }

    /// A string plus NUL fills exactly `cap`; one byte less writes nothing. Null slots are skipped.
    #[test]
    fn out_param_helpers_respect_capacity_and_null() {
        let mut buf = [0x5a as c_char; 4];
        // SAFETY: `buf` is writable for the 3- and 4-byte capacities passed; null is allowed.
        unsafe {
            assert!(!write_cstr(buf.as_mut_ptr(), 3, "abc"));
            assert_eq!(buf, [0x5a; 4]);
            assert!(write_cstr(buf.as_mut_ptr(), 4, "abc"));
            assert!(!write_cstr(ptr::null_mut(), 4, "abc"));
        }
        assert_eq!(buf.map(|b| b.to_ne_bytes()[0]), *b"abc\0");

        // Byte 0 is a sentinel: the fingerprint lands at offset 1 and fills exactly 32 bytes.
        let mut fp = [0u8; 33];
        // SAFETY: `fp[1..]` is 32 writable bytes; null is allowed for both helpers.
        unsafe {
            put_sha256(fp.as_mut_ptr().add(1), [7; 32]);
            put_sha256(ptr::null_mut(), [7; 32]);
            put(ptr::null_mut::<u32>(), 1);
        }
        assert_eq!(fp[0], 0);
        assert_eq!(fp[1..], [7; 32]);
    }

    #[test]
    fn guard_maps_panics_to_panic_status() {
        assert_eq!(
            guard(|| panic!("test panic must stay inside the ABI guard")),
            PunktfunkStatus::Panic
        );
    }
}
