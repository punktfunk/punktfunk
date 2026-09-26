//! Stable `extern "C"` surface. `cbindgen` emits `include/punktfunk_core.h`
//! (`build.rs`). Pin with [`punktfunk_abi_version`] and `struct_size`.
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

// Crate-denied `unsafe_code` (lib.rs). This `extern "C"` surface is a carve-out;
// every `unsafe` site has a proof.
#![allow(unsafe_code)]

use crate::config::{Config, FecConfig, FecScheme, ProtocolPhase, Role};
use crate::crypto::SessionKey;
use crate::error::PunktfunkStatus;
use crate::input::InputEvent;
use crate::reanchor::{GateVerdict, ReanchorGate};
use crate::session::Session;
use crate::stats::Stats;
use crate::transport::{loopback_pair, Transport, UdpTransport};
use pf_bitstream::h265::conceal::{Concealment, H265Concealer};
use std::ffi::{c_void, CStr};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;
use std::ptr;

/// Recover a poisoned mutex. Slots are last-value caches; a poisoned writer
/// still left structurally valid data.
fn lock_recover<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Opaque session handle. C sees only the pointer.
pub struct PunktfunkSession {
    inner: Session,
    /// Last polled frame. [`PunktfunkFrame::data`] is valid until the next poll/free.
    last_frame: Option<crate::session::Frame>,
    input_cb: Option<(PunktfunkInputCb, *mut c_void)>,
}

/// Session configuration. Set `struct_size` to `sizeof(PunktfunkConfig)`; a
/// smaller prefix is rejected rather than over-read.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkConfig {
    pub struct_size: u32,
    /// 0 = host, 1 = client.
    pub role: u32,
    /// 1 = P1 (GameStream-compatible), 2 = P2 (`punktfunk/1`).
    pub phase: u32,
    /// 0 = GF(2⁸), 1 = GF(2¹⁶).
    pub fec_scheme: u32,
    pub fec_percent: u32,
    pub max_data_per_block: u32,
    pub shard_payload: u32,
    /// Non-zero enables AES-128-GCM.
    pub encrypt: u32,
    pub key: [u8; 16],
    pub salt: [u8; 4],
    /// Test hook for the loopback transport; 0 in production.
    pub loopback_drop_period: u32,
    /// Largest encoded access unit the receiver accepts (reassembler memory bound).
    pub max_frame_bytes: u64,
}

impl PunktfunkConfig {
    fn to_config(self) -> Result<Config, PunktfunkStatus> {
        let role = match self.role {
            0 => Role::Host,
            1 => Role::Client,
            _ => return Err(PunktfunkStatus::InvalidArg),
        };
        let phase = match self.phase {
            1 => ProtocolPhase::P1GameStream,
            2 => ProtocolPhase::P2Punktfunk,
            _ => return Err(PunktfunkStatus::InvalidArg),
        };
        // Reject before narrowing: 300% or a 65600-shard block must not wrap to a valid u8/u16.
        let scheme = u8::try_from(self.fec_scheme)
            .ok()
            .and_then(FecScheme::from_u8)
            .ok_or(PunktfunkStatus::InvalidArg)?;
        let fec_percent =
            u8::try_from(self.fec_percent).map_err(|_| PunktfunkStatus::InvalidArg)?;
        let max_data_per_block =
            u16::try_from(self.max_data_per_block).map_err(|_| PunktfunkStatus::InvalidArg)?;
        // 32-bit: `as usize` truncates >4 GiB to a residue that still passes `validate()`.
        let max_frame_bytes =
            usize::try_from(self.max_frame_bytes).map_err(|_| PunktfunkStatus::InvalidArg)?;
        let cfg = Config {
            role,
            phase,
            fec: FecConfig {
                scheme,
                fec_percent,
                max_data_per_block,
            },
            shard_payload: self.shard_payload as usize,
            max_frame_bytes,
            encrypt: self.encrypt != 0,
            // 16-byte key is AES-128-GCM. A different cipher needs an ABI bump.
            key: SessionKey::Aes128Gcm(self.key),
            salt: self.salt,
            loopback_drop_period: self.loopback_drop_period,
        };
        cfg.validate().map_err(|e| e.status())?;
        Ok(cfg)
    }
}

/// Read `struct_size` first so a smaller older layout is rejected, not over-read.
///
/// # Safety
/// `cfg` is null or points to at least its declared `struct_size` bytes.
unsafe fn config_from_ptr(cfg: *const PunktfunkConfig) -> Result<Config, PunktfunkStatus> {
    if cfg.is_null() {
        return Err(PunktfunkStatus::NullPointer);
    }
    // SAFETY: `addr_of!` does not form a `&`; the caller may have a smaller older layout.
    let declared = unsafe { std::ptr::addr_of!((*cfg).struct_size).read_unaligned() } as usize;
    if declared < std::mem::size_of::<PunktfunkConfig>() {
        return Err(PunktfunkStatus::InvalidArg);
    }
    // SAFETY: `cfg` is non-null and `struct_size` covers this type.
    unsafe { *cfg }.to_config()
}

/// Reassembled access unit. `data`/`len` borrow session memory until the next
/// `punktfunk_client_poll_frame` / `punktfunk_session_free` on this session.
#[repr(C)]
pub struct PunktfunkFrame {
    pub data: *const u8,
    pub len: usize,
    pub frame_index: u32,
    pub pts_ns: u64,
    pub flags: u32,
    /// Reassembly-complete instant, ns since Unix epoch (`CLOCK_REALTIME`, same
    /// clock as `pts_ns`). A stamp at poll return includes pre-decode queue wait.
    pub received_ns: u64,
}

/// Session counters.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PunktfunkStats {
    pub frames_submitted: u64,
    pub frames_completed: u64,
    pub frames_dropped: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub packets_dropped: u64,
    /// Host send-path drops (`WouldBlock`). Distinct from recv-side `packets_dropped`.
    pub packets_send_dropped: u64,
    pub fec_recovered_shards: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
}

impl From<Stats> for PunktfunkStats {
    fn from(s: Stats) -> Self {
        PunktfunkStats {
            frames_submitted: s.frames_submitted,
            frames_completed: s.frames_completed,
            frames_dropped: s.frames_dropped,
            packets_sent: s.packets_sent,
            packets_received: s.packets_received,
            packets_dropped: s.packets_dropped,
            packets_send_dropped: s.packets_send_dropped,
            fec_recovered_shards: s.fec_recovered_shards,
            bytes_sent: s.bytes_sent,
            bytes_received: s.bytes_received,
        }
    }
}

/// Host-side callback for each input event drained by `punktfunk_host_poll_input`.
pub type PunktfunkInputCb = extern "C" fn(event: *const InputEvent, user: *mut c_void);

#[inline]
fn guard<F: FnOnce() -> PunktfunkStatus>(f: F) -> PunktfunkStatus {
    std::panic::catch_unwind(AssertUnwindSafe(f)).unwrap_or(PunktfunkStatus::Panic)
}

fn ffi_slice_bytes<T>(len: usize) -> Option<usize> {
    len.checked_mul(std::mem::size_of::<T>())
        .filter(|&bytes| bytes <= isize::MAX as usize)
}

/// [`guard`] for teardown with no status: swallow the panic. Unwinding into C
/// aborts the embedder; the object is being dropped either way.
fn guard_void<F: FnOnce()>(f: F) {
    if std::panic::catch_unwind(AssertUnwindSafe(f)).is_err() {
        tracing::error!("panic escaped a punktfunk_* teardown entry point; swallowed at the C ABI");
    }
}

fn new_handle(session: Session) -> *mut PunktfunkSession {
    Box::into_raw(Box::new(PunktfunkSession {
        inner: session,
        last_frame: None,
        input_cb: None,
    }))
}

/// Current ABI version. Mismatch with [`crate::ABI_VERSION`] is an incompatible core.
#[unsafe(no_mangle)]
pub extern "C" fn punktfunk_abi_version() -> u32 {
    crate::ABI_VERSION
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

#[derive(Clone, Copy)]
struct LogSink {
    cb: unsafe extern "C" fn(u8, *const c_char, *const c_char, *mut c_void),
    user: *mut c_void,
}
// SAFETY: `user` is an opaque token handed back to the caller's thread-safe callback; never deref'd.
unsafe impl Send for LogSink {}

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
                sink.user,
            )
        };
    }

    fn flush(&self) {}
}

/// Route core `log`/`tracing` lines to `cb`. `max_level` 1..=5 (error…trace), 0 = off.
/// 3 (info) is the usual default; debug/trace is per-packet. `cb == NULL` detaches.
/// `Unsupported` if another `log` backend is already installed. Idempotent.
///
/// # Safety
/// Non-null `cb` stays valid until the next NULL call; `user` stays valid for every callback.
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
        *lock_recover(&LOG_SINK) = cb.map(|cb| LogSink { cb, user });
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
/// `last_known_ip` is an optional IPv4 dotted-quad unicast target. Broadcasts
/// subnet-directed and `255.255.255.255` on ports 9 and 7. No session needed.
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
        let Some(byte_len) = ffi_slice_bytes::<crate::wol::Mac>(mac_count) else {
            return PunktfunkStatus::InvalidArg;
        };
        if byte_len == 0 {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: `ffi_slice_bytes` proved `mac_count` MACs fit a Rust slice; borrowed for this call.
        let bytes = unsafe { std::slice::from_raw_parts(macs, byte_len) };
        let mac_vec: Vec<crate::wol::Mac> = bytes
            .chunks_exact(6)
            .map(|c| {
                let mut m = [0u8; 6];
                m.copy_from_slice(c);
                m
            })
            .collect();
        let ip = if last_known_ip.is_null() {
            None
        } else {
            // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
            match unsafe { CStr::from_ptr(last_known_ip) }
                .to_str()
                .ok()
                .and_then(|s| s.parse::<std::net::Ipv4Addr>().ok())
            {
                Some(ip) => Some(ip),
                None => return PunktfunkStatus::InvalidArg,
            }
        };
        match crate::wol::send_magic_packet(&mac_vec, ip) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(_) => PunktfunkStatus::Io,
        }
    })
}

/// Create a session over UDP (`local`/`peer` are `host:port` strings). NULL on error.
///
/// # Safety
/// `cfg`, `local`, `peer` are valid pointers; the strings are NUL-terminated.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_session_new(
    cfg: *const PunktfunkConfig,
    local: *const c_char,
    peer: *const c_char,
) -> *mut PunktfunkSession {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if cfg.is_null() || local.is_null() || peer.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: pointers are caller-supplied and null-checked on this path.
        let config = match unsafe { config_from_ptr(cfg) } {
            Ok(c) => c,
            Err(_) => return ptr::null_mut(),
        };
        // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
        let local = match unsafe { CStr::from_ptr(local) }.to_str() {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        };
        // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
        let peer = match unsafe { CStr::from_ptr(peer) }.to_str() {
            Ok(s) => s,
            Err(_) => return ptr::null_mut(),
        };
        let transport: Box<dyn Transport> = match UdpTransport::connect(local, peer) {
            Ok(t) => Box::new(t),
            Err(_) => return ptr::null_mut(),
        };
        match Session::new(config, transport) {
            Ok(s) => new_handle(s),
            Err(_) => ptr::null_mut(),
        }
    }));
    result.unwrap_or(ptr::null_mut())
}

/// Connected host+client pair on in-process loopback. Test/dev only: full FEC
/// + framing without a network.
///
/// # Safety
/// All four pointers are valid; the two out-params receive owned handles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_test_loopback_pair(
    host_cfg: *const PunktfunkConfig,
    client_cfg: *const PunktfunkConfig,
    out_host: *mut *mut PunktfunkSession,
    out_client: *mut *mut PunktfunkSession,
) -> PunktfunkStatus {
    guard(|| {
        if host_cfg.is_null() || client_cfg.is_null() || out_host.is_null() || out_client.is_null()
        {
            return PunktfunkStatus::NullPointer;
        }
        // SAFETY: pointers are caller-supplied and null-checked on this path.
        let hconf = match unsafe { config_from_ptr(host_cfg) } {
            Ok(c) => c,
            Err(s) => return s,
        };
        // SAFETY: pointers are caller-supplied and null-checked on this path.
        let cconf = match unsafe { config_from_ptr(client_cfg) } {
            Ok(c) => c,
            Err(s) => return s,
        };
        let (ht, ct) = loopback_pair(hconf.loopback_drop_period, cconf.loopback_drop_period);
        let hs = match Session::new(hconf, Box::new(ht)) {
            Ok(s) => s,
            Err(e) => return e.status(),
        };
        let cs = match Session::new(cconf, Box::new(ct)) {
            Ok(s) => s,
            Err(e) => return e.status(),
        };
        // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
        unsafe {
            *out_host = new_handle(hs);
            *out_client = new_handle(cs);
        }
        PunktfunkStatus::Ok
    })
}

/// Free a session handle. NULL is a no-op.
///
/// # Safety
/// `s` is a handle from `punktfunk_session_new` / `punktfunk_test_loopback_pair`, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_session_free(s: *mut PunktfunkSession) {
    guard_void(|| {
        if !s.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(s) });
        }
    });
}

/// Host: FEC-protect, packetize, seal, and send one encoded access unit.
///
/// # Safety
/// `s` is a valid host handle. For a representable nonzero `len`, `data` points
/// to that many readable bytes; `data` may be NULL when `len == 0`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_host_submit_frame(
    s: *mut PunktfunkSession,
    data: *const u8,
    len: usize,
    pts_ns: u64,
    flags: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer,
        };
        if data.is_null() && len != 0 {
            return PunktfunkStatus::NullPointer;
        }
        if ffi_slice_bytes::<u8>(len).is_none() {
            return PunktfunkStatus::InvalidArg;
        }
        let slice = if len == 0 {
            &[][..]
        } else {
            // SAFETY: `ffi_slice_bytes` proved `len` bytes fit a Rust slice; borrowed for this call.
            unsafe { std::slice::from_raw_parts(data, len) }
        };
        match s.inner.submit_frame(slice, pts_ns, flags) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Client: poll for the next reassembled access unit. [`PunktfunkStatus::NoFrame`]
/// when nothing is ready. On `Ok`, `*out` borrows session memory until the next poll.
///
/// # Safety
/// `s` is a valid client handle; `out` points to a writable `PunktfunkFrame`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_client_poll_frame(
    s: *mut PunktfunkSession,
    out: *mut PunktfunkFrame,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match s.inner.poll_frame() {
            Ok(frame) => {
                let f = s.last_frame.insert(frame);
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkFrame {
                        data: f.data.as_ptr(),
                        len: f.data.len(),
                        frame_index: f.frame_index,
                        pts_ns: f.pts_ns,
                        flags: f.flags,
                        received_ns: f.received_ns,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Client: serialize and send one input event to the host.
/// `InvalidArg` if `ev->kind` is not a recognized event kind.
///
/// # Safety
/// `s` is a valid client handle; `ev` points to a readable `InputEvent`-sized allocation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_send_input(
    s: *mut PunktfunkSession,
    ev: *const InputEvent,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: `read_input_event` validates the tag before forming `&InputEvent` (else UB).
        let ev = match unsafe { read_input_event(ev) } {
            Ok(e) => e,
            Err(status) => return status,
        };
        match s.inner.send_input(ev) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Validate the `kind` tag as a raw byte before forming `&InputEvent`. An
/// unknown `ev->kind` is UB once the typed reference exists; other fields are integers.
///
/// # Safety
/// `ev` is null (status) or readable for `size_of::<InputEvent>()` bytes.
unsafe fn read_input_event<'a>(ev: *const InputEvent) -> Result<&'a InputEvent, PunktfunkStatus> {
    if ev.is_null() {
        return Err(PunktfunkStatus::NullPointer);
    }
    // SAFETY: non-null, readable; a one-byte read of the leading `kind` tag is valid for any value.
    if crate::input::InputKind::from_u8(unsafe { ev.cast::<u8>().read() }).is_none() {
        return Err(PunktfunkStatus::InvalidArg);
    }
    // SAFETY: discriminant validated; remaining fields are valid for any bit pattern.
    Ok(unsafe { &*ev })
}

/// Register the host-side input callback (NULL fn pointer clears). Fires from
/// [`punktfunk_host_poll_input`] on the calling thread.
///
/// # Safety
/// `s` is a valid host handle; `user` is passed back verbatim to `cb`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_set_input_callback(
    s: *mut PunktfunkSession,
    // Explicit `Option<fn>` so cbindgen emits a nullable C function pointer, not a wrapper.
    cb: Option<extern "C" fn(event: *const InputEvent, user: *mut c_void)>,
    user: *mut c_void,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let s = match unsafe { s.as_mut() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer,
        };
        s.input_cb = cb.map(|c| (c, user));
        PunktfunkStatus::Ok
    })
}

/// Host: drain pending input events, invoking the registered callback for each.
/// Returns the count dispatched (≥ 0), or a negative [`PunktfunkStatus`] on error.
///
/// # Safety
/// `s` is a valid host handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_host_poll_input(s: *mut PunktfunkSession) -> i32 {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut count = 0i32;
        loop {
            // Drop the `&mut` before the callback: it may re-enter this handle (noalias UB).
            // Re-read `input_cb` each iteration so a mid-drain NULL clear takes effect now.
            let (ev, cb) = {
                // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
                let s = match unsafe { s.as_mut() } {
                    Some(s) => s,
                    None => return PunktfunkStatus::NullPointer as i32,
                };
                match s.inner.poll_input() {
                    Ok(Some(ev)) => (ev, s.input_cb),
                    Ok(None) => break,
                    Err(e) => return e.status() as i32,
                }
            };
            if let Some((cb, user)) = cb {
                cb(&ev as *const InputEvent, user);
            }
            count += 1;
        }
        count
    }));
    r.unwrap_or(PunktfunkStatus::Panic as i32)
}

/// Copy session counters into `*out`.
///
/// # Safety
/// `s` is a valid handle; `out` points to a writable `PunktfunkStats`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_get_stats(
    s: *mut PunktfunkSession,
    out: *mut PunktfunkStats,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let s = match unsafe { s.as_ref() } {
            Some(s) => s,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let stats = s.inner.stats();
        // SAFETY: `out` is non-null on this path; written once by value.
        unsafe { *out = PunktfunkStats::from(stats) };
        PunktfunkStatus::Ok
    })
}

// `quic` client connector. Header symbols sit under `PUNKTFUNK_FEATURE_QUIC`.

/// Live `punktfunk/1` connection (QUIC control + UDP data, pumped on internal threads).
/// One puller thread per plane; never two threads on the same plane.
#[cfg(feature = "quic")]
pub struct PunktfunkConnection {
    inner: crate::client::NativeClient,
    /// Last `next_au` payload. Pointer valid until the next video pull.
    last: std::sync::Mutex<Option<crate::session::Frame>>,
    /// Last `next_audio` payload. Independent of the video slot.
    last_audio: std::sync::Mutex<Option<crate::client::AudioPacket>>,
    /// In-core PCM decode. Returned pointer valid until the next PCM call.
    audio_pcm: std::sync::Mutex<AudioPcmState>,
    /// Last clipboard payload. Pointer valid until the next `next_clipboard`.
    last_clip: std::sync::Mutex<Option<Vec<u8>>>,
    /// Last cursor RGBA. Pointer valid until the next cursor-shape call.
    last_cursor_shape: std::sync::Mutex<Option<crate::quic::CursorShape>>,
    /// Stats overlay window as last drained; `hud_text` formats it at any tier.
    hud_snap: std::sync::Mutex<crate::hud::StatsSnapshot>,
}

/// Handshake-resolved audio format. Codec + rate together distinguish 48 kHz
/// PCM from 48 kHz Opus. Read fresh each call; the host never changes it live.
#[cfg(feature = "quic")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct AudioFormat {
    /// `AUDIO_CODEC_OPUS` (`0xC9`) or `AUDIO_CODEC_PCM` (`0xD3`). Selects the decoder.
    codec: u8,
    /// 48 000 on Opus; any [`crate::audio::pcm::rate_is_supported`] rate on PCM.
    rate_hz: u32,
    /// 16 or 24. PCM unpack stride. Unused on Opus (decodes to f32).
    bits: u8,
    /// After [`crate::audio::normalize_channels`].
    channels: u8,
    /// Frame length in µs. Concealment cap is a duration; this turns it into a frame count.
    frame_us: u32,
    /// Surround coupling, verbatim off Welcome ([`crate::audio::AudioLayout`] wire id).
    layout: u8,
}

#[cfg(feature = "quic")]
impl AudioFormat {
    fn of(c: &crate::client::NativeClient) -> AudioFormat {
        AudioFormat {
            codec: c.audio_codec,
            // Zero rate sizes a 0-length buffer (`!pcm.is_empty()` is the sized latch)
            // and reallocates every packet. Do not trust a peer 0.
            rate_hz: if c.audio_sample_rate_hz == 0 {
                crate::audio::SAMPLE_RATE_HZ
            } else {
                c.audio_sample_rate_hz
            },
            bits: c.audio_bits,
            channels: crate::audio::normalize_channels(c.audio_channels),
            // 0 is not a frame length. Fold to the 5 ms Opus frame.
            frame_us: if c.audio_frame_us == 0 {
                crate::audio::FRAME_MS * 1000
            } else {
                c.audio_frame_us as u32
            },
            layout: c.audio_layout,
        }
    }

    /// True when this session runs the lossless `0xD3` plane rather than Opus on `0xC9`.
    fn is_pcm(&self) -> bool {
        self.codec == crate::quic::AUDIO_CODEC_PCM
    }
}

/// In-core decode for either audio plane. Opus uses one [`opus::MSDecoder`];
/// PCM unpacks with [`crate::audio::pcm::to_f32`] and conceals with [`PcmConceal`].
#[cfg(feature = "quic")]
#[derive(Default)]
struct AudioPcmState {
    decoder: Option<opus::MSDecoder>,
    /// Interleaved f32. Sized once; growth would dangle the pointer handed to the embedder.
    pcm: Vec<f32>,
    /// Seq-gap tracker. Without it a lost packet is a hard click in the playout ring.
    gaps: crate::audio::AudioGapTracker,
    /// Last real decode's per-channel samples. 0 skips concealment (nothing to size from).
    frame_samples: usize,
    /// PLC frames already given during a drought. Subtract so a later gap is not covered twice.
    drought_frames: u32,
    /// PCM-plane concealer. Unused on Opus (libopus PLC).
    conceal_pcm: crate::audio::pcm::PcmConceal,
    /// PCM staging. `to_f32` clears/reserves; writing into `pcm` would move the embedder pointer.
    scratch_pcm: Vec<f32>,
}

#[cfg(feature = "quic")]
impl AudioPcmState {
    /// Size `pcm` once. The embedder holds a pointer into it until the next PCM
    /// call; later `Vec` growth would dangle. Writes clamp; never extend.
    /// `!pcm.is_empty()` is the sized latch — a zero rate would re-enter forever.
    fn ensure_buffer(&mut self, fmt: AudioFormat) {
        if !self.pcm.is_empty() {
            return;
        }
        let ch = fmt.channels.max(1) as usize;
        // Largest frame this plane can present. Opus: 120 ms. PCM: longest `FRAME_US_LADDER`
        // rung (MTU-sized, never fragmented). Copies into `pcm` still clamp.
        let per_ch = if fmt.is_pcm() {
            crate::audio::pcm::samples_per_frame(
                fmt.rate_hz,
                crate::audio::pcm::FRAME_US_LADDER[0],
                1,
            )
        } else {
            fmt.rate_hz as usize / 1000 * 120
        };
        // Count from resolved frame (50 ms cap → 25×2 ms, not 10). Size from the longest
        // ladder rung: a 2 ms session that then sends 5 ms must not truncate; cannot grow.
        let run = crate::audio::max_conceal_packets(fmt.frame_us) as usize;
        self.pcm = vec![0f32; (1 + run) * per_ch.max(1) * ch];
        // Cap the tracker at this same run. A larger cap would silently truncate frames.
        self.gaps.set_frame_us(fmt.frame_us);
    }

    /// Copy `n` scratch samples into `pcm` at `filled`. Clamp is load-bearing:
    /// oversized datagrams truncate instead of reallocating or overrunning.
    fn stage_scratch(&mut self, filled: usize, n: usize) -> usize {
        let n = n.min(self.scratch_pcm.len()).min(self.pcm.len() - filled);
        self.pcm[filled..filled + n].copy_from_slice(&self.scratch_pcm[..n]);
        n
    }

    /// PCM half of [`decode_packet`](Self::decode_packet): unpack + [`PcmConceal`].
    /// Same shape as Opus (conceal then real, one buffer) so the C surface does not branch.
    fn decode_pcm_packet(
        &mut self,
        data: &[u8],
        seq: u32,
        fmt: AudioFormat,
    ) -> Result<usize, PunktfunkStatus> {
        let ch = fmt.channels.max(1) as usize;
        self.ensure_buffer(fmt);

        // Same drought credit as Opus: frames already handed out while the wire was quiet.
        let missing = self
            .gaps
            .missing_before(seq)
            .saturating_sub(std::mem::take(&mut self.drought_frames));
        let mut filled = 0usize;
        for _ in 0..missing {
            if !self.conceal_pcm.conceal(&mut self.scratch_pcm) {
                break; // nothing has arrived yet — nothing to build a repeat from
            }
            let n = self.scratch_pcm.len();
            let staged = self.stage_scratch(filled, n);
            filled += staged;
            if staged < n {
                break; // buffer full: stop rather than emit a torn frame
            }
        }

        if data.is_empty() {
            // PCM has no DTX. Empty is a torn datagram: do not `accept` it (that clears the
            // last good frame). Account the slot like Opus DTX; still emit concealment owed.
            return Ok(filled);
        }
        match crate::audio::pcm::to_f32(data, fmt.bits, &mut self.scratch_pcm) {
            Some(n) => {
                let staged = self.stage_scratch(filled, n);
                // Conceal from the staged length (what the embedder heard), not the decoded
                // one: keeps the source inside the fixed buffer on an oversized datagram.
                self.conceal_pcm.accept(&self.scratch_pcm[..staged]);
                self.frame_samples = staged / ch;
                Ok(filled + staged)
            }
            // Torn datagram: keep concealment already earned, same as undecodable Opus.
            None if filled > 0 => Ok(filled),
            None => Err(PunktfunkStatus::BadPacket),
        }
    }

    /// Decode one packet into `pcm`. Missing seqs are concealed first, then the
    /// real frame, one interleaved buffer. Empty `data` is DTX: account the slot,
    /// flush owed concealment, never decode (`decode_float` would fill the buffer).
    /// `Ok(0)` = nothing to hand out.
    fn decode_packet(
        &mut self,
        data: &[u8],
        seq: u32,
        fmt: AudioFormat,
    ) -> Result<usize, PunktfunkStatus> {
        if fmt.is_pcm() {
            return self.decode_pcm_packet(data, seq, fmt);
        }
        let channels = fmt.channels;
        let ch = channels as usize;
        if self.decoder.is_none() {
            // A coupling this build does not know would pair channels wrongly, not loudly.
            let Some(layout) = crate::audio::AudioLayout::from_wire(fmt.layout) else {
                return Err(PunktfunkStatus::Unsupported);
            };
            let layout = crate::audio::layout_for(channels, layout);
            // Negotiated rate, not a constant. libopus rejects 96 kHz; fail here, not silently.
            match opus::MSDecoder::new(fmt.rate_hz, layout.streams, layout.coupled, layout.mapping)
            {
                Ok(d) => {
                    self.ensure_buffer(fmt);
                    self.decoder = Some(d);
                }
                Err(_) => return Err(PunktfunkStatus::Unsupported),
            }
        }
        let dec = self.decoder.as_mut().unwrap();

        // PLC the seq gap first (empty input, 50 ms cap). Subtract drought frames already in the ring.
        let missing = self
            .gaps
            .missing_before(seq)
            .saturating_sub(std::mem::take(&mut self.drought_frames));
        let mut filled = 0usize;
        if self.frame_samples > 0 {
            for _ in 0..missing {
                let plc = self.frame_samples * ch;
                match dec.decode_float(&[], &mut self.pcm[filled..filled + plc], false) {
                    Ok(samples) => filled += samples * ch,
                    Err(_) => break,
                }
            }
        }

        if data.is_empty() {
            // DTX: never decode; still emit concealment owed for losses before it.
            return Ok(filled);
        }
        match dec.decode_float(data, &mut self.pcm[filled..], false) {
            Ok(samples) => {
                self.frame_samples = samples;
                Ok(filled + samples * ch)
            }
            // Undecodable: keep concealment already earned. This slot is a ring gap.
            Err(_) if filled > 0 => Ok(filled),
            Err(_) => Err(PunktfunkStatus::BadPacket),
        }
    }

    /// One drought concealment frame, no packet. `Ok(0)` before the first decode.
    fn conceal(&mut self, fmt: AudioFormat) -> Result<usize, PunktfunkStatus> {
        if fmt.is_pcm() {
            // PCM has no decoder/PLC. `conceal` is false before the first real frame.
            if !self.conceal_pcm.conceal(&mut self.scratch_pcm) {
                return Ok(0);
            }
            let n = self.scratch_pcm.len();
            let staged = self.stage_scratch(0, n);
            if staged == 0 {
                return Ok(0);
            }
            self.drought_frames = self.drought_frames.saturating_add(1);
            return Ok(staged);
        }
        let ch = fmt.channels as usize;
        let plc = self.frame_samples * ch;
        if plc == 0 {
            return Ok(0);
        }
        let Some(dec) = self.decoder.as_mut() else {
            return Ok(0);
        };
        match dec.decode_float(&[], &mut self.pcm[..plc], false) {
            Ok(samples) => {
                self.drought_frames = self.drought_frames.saturating_add(1);
                Ok(samples * ch)
            }
            // libopus declined; write nothing (same as a timeout).
            Err(_) => Ok(0),
        }
    }
}

/// `PunktfunkHidOutput::kind`: lightbar RGB (`r`/`g`/`b` valid).
pub const PUNKTFUNK_HIDOUT_LED: u8 = 1;
/// `PunktfunkHidOutput::kind`: player-indicator LEDs (`player_bits` valid, low 5 bits).
pub const PUNKTFUNK_HIDOUT_PLAYER_LEDS: u8 = 2;
/// `PunktfunkHidOutput::kind`: one adaptive-trigger effect (`which` + `effect`/`effect_len` valid).
pub const PUNKTFUNK_HIDOUT_TRIGGER: u8 = 3;
/// Trackpad haptic pulse. `which` = side (0 right, 1 left); `effect[0..6]` =
/// amplitude/period/count as LE `u16`, `effect_len = 6`. Drop if no coils.
pub const PUNKTFUNK_HIDOUT_TRACKPAD_HAPTIC: u8 = 4;
/// DS5 audio-control region. Samples arrive via [`punktfunk_connection_next_pad_audio`].
/// `which` = flags; `effect[0..6]` = report bytes 5..=10; `effect_len = 6`. Change-only.
pub const PUNKTFUNK_HIDOUT_AUDIO_CTL: u8 = 5;
/// Raw hidraw report to replay (`HidRaw`). `hid_kind` + `raw`/`raw_len` valid.
/// Only `PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2` emits these; others drop.
pub const PUNKTFUNK_HIDOUT_HID_RAW: u8 = 6;
/// Capacity of `PunktfunkHidOutput::effect` (DualSense trigger parameter block).
pub const PUNKTFUNK_HID_EFFECT_MAX: u8 = 11;

/// HID-output feedback from the host virtual pad ([`punktfunk_connection_next_hidout`]).
/// `kind` selects which fields to replay on the physical controller.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHidOutput {
    /// One of `PUNKTFUNK_HIDOUT_*`.
    pub kind: u8,
    /// Gamepad index.
    pub pad: u8,
    /// LED: lightbar red.
    pub r: u8,
    /// LED: lightbar green.
    pub g: u8,
    /// LED: lightbar blue.
    pub b: u8,
    /// PlayerLeds: lit player indicators (low 5 bits).
    pub player_bits: u8,
    /// Trigger: 0 = L2, 1 = R2.
    pub which: u8,
    /// Trigger: number of valid bytes in `effect` (≤ `PUNKTFUNK_HID_EFFECT_MAX`).
    pub effect_len: u8,
    /// DualSense trigger parameter block. Length is [`PUNKTFUNK_HID_EFFECT_MAX`], not a second `11`.
    pub effect: [u8; PUNKTFUNK_HID_EFFECT_MAX as usize],
    /// HidRaw channel: `PUNKTFUNK_HID_RAW_OUTPUT` or `PUNKTFUNK_HID_RAW_FEATURE`.
    pub hid_kind: u8,
    /// HidRaw: number of valid bytes in `raw` (≤ `PUNKTFUNK_HID_REPORT_MAX`).
    pub raw_len: u8,
    /// HidRaw report, id byte first. Feature frames may be zero-padded; sized off `HID_REPORT_MAX`.
    pub raw: [u8; crate::quic::HID_REPORT_MAX],
}

#[cfg(feature = "quic")]
impl PunktfunkHidOutput {
    /// Map every [`HidOutput`](crate::quic::HidOutput) variant. `HidRaw` uses `raw`/`hid_kind`.
    fn from_hid(h: &crate::quic::HidOutput) -> PunktfunkHidOutput {
        use crate::quic::HidOutput;
        let mut out = PunktfunkHidOutput {
            kind: 0,
            pad: 0,
            r: 0,
            g: 0,
            b: 0,
            player_bits: 0,
            which: 0,
            effect_len: 0,
            effect: [0u8; 11],
            hid_kind: 0,
            raw_len: 0,
            raw: [0u8; crate::quic::HID_REPORT_MAX],
        };
        match h {
            HidOutput::Led { pad, r, g, b } => {
                out.kind = PUNKTFUNK_HIDOUT_LED;
                out.pad = *pad;
                out.r = *r;
                out.g = *g;
                out.b = *b;
            }
            HidOutput::PlayerLeds { pad, bits } => {
                out.kind = PUNKTFUNK_HIDOUT_PLAYER_LEDS;
                out.pad = *pad;
                out.player_bits = *bits;
            }
            HidOutput::Trigger { pad, which, effect } => {
                out.kind = PUNKTFUNK_HIDOUT_TRIGGER;
                out.pad = *pad;
                out.which = *which;
                let n = effect.len().min(out.effect.len());
                out.effect[..n].copy_from_slice(&effect[..n]);
                out.effect_len = n as u8;
            }
            HidOutput::TrackpadHaptic {
                pad,
                side,
                amplitude,
                period,
                count,
            } => {
                // No size guard: pack into `which` + `effect[0..6]` LE, `effect_len = 6`.
                out.kind = PUNKTFUNK_HIDOUT_TRACKPAD_HAPTIC;
                out.pad = *pad;
                out.which = *side;
                out.effect[0..2].copy_from_slice(&amplitude.to_le_bytes());
                out.effect[2..4].copy_from_slice(&period.to_le_bytes());
                out.effect[4..6].copy_from_slice(&count.to_le_bytes());
                out.effect_len = 6;
            }
            HidOutput::HidRaw { pad, kind, data } => {
                out.kind = PUNKTFUNK_HIDOUT_HID_RAW;
                out.pad = *pad;
                out.hid_kind = *kind;
                // `decode` already bounds; clamp so a local oversize cannot overrun `raw`.
                let n = data.len().min(out.raw.len());
                out.raw[..n].copy_from_slice(&data[..n]);
                out.raw_len = n as u8;
            }
            HidOutput::AudioCtl { pad, flags, raw } => {
                // Same pack as TrackpadHaptic. `pad as u8` is lossless: decode rejects ≥ MAX_PADS.
                out.kind = PUNKTFUNK_HIDOUT_AUDIO_CTL;
                out.pad = *pad as u8;
                out.which = *flags;
                out.effect[0..6].copy_from_slice(raw);
                out.effect_len = 6;
            }
        }
        out
    }
}

/// Static HDR metadata ([`punktfunk_connection_next_hdr_meta`]): ST.2086 mastering
/// display + CEA-861.3 content light. HDR10 SEI units (primaries/white 1/50000,
/// luminance 0.0001 cd/m²) for DXGI / `CAEDRMetadata` / `KEY_HDR_STATIC_INFO`.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHdrMeta {
    /// Display-primaries x-chromaticities, 1/50000 units, ST.2086 order [green, blue, red].
    pub display_primaries_x: [u16; 3],
    /// Display-primaries y-chromaticities, 1/50000 units, ST.2086 order [green, blue, red].
    pub display_primaries_y: [u16; 3],
    /// White-point x-chromaticity, 1/50000 units.
    pub white_point_x: u16,
    /// White-point y-chromaticity, 1/50000 units.
    pub white_point_y: u16,
    /// Max display mastering luminance, 0.0001 cd/m².
    pub max_display_mastering_luminance: u32,
    /// Min display mastering luminance, 0.0001 cd/m².
    pub min_display_mastering_luminance: u32,
    /// Max content light level (MaxCLL), nits. 0 = unknown.
    pub max_cll: u16,
    /// Max frame-average light level (MaxFALL), nits. 0 = unknown.
    pub max_fall: u16,
}

#[cfg(feature = "quic")]
impl PunktfunkHdrMeta {
    fn from_meta(m: &crate::quic::HdrMeta) -> PunktfunkHdrMeta {
        PunktfunkHdrMeta {
            display_primaries_x: [
                m.display_primaries[0][0],
                m.display_primaries[1][0],
                m.display_primaries[2][0],
            ],
            display_primaries_y: [
                m.display_primaries[0][1],
                m.display_primaries[1][1],
                m.display_primaries[2][1],
            ],
            white_point_x: m.white_point[0],
            white_point_y: m.white_point[1],
            max_display_mastering_luminance: m.max_display_mastering_luminance,
            min_display_mastering_luminance: m.min_display_mastering_luminance,
            max_cll: m.max_cll,
            max_fall: m.max_fall,
        }
    }
}

/// Host capture→sent duration for one AU ([`punktfunk_connection_next_host_timing`]).
/// Correlate by `pts_ns`; `network = (received + clock_offset − pts_ns) − host_us`.
/// Lost datagram: no sample. See `design/stats-unification.md`.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHostTiming {
    /// AU capture stamp (host capture clock — matches `PunktfunkFrame::pts_ns`).
    pub pts_ns: u64,
    /// Host capture→sent duration, µs.
    pub host_us: u32,
}

/// `PunktfunkRichInput::kind`: a touchpad contact (`finger`/`active`/`x`/`y` valid).
pub const PUNKTFUNK_RICH_TOUCHPAD: u8 = 1;
/// `PunktfunkRichInput::kind`: a motion sample (`gyro`/`accel` valid).
pub const PUNKTFUNK_RICH_MOTION: u8 = 2;
/// `RichInput::TouchpadEx` on the wire: surface (0 single / 1 Steam-left / 2
/// Steam-right) plus click + pressure. C send path is size-prefixed
/// `PunktfunkRichInputEx` via `punktfunk_connection_send_rich_input2`.
pub const PUNKTFUNK_RICH_TOUCHPAD_EX: u8 = 3;
/// `RichInput::HidReport` on the wire (`[0xCC][0x04][pad][len][data…]`): raw HID
/// input for the host as-is pad (`PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2`). C clients
/// send via [`punktfunk_connection_send_hid_report`], never by building the datagram.
pub const PUNKTFUNK_RICH_HID_REPORT: u8 = 4;

/// One rich client→host input for the host virtual DualSense
/// ([`punktfunk_connection_send_rich_input`]): touchpad contact or motion sample.
/// Set `kind` and the matching fields; the others are ignored.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkRichInput {
    /// One of `PUNKTFUNK_RICH_*`.
    pub kind: u8,
    /// Gamepad index.
    pub pad: u8,
    /// Touchpad: contact id (0 or 1).
    pub finger: u8,
    /// Touchpad: 1 = finger down, 0 = lifted.
    pub active: u8,
    /// Touchpad: normalized x, 0..=65535 across the touchpad.
    pub x: u16,
    /// Touchpad: normalized y, 0..=65535 across the touchpad.
    pub y: u16,
    /// Motion: gyro (pitch, yaw, roll), raw signed-16.
    pub gyro: [i16; 3],
    /// Motion: accelerometer (x, y, z), raw signed-16.
    pub accel: [i16; 3],
}

#[cfg(feature = "quic")]
impl PunktfunkRichInput {
    fn to_rich(self) -> Option<crate::quic::RichInput> {
        use crate::quic::RichInput;
        match self.kind {
            PUNKTFUNK_RICH_TOUCHPAD => Some(RichInput::Touchpad {
                pad: self.pad,
                finger: self.finger,
                active: self.active != 0,
                x: self.x,
                y: self.y,
            }),
            PUNKTFUNK_RICH_MOTION => Some(RichInput::Motion {
                pad: self.pad,
                gyro: self.gyro,
                accel: self.accel,
            }),
            _ => None,
        }
    }
}

/// Superset of [`PunktfunkRichInput`] for `TouchpadEx` (second pad, click, signed
/// coords, pressure). Set `struct_size = sizeof(PunktfunkRichInputEx)`.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkRichInputEx {
    /// Must equal `sizeof(PunktfunkRichInputEx)`.
    pub struct_size: u32,
    /// One of `PUNKTFUNK_RICH_*` (`TOUCHPAD` / `MOTION` / `TOUCHPAD_EX`).
    pub kind: u8,
    /// Gamepad index.
    pub pad: u8,
    /// Touchpad/TouchpadEx: contact id.
    pub finger: u8,
    /// Touchpad/TouchpadEx: 1 = finger down / touching, 0 = lifted.
    pub active: u8,
    /// TouchpadEx: surface — 0 = single/DualSense, 1 = Steam left pad, 2 = Steam right pad.
    pub surface: u8,
    /// TouchpadEx: 1 = the pad is physically clicked, distinct from a touch contact.
    pub click: u8,
    /// Reserved for alignment; set to 0.
    pub _reserved: [u8; 2],
    /// TouchpadEx: x, **signed**, centred at 0 (Steam report convention). For a
    /// `TOUCHPAD` kind through this struct, store the unsigned `0..=65535` bits.
    pub x: i16,
    /// TouchpadEx: y, signed, centred at 0.
    pub y: i16,
    /// TouchpadEx: contact pressure (`0` if the surface has no force sensor).
    pub pressure: u16,
    /// Motion: gyro (pitch, yaw, roll), raw signed-16.
    pub gyro: [i16; 3],
    /// Motion: accelerometer (x, y, z), raw signed-16.
    pub accel: [i16; 3],
}

#[cfg(feature = "quic")]
impl PunktfunkRichInputEx {
    fn to_rich(self) -> Option<crate::quic::RichInput> {
        use crate::quic::RichInput;
        match self.kind {
            PUNKTFUNK_RICH_TOUCHPAD_EX => Some(RichInput::TouchpadEx {
                pad: self.pad,
                surface: self.surface,
                finger: self.finger,
                touch: self.active != 0,
                click: self.click != 0,
                x: self.x,
                y: self.y,
                pressure: self.pressure,
            }),
            PUNKTFUNK_RICH_MOTION => Some(RichInput::Motion {
                pad: self.pad,
                gyro: self.gyro,
                accel: self.accel,
            }),
            PUNKTFUNK_RICH_TOUCHPAD => Some(RichInput::Touchpad {
                pad: self.pad,
                finger: self.finger,
                active: self.active != 0,
                x: self.x as u16,
                y: self.y as u16,
            }),
            _ => None,
        }
    }
}

/// [`PunktfunkPenSample::state`] bit: the pen hovers in range (implied by `TOUCHING`).
pub const PUNKTFUNK_PEN_IN_RANGE: u8 = 0x01;
/// [`PunktfunkPenSample::state`] bit: the tip is in contact.
pub const PUNKTFUNK_PEN_TOUCHING: u8 = 0x02;
/// [`PunktfunkPenSample::state`] bit: primary barrel button (or squeeze mapping) held.
pub const PUNKTFUNK_PEN_BARREL1: u8 = 0x04;
/// [`PunktfunkPenSample::state`] bit: secondary barrel button (or double-tap mapping) held.
pub const PUNKTFUNK_PEN_BARREL2: u8 = 0x08;
/// [`PunktfunkPenSample::tool`]: the pen tip.
pub const PUNKTFUNK_PEN_TOOL_PEN: u8 = 0;
/// [`PunktfunkPenSample::tool`]: the eraser. Client-side mode — no hardware eraser
/// end; squeeze/double-tap mapping usually drives this.
pub const PUNKTFUNK_PEN_TOOL_ERASER: u8 = 1;
/// Most samples one [`punktfunk_connection_send_pen`] call accepts (one wire batch).
pub const PUNKTFUNK_PEN_BATCH_MAX: u32 = 8;
/// [`PunktfunkPenSample::tilt_deg`] sentinel: no tilt reading.
pub const PUNKTFUNK_PEN_TILT_UNKNOWN: u8 = 0xFF;
/// [`PunktfunkPenSample::azimuth_deg`] / `roll_deg` sentinel: no reading.
pub const PUNKTFUNK_PEN_ANGLE_UNKNOWN: u16 = 0xFFFF;
/// [`PunktfunkPenSample::distance`] sentinel: no hover-distance reading.
pub const PUNKTFUNK_PEN_DISTANCE_UNKNOWN: u16 = 0xFFFF;

/// Full stylus state at one instant ([`punktfunk_connection_send_pen`];
/// `design/pen-tablet-input.md`). Fill every field (`*_UNKNOWN` if missing); the
/// host diffs samples. `x`/`y` are `0.0..=1.0` in video-frame space.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkPenSample {
    /// Normalized `0.0..=1.0` across the video frame. Must be finite.
    pub x: f32,
    /// Normalized `0.0..=1.0` across the video frame. Must be finite.
    pub y: f32,
    /// Tip force, `0..=65535` full scale (`0` while hovering).
    pub pressure: u16,
    /// Hover distance `0..=65534` (0 = at the hover floor), or `PUNKTFUNK_PEN_DISTANCE_UNKNOWN`.
    pub distance: u16,
    /// Tilt azimuth, degrees `0..=359` clockwise from north, or `PUNKTFUNK_PEN_ANGLE_UNKNOWN`.
    pub azimuth_deg: u16,
    /// Barrel roll (Apple Pencil Pro `rollAngle`), degrees `0..=359`, or
    /// `PUNKTFUNK_PEN_ANGLE_UNKNOWN`.
    pub roll_deg: u16,
    /// µs since the previous sample in the same call (`0` for the first) — coalesced
    /// capture spacing.
    pub dt_us: u16,
    /// Bitfield of `PUNKTFUNK_PEN_*` state bits. Unknown bits are rejected (`InvalidArg`).
    pub state: u8,
    /// `PUNKTFUNK_PEN_TOOL_PEN` or `PUNKTFUNK_PEN_TOOL_ERASER`.
    pub tool: u8,
    /// Tilt from the surface normal, degrees `0..=90`, or `PUNKTFUNK_PEN_TILT_UNKNOWN`.
    pub tilt_deg: u8,
    /// Set to 0.
    pub _reserved: [u8; 3],
}

#[cfg(feature = "quic")]
impl PunktfunkPenSample {
    /// `None` = invalid field (non-finite coordinate, unknown state bit, unknown tool).
    /// Embedder input is validated strictly, unlike the loss-tolerant wire decode.
    fn to_sample(self) -> Option<crate::quic::PenSample> {
        use crate::quic as q;
        let known = q::PEN_IN_RANGE | q::PEN_TOUCHING | q::PEN_BARREL1 | q::PEN_BARREL2;
        if !self.x.is_finite() || !self.y.is_finite() || self.state & !known != 0 {
            return None;
        }
        let tool = match self.tool {
            PUNKTFUNK_PEN_TOOL_PEN => q::PenTool::Pen,
            PUNKTFUNK_PEN_TOOL_ERASER => q::PenTool::Eraser,
            _ => return None,
        };
        Some(q::PenSample {
            state: self.state,
            tool,
            x: self.x,
            y: self.y,
            pressure: self.pressure,
            distance: self.distance,
            tilt_deg: self.tilt_deg,
            azimuth_deg: self.azimuth_deg,
            roll_deg: self.roll_deg,
            dt_us: self.dt_us,
        })
    }
}

/// Read an optional NUL-terminated UTF-8 string; `Err` = invalid pointer/UTF-8.
#[cfg(feature = "quic")]
unsafe fn opt_cstr<'a>(p: *const std::os::raw::c_char) -> std::result::Result<Option<&'a str>, ()> {
    if p.is_null() {
        return Ok(None);
    }
    // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_str()
        .map(Some)
        .map_err(|_| ())
}

/// Compositor preference for [`punktfunk_connect_ex`]. `AUTO` lets the host pick.
/// A concrete value is honored only if that backend is available now; else auto-detect.
/// Resolved choice is on `Welcome`.
pub const PUNKTFUNK_COMPOSITOR_AUTO: u32 = 0;
/// KWin / KDE Plasma.
pub const PUNKTFUNK_COMPOSITOR_KWIN: u32 = 1;
/// wlroots (Sway / River). Older clients sent it for Hyprland too; the host still honors that.
pub const PUNKTFUNK_COMPOSITOR_WLROOTS: u32 = 2;
/// Mutter / GNOME.
pub const PUNKTFUNK_COMPOSITOR_MUTTER: u32 = 3;
/// gamescope (spawned nested).
pub const PUNKTFUNK_COMPOSITOR_GAMESCOPE: u32 = 4;
/// Hyprland.
pub const PUNKTFUNK_COMPOSITOR_HYPRLAND: u32 = 5;
/// A Windows host's virtual display. Only ever resolved, never requested.
pub const PUNKTFUNK_COMPOSITOR_WINDOWS: u32 = 6;

/// Gamepad-backend preference for [`punktfunk_connect_ex2`]: which virtual pad
/// the host creates. Precedence: client choice > `PUNKTFUNK_GAMEPAD` env > X-Box 360.
/// `AUTO` (or unrecognized) = host decides. Resolved via [`punktfunk_connection_gamepad`].
pub const PUNKTFUNK_GAMEPAD_AUTO: u32 = 0;
/// uinput X-Box 360 pad (default — every game speaks XInput).
pub const PUNKTFUNK_GAMEPAD_XBOX360: u32 = 1;
/// UHID DualSense (`hid-playstation`): adaptive triggers, lightbar, touchpad, motion.
/// Feedback on [`punktfunk_connection_next_hidout`]. Linux UHID / Windows UMDF; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_DUALSENSE: u32 = 2;
/// X-Box One/Series identity on the 360 backend (glyphs). No impulse-trigger rumble:
/// evdev `FF_RUMBLE` has two magnitudes. Windows HID Xbox can; see `next_rumble_cmd2`.
pub const PUNKTFUNK_GAMEPAD_XBOXONE: u32 = 3;
/// UHID DualShock 4 (`hid-playstation`): lightbar, touchpad, motion, rumble.
/// Rich-input + HID-output planes, no adaptive triggers / player LEDs / mute.
/// Linux UHID / Windows UMDF; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_DUALSHOCK4: u32 = 4;
/// UHID classic Steam Controller (`hid-steam`): one stick + dual trackpads + two
/// grip paddles. Linux only; else Xbox 360.
pub const PUNKTFUNK_GAMEPAD_STEAMCONTROLLER: u32 = 5;
/// Steam Deck controller: four back grips, both trackpads, IMU. Steam Input
/// re-grabs with native glyphs when Steam runs on the host. Linux/Windows; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_STEAMDECK: u32 = 6;
/// DualSense Edge: DualSense plus two back buttons + two Fn buttons, so client
/// paddles land on native slots. Linux UHID / Windows UMDF; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_DUALSENSEEDGE: u32 = 7;
/// Nintendo Switch Pro: Nintendo glyphs + positional layout, gyro/accel, HD rumble.
/// Linux UHID `hid-nintendo` only; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_SWITCHPRO: u32 = 8;
/// Steam Controller 2 as-is passthrough. Linux UHID; else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2: u32 = 9;
/// Steam Controller Puck dongle: native seven-interface topology, four slots.
/// For capture clients that own the physical Puck; wired/BLE SC2 stays `STEAMCONTROLLER2`.
pub const PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2_PUCK: u32 = 10;
/// Xbox Elite identity (Windows UMDF). Identity only — paddles still fold. Else X-Box 360.
pub const PUNKTFUNK_GAMEPAD_XBOXELITE: u32 = 11;

/// Extended `InputEvent` gamepad button bits: four back grips (Steam L4/L5/R4/R5 ≙
/// Xbox-Elite P1–P4) + misc/capture, in Moonlight's `buttonFlags2 << 16` namespace.
/// Mirror `input::gamepad::BTN_PADDLE1..4` / `BTN_MISC1`.
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE1: u32 = 0x0001_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE2: u32 = 0x0002_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE3: u32 = 0x0004_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_PADDLE4: u32 = 0x0008_0000;
pub const PUNKTFUNK_GAMEPAD_BTN_MISC1: u32 = 0x0020_0000;

/// Connect to a `punktfunk/1` host at `width`x`height`@`refresh_hz`. Blocks up to
/// `timeout_ms`. NULL on failure. Same as [`punktfunk_connect_ex`] with
/// `compositor = PUNKTFUNK_COMPOSITOR_AUTO`.
///
/// Video-capability bit for [`punktfunk_connect_ex5`]: the client can decode
/// 10-bit (Main10) HEVC.
pub const PUNKTFUNK_VIDEO_CAP_10BIT: u8 = 0x01;
/// Video-capability bit: the client can present BT.2020 PQ HDR10 (implies 10-bit).
pub const PUNKTFUNK_VIDEO_CAP_HDR: u8 = 0x02;
/// Video-capability bit: the client can decode full-chroma 4:4:4 HEVC. The host
/// emits 4:4:4 only when this is set, the host opted in, the codec is HEVC, and
/// the GPU supports it — else 4:2:0. Read [`punktfunk_connection_chroma_format`].
pub const PUNKTFUNK_VIDEO_CAP_444: u8 = 0x04;

/// Codec bit for [`punktfunk_connect_ex7`] and [`punktfunk_connection_codec`]: H.264 / AVC.
pub const PUNKTFUNK_CODEC_H264: u8 = 0x01;
/// Codec bit: H.265 / HEVC — the default codec.
pub const PUNKTFUNK_CODEC_HEVC: u8 = 0x02;
/// Codec bit: AV1.
pub const PUNKTFUNK_CODEC_AV1: u8 = 0x04;
/// PyroWave. Never auto-selected; pass it as `preferred_codec` (`design/pyrowave-codec-plan.md`).
pub const PUNKTFUNK_CODEC_PYROWAVE: u8 = 0x08;

/// Host-capability bit: the host applies gamepad-state snapshots (a capable client
/// sends full-state snapshots instead of per-transition events).
pub const PUNKTFUNK_HOST_CAP_GAMEPAD_STATE: u8 = 0x01;
/// Host-capability bit: the host supports the shared clipboard; a client may offer the toggle.
pub const PUNKTFUNK_HOST_CAP_CLIPBOARD: u8 = 0x02;
/// Host injects stylus. Without it [`punktfunk_connection_send_pen`] is `Unsupported`.
/// (`design/pen-tablet-input.md`.)
pub const PUNKTFUNK_HOST_CAP_PEN: u8 = 0x10;
/// Host-capability bit: per-gamepad audio (DualSense voice-coil + speaker) on
/// the 0xD1 plane toward pads declared via [`punktfunk_connection_set_pad_audio_caps`].
/// Set only when the client asked via [`PUNKTFUNK_CLIENT_CAP_PAD_AUDIO`].
pub const PUNKTFUNK_HOST_CAP_PAD_AUDIO: u8 = 0x40;
/// Session is on lossless `0xD3`, not Opus. Distinguishes 48 kHz/16-bit PCM from
/// 48 kHz Opus when draining [`punktfunk_connection_next_audio`]. PCM decode path
/// does not need it; still read [`punktfunk_connection_audio_sample_rate`].
pub const PUNKTFUNK_HOST_CAP_AUDIO_HIRES: u8 = 0x80;

/// Host-capability bit in [`punktfunk_connection_host_caps2`] (second byte): the
/// host injector puts wire touch contacts on its desktop. Without the bit, fall
/// back to a cursor model — the host drops every contact silently.
pub const PUNKTFUNK_HOST_CAP2_TOUCH: u8 = 0x02;

/// Pad-audio `kind` ([`punktfunk_connection_next_pad_audio`]): BACK channel pair —
/// DualSense voice-coil haptics, 5 ms Opus frames.
pub const PUNKTFUNK_PAD_AUDIO_KIND_HAPTICS: u8 = 0;
/// Pad-audio `kind`: FRONT channel pair — the controller's built-in speaker, 10 ms Opus frames.
pub const PUNKTFUNK_PAD_AUDIO_KIND_SPEAKER: u8 = 1;

/// [`punktfunk_connection_set_pad_audio_caps`] bit: the pad renders the HAPTICS stream
/// (DualSense voice coils).
pub const PUNKTFUNK_PAD_AUDIO_CAP_HAPTICS: u8 = 0x01;
/// [`punktfunk_connection_set_pad_audio_caps`] bit: the pad renders the SPEAKER stream.
pub const PUNKTFUNK_PAD_AUDIO_CAP_SPEAKER: u8 = 0x02;

// ABI cap bits must match the wire constants.
#[cfg(feature = "quic")]
const _: () = {
    assert!(PUNKTFUNK_VIDEO_CAP_10BIT == crate::quic::VIDEO_CAP_10BIT);
    assert!(PUNKTFUNK_VIDEO_CAP_HDR == crate::quic::VIDEO_CAP_HDR);
    assert!(PUNKTFUNK_VIDEO_CAP_444 == crate::quic::VIDEO_CAP_444);
    assert!(PUNKTFUNK_CODEC_H264 == crate::quic::CODEC_H264);
    assert!(PUNKTFUNK_CODEC_HEVC == crate::quic::CODEC_HEVC);
    assert!(PUNKTFUNK_CODEC_AV1 == crate::quic::CODEC_AV1);
    assert!(PUNKTFUNK_CODEC_PYROWAVE == crate::quic::CODEC_PYROWAVE);
    assert!(PUNKTFUNK_HOST_CAP_GAMEPAD_STATE == crate::quic::HOST_CAP_GAMEPAD_STATE);
    assert!(PUNKTFUNK_HOST_CAP_CLIPBOARD == crate::quic::HOST_CAP_CLIPBOARD);
    assert!(PUNKTFUNK_HOST_CAP_PEN == crate::quic::HOST_CAP_PEN);
    assert!(PUNKTFUNK_HOST_CAP_PAD_AUDIO == crate::quic::HOST_CAP_PAD_AUDIO);
    assert!(PUNKTFUNK_HOST_CAP_AUDIO_HIRES == crate::quic::HOST_CAP_AUDIO_HIRES);
    assert!(PUNKTFUNK_HOST_CAP2_TOUCH == crate::quic::HOST_CAP2_TOUCH);
    assert!(PUNKTFUNK_CLIENT_CAP_PAD_AUDIO == crate::quic::CLIENT_CAP_PAD_AUDIO);
    assert!(PUNKTFUNK_CLIENT_CAP_AUDIO_HIRES == crate::quic::CLIENT_CAP_AUDIO_HIRES);
    assert!(PUNKTFUNK_CLIENT_CAP_KEEP_HOST_AUDIO == crate::quic::CLIENT_CAP_KEEP_HOST_AUDIO);
    assert!(PUNKTFUNK_PAD_AUDIO_KIND_HAPTICS == crate::quic::PAD_AUDIO_KIND_HAPTICS);
    assert!(PUNKTFUNK_PAD_AUDIO_KIND_SPEAKER == crate::quic::PAD_AUDIO_KIND_SPEAKER);
    // Setter cap bits are arrival flags 8/9 shifted down.
    assert!(
        (PUNKTFUNK_PAD_AUDIO_CAP_HAPTICS as u32) << 8
            == crate::input::ARRIVAL_FLAG_PAD_AUDIO_HAPTICS
    );
    assert!(
        (PUNKTFUNK_PAD_AUDIO_CAP_SPEAKER as u32) << 8
            == crate::input::ARRIVAL_FLAG_PAD_AUDIO_SPEAKER
    );
    assert!(PUNKTFUNK_PEN_IN_RANGE == crate::quic::PEN_IN_RANGE);
    assert!(PUNKTFUNK_PEN_TOUCHING == crate::quic::PEN_TOUCHING);
    assert!(PUNKTFUNK_PEN_BARREL1 == crate::quic::PEN_BARREL1);
    assert!(PUNKTFUNK_PEN_BARREL2 == crate::quic::PEN_BARREL2);
    assert!(PUNKTFUNK_PEN_BATCH_MAX as usize == crate::quic::PEN_BATCH_MAX);
    assert!(PUNKTFUNK_PEN_TILT_UNKNOWN == crate::quic::PEN_TILT_UNKNOWN);
    assert!(PUNKTFUNK_PEN_ANGLE_UNKNOWN == crate::quic::PEN_ANGLE_UNKNOWN);
    assert!(PUNKTFUNK_PEN_DISTANCE_UNKNOWN == crate::quic::PEN_DISTANCE_UNKNOWN);
};

// ABI gamepad constants must match the wire enum.
const _: () = {
    use crate::config::GamepadPref;
    use crate::input::gamepad as g;
    assert!(PUNKTFUNK_GAMEPAD_AUTO == GamepadPref::Auto.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_XBOX360 == GamepadPref::Xbox360.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_DUALSENSE == GamepadPref::DualSense.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_XBOXONE == GamepadPref::XboxOne.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_DUALSHOCK4 == GamepadPref::DualShock4.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_STEAMCONTROLLER == GamepadPref::SteamController.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_STEAMDECK == GamepadPref::SteamDeck.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_DUALSENSEEDGE == GamepadPref::DualSenseEdge.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_SWITCHPRO == GamepadPref::SwitchPro.to_u8() as u32);
    assert!(PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2 == GamepadPref::SteamController2.to_u8() as u32);
    assert!(
        PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2_PUCK == GamepadPref::SteamController2Puck.to_u8() as u32
    );
    assert!(PUNKTFUNK_GAMEPAD_XBOXELITE == GamepadPref::XboxElite.to_u8() as u32);
    // Extended button bits mirror the wire `input::gamepad` constants.
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE1 == g::BTN_PADDLE1);
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE2 == g::BTN_PADDLE2);
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE3 == g::BTN_PADDLE3);
    assert!(PUNKTFUNK_GAMEPAD_BTN_PADDLE4 == g::BTN_PADDLE4);
    assert!(PUNKTFUNK_GAMEPAD_BTN_MISC1 == g::BTN_MISC1);
};

// No `struct_size`: growing these corrupts old callers. Additive kinds must not
// grow them; a deliberate widen needs an [`crate::ABI_VERSION`] bump. RichInput
// is frozen at 20. HidOutput is 19 + 2 + `HID_REPORT_MAX`.
#[cfg(feature = "quic")]
const _: () = {
    assert!(core::mem::size_of::<PunktfunkRichInput>() == 20);
    assert!(core::mem::size_of::<PunktfunkHidOutput>() == 19 + 2 + crate::quic::HID_REPORT_MAX);
};

/// Trust: `pin_sha256` (NULL or 32 bytes) is the expected SHA-256 of the host
/// certificate — a mismatch is rejected. NULL = trust on first use; persist
/// `observed_sha256_out` (NULL or 32 bytes, filled on success) and pass it as
/// the pin on every later connect.
///
/// Identity: `client_cert_pem`/`client_key_pem` (both NULL, or both NUL-terminated
/// PEM — [`punktfunk_generate_identity`]) are TLS client auth so a host can
/// recognize this client once paired ([`punktfunk_pair`]). NULL = anonymous;
/// `--require-pairing` hosts reject anonymous sessions.
///
/// # Safety
/// `host` is a NUL-terminated UTF-8 string (IP or resolvable hostname);
/// `pin_sha256`/`observed_sha256_out` are each NULL or valid for 32 bytes;
/// `client_cert_pem`/`client_key_pem` are each NULL or NUL-terminated UTF-8.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        punktfunk_connect_ex(
            host,
            port,
            width,
            height,
            refresh_hz,
            PUNKTFUNK_COMPOSITOR_AUTO,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// [`punktfunk_connect`] plus a `compositor` (`PUNKTFUNK_COMPOSITOR_*`). `AUTO`
/// (or unrecognized) lets the host decide; a concrete value is honored only if
/// available. Same as [`punktfunk_connect_ex2`] with `gamepad = PUNKTFUNK_GAMEPAD_AUTO`.
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect_ex(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        punktfunk_connect_ex2(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            PUNKTFUNK_GAMEPAD_AUTO,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// [`punktfunk_connect_ex`] plus a virtual `gamepad` (`PUNKTFUNK_GAMEPAD_*`).
/// `AUTO` (or unrecognized) lets the host decide (`PUNKTFUNK_GAMEPAD` env, else
/// X-Box 360). Resolved via [`punktfunk_connection_gamepad`]. Only DualSense
/// emits HID-output feedback.
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect_ex2(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        punktfunk_connect_ex3(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            0, // bitrate_kbps = 0: host default
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// [`punktfunk_connect_ex2`] plus encoder `bitrate_kbps`. `0` = host default;
/// other values clamp to the host range. Read [`punktfunk_connection_bitrate`].
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect_ex3(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    // No game requested: the host's default session.
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        punktfunk_connect_ex4(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            std::ptr::null(),
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// [`punktfunk_connect_ex3`] plus a library title. `launch_id` is a store-qualified
/// id (`steam:<appid>` / `custom:<id>`); the host resolves it against its own
/// library. `NULL` / empty / unknown ⇒ default session, no game.
///
/// # Safety
/// Same as [`punktfunk_connect`]; non-NULL `launch_id` is a NUL-terminated C string.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect_ex4(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    // No video caps: 8-bit BT.709 SDR. HDR embedders pass bits via `ex5`.
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        punktfunk_connect_ex5(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            0,
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// [`punktfunk_connect_ex4`] plus `video_caps` (`PUNKTFUNK_VIDEO_CAP_*`).
/// Host upgrades only when the bit is set. Read colour via
/// [`punktfunk_connection_color_info`] / [`punktfunk_connection_next_hdr_meta`].
///
/// # Safety
/// Same as [`punktfunk_connect`]; non-NULL `launch_id` is a NUL-terminated C string.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex5(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    // Stereo (2 channels). Surround embedders pass 6/8 via `ex6`.
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        punktfunk_connect_ex6(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            2, // audio_channels = stereo
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// [`punktfunk_connect_ex5`] plus audio channel count: `2` (stereo), `6` (5.1) or
/// `8` (7.1). Host clamps to what it can capture; read
/// [`punktfunk_connection_audio_channels`]. Advertises HEVC-only, no codec
/// preference ([`punktfunk_connect_ex7`] negotiates).
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex6(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        punktfunk_connect_ex7(
            host,
            port,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            audio_channels,
            PUNKTFUNK_CODEC_HEVC, // HEVC-only, no preference
            0,
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            timeout_ms,
        )
    }
}

/// [`punktfunk_connect_ex6`] plus `video_codecs` (`PUNKTFUNK_CODEC_*` bits) and a
/// soft `preferred_codec` (one codec bit, `0` = none). Host honors preference when
/// it can produce it, else best shared codec. Read [`punktfunk_connection_codec`].
///
/// # Safety
/// Same as [`punktfunk_connect`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex7(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
) -> *mut PunktfunkConnection {
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        connect_ex_impl(
            host,
            port,
            0, // no client caps
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            audio_channels,
            video_codecs,
            preferred_codec,
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            std::ptr::null(), // no device name: OS default
            // 0/0 = unspecified (Opus). Any non-zero rate/bits is a lossless ask.
            0,
            0,
            0,
            timeout_ms,
            std::ptr::null_mut(),
        )
    }
}

/// [`punktfunk_connect_ex7`] plus `status_out` (nullable): the mapped
/// [`PunktfunkStatus`], including typed host rejections. NULL alone cannot say why.
///
/// # Safety
/// Same as [`punktfunk_connect`]; non-null `status_out` points to a writable `i32`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex8(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        connect_ex_impl(
            host,
            port,
            0, // no client caps
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            audio_channels,
            video_codecs,
            preferred_codec,
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            std::ptr::null(), // no device name: OS default
            // 0/0 = unspecified (Opus). Any non-zero rate/bits is a lossless ask.
            0,
            0,
            0,
            timeout_ms,
            status_out,
        )
    }
}

/// [`punktfunk_connect_ex8`] plus `client_caps`. Cursor bit: host stops compositing;
/// the embedder must drain shape/state or there is no pointer. Pass 0 for composited.
///
/// # Safety
/// Same as [`punktfunk_connect_ex8`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex9(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
    client_caps: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        connect_ex_impl(
            host,
            port,
            client_caps,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            audio_channels,
            video_codecs,
            preferred_codec,
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            std::ptr::null(), // no device name: OS default
            // 0/0 = unspecified (Opus). Any non-zero rate/bits is a lossless ask.
            0,
            0,
            0,
            timeout_ms,
            status_out,
        )
    }
}

/// [`punktfunk_connect_ex9`] plus `device_name` — the label this device knocks
/// with. NULL/empty = [`crate::client::device_name`]. Longer than
/// [`HELLO_NAME_MAX`] is truncated on a character boundary, not rejected.
///
/// # Safety
/// Same as [`punktfunk_connect_ex9`]; non-null `device_name` is a NUL-terminated C string.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex10(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
    client_caps: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    device_name: *const std::os::raw::c_char,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        connect_ex_impl(
            host,
            port,
            client_caps,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            audio_channels,
            video_codecs,
            preferred_codec,
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            device_name,
            // 0/0 = unspecified (Opus). Any non-zero rate/bits is a lossless ask.
            0,
            0,
            0,
            timeout_ms,
            status_out,
        )
    }
}

/// [`punktfunk_connect_ex10`] plus an audio-format ask (`audio_rate_hz` /
/// `audio_bits`). Any non-zero pair — including `48000`/`16` — sets
/// `CLIENT_CAP_AUDIO_HIRES` and asks for lossless `0xD3`. `0`/`0` is unspecified
/// (Opus). Do not pass 48 kHz/16 as a stand-in for default.
///
/// The host may still resolve Opus (`design/hi-res-audio.md`); open the device
/// from [`punktfunk_connection_audio_sample_rate`] / `_bits`, not from the ask.
/// At 44.1 kHz, [`punktfunk_connection_audio_frame_us`] is a nominal length —
/// advance clocks from samples / rate, not from that figure.
///
/// # Safety
/// Same as [`punktfunk_connect_ex10`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex11(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    audio_rate_hz: u32,
    audio_bits: u8,
    video_codecs: u8,
    preferred_codec: u8,
    client_caps: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    device_name: *const std::os::raw::c_char,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        connect_ex_impl(
            host,
            port,
            client_caps,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            audio_channels,
            video_codecs,
            preferred_codec,
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            device_name,
            audio_rate_hz,
            audio_bits,
            0,
            timeout_ms,
            status_out,
        )
    }
}

/// [`punktfunk_connect_ex11`] plus `video_fit`: how this client fills its view when the frame's
/// shape differs (`PUNKTFUNK_VIDEO_FIT_*`; unknown = fit). A host that frames the picture for
/// another device reframes to it. Every other argument is [`punktfunk_connect_ex11`]'s.
///
/// # Safety
/// Same as [`punktfunk_connect_ex10`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn punktfunk_connect_ex12(
    host: *const std::os::raw::c_char,
    port: u16,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    audio_rate_hz: u32,
    audio_bits: u8,
    video_codecs: u8,
    preferred_codec: u8,
    client_caps: u8,
    video_fit: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    device_name: *const std::os::raw::c_char,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    // SAFETY: pointers forwarded unchanged; this shim dereferences nothing.
    unsafe {
        connect_ex_impl(
            host,
            port,
            client_caps,
            width,
            height,
            refresh_hz,
            compositor,
            gamepad,
            bitrate_kbps,
            video_caps,
            audio_channels,
            video_codecs,
            preferred_codec,
            launch_id,
            pin_sha256,
            observed_sha256_out,
            client_cert_pem,
            client_key_pem,
            device_name,
            audio_rate_hz,
            audio_bits,
            video_fit,
            timeout_ms,
            status_out,
        )
    }
}

/// [`punktfunk_connect_ex12`] `video_fit`: whole picture, bars.
pub const PUNKTFUNK_VIDEO_FIT_FIT: u8 = 0;
/// [`punktfunk_connect_ex12`] `video_fit`: fill the view, cut the overflow.
pub const PUNKTFUNK_VIDEO_FIT_CROP: u8 = 1;
/// [`punktfunk_connect_ex12`] `video_fit`: fill the view, scale each axis alone.
pub const PUNKTFUNK_VIDEO_FIT_STRETCH: u8 = 2;

/// [`punktfunk_connect_ex9`] `client_caps` bit: render the host cursor locally
/// (`design/remote-desktop-sweep.md`).
pub const PUNKTFUNK_CLIENT_CAP_CURSOR: u8 = 0x01;

/// [`punktfunk_connect_ex9`] `client_caps` bit: presenter is vsync-aware and
/// feeds [`punktfunk_connection_report_phase`] (`design/phase-locked-capture.md`).
/// Advisory: the host arms on report receipt.
pub const PUNKTFUNK_CLIENT_CAP_PHASE_LOCK: u8 = 0x02;

/// [`punktfunk_connect_ex9`] `client_caps` bit: pad-audio plane (0xD1 — DualSense
/// voice-coil + speaker). Drain [`punktfunk_connection_next_pad_audio`] and declare
/// pads via [`punktfunk_connection_set_pad_audio_caps`]. Host emits only with
/// [`PUNKTFUNK_HOST_CAP_PAD_AUDIO`].
pub const PUNKTFUNK_CLIENT_CAP_PAD_AUDIO: u8 = 0x08;

/// Ask for lossless `0xD3`. Usually derived from a non-zero `audio_rate_hz`/
/// `audio_bits` on [`punktfunk_connect_ex11`]. Set by hand only for 48 kHz/16-bit
/// lossless (indistinguishable from an unspecified ask).
pub const PUNKTFUNK_CLIENT_CAP_AUDIO_HIRES: u8 = 0x10;

/// Keep host speakers live this session (do not park the mix). Request-only;
/// hosts that do not know the bit ignore it.
pub const PUNKTFUNK_CLIENT_CAP_KEEP_HOST_AUDIO: u8 = 0x20;

/// Cut a device name to [`HELLO_NAME_MAX`] bytes on a character boundary.
/// Truncate, don't reject: a too-long label must not fail connect.
#[cfg(feature = "quic")]
fn clamp_device_name(s: &str) -> String {
    let end = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&i| i <= crate::quic::HELLO_NAME_MAX)
        .last()
        .unwrap_or(0);
    s[..end].to_string()
}

/// Growable connect options for [`punktfunk_connect_opts`]. Zero-init, set
/// `struct_size = sizeof(PunktfunkConnectOpts)`, then the fields you mean.
/// Zero = auto/unspecified (`audio_rate_hz = 0` is Opus; a non-zero pair is
/// lossless). Append only; no tail padding (96/68-byte asserts); bump ABI.
#[cfg(feature = "quic")]
#[repr(C)]
pub struct PunktfunkConnectOpts {
    /// `sizeof(PunktfunkConnectOpts)` as this caller was compiled. Smaller than
    /// the frozen minimum is rejected; a shorter prefix defaults the tail.
    pub struct_size: u32,
    /// Required: NUL-terminated UTF-8 IP or hostname (the one non-nullable pointer here).
    pub host: *const std::os::raw::c_char,
    /// Library id to auto-launch, or null ([`punktfunk_connect_ex4`]).
    pub launch_id: *const std::os::raw::c_char,
    /// Null (trust on first use) or the host certificate's expected 32-byte SHA-256
    /// ([`punktfunk_connect`]'s trust contract).
    pub pin_sha256: *const u8,
    /// TLS client identity: both null (anonymous) or both NUL-terminated PEM
    /// ([`punktfunk_generate_identity`]).
    pub client_cert_pem: *const std::os::raw::c_char,
    /// See `client_cert_pem`.
    pub client_key_pem: *const std::os::raw::c_char,
    /// Label this device knocks with, or null for the OS default
    /// ([`punktfunk_connect_ex10`]).
    pub device_name: *const std::os::raw::c_char,
    /// Requested mode ([`punktfunk_connect`]).
    pub width: u32,
    /// See `width`.
    pub height: u32,
    /// See `width`.
    pub refresh_hz: u32,
    /// `PUNKTFUNK_COMPOSITOR_*`; `0`/unrecognized = auto ([`punktfunk_connect_ex`]).
    pub compositor: u32,
    /// `PUNKTFUNK_GAMEPAD_*`; `0`/unrecognized = auto ([`punktfunk_connect_ex2`]).
    pub gamepad: u32,
    /// Session wire budget in kbps; `0` = host default ([`punktfunk_connect_ex3`]).
    pub bitrate_kbps: u32,
    /// Audio format ask; `0`/`0` = unspecified (Opus). An explicit pair — including
    /// 48000/16 — is lossless and derives `PUNKTFUNK_CLIENT_CAP_AUDIO_HIRES`.
    pub audio_rate_hz: u32,
    /// Connect timeout in milliseconds.
    pub timeout_ms: u32,
    /// Required: the host's UDP port.
    pub port: u16,
    /// `PUNKTFUNK_VIDEO_CAP_*` bits ([`punktfunk_connect_ex5`]).
    pub video_caps: u8,
    /// Channel ask: 2 / 6 / 8; `0` = stereo ([`punktfunk_connect_ex6`]).
    pub audio_channels: u8,
    /// See `audio_rate_hz`.
    pub audio_bits: u8,
    /// `PUNKTFUNK_CODEC_*` bits the client can decode ([`punktfunk_connect_ex7`]).
    pub video_codecs: u8,
    /// The one `PUNKTFUNK_CODEC_*` bit to prefer; `0` = host's choice
    /// ([`punktfunk_connect_ex7`]).
    pub preferred_codec: u8,
    /// `PUNKTFUNK_CLIENT_CAP_*` bits ([`punktfunk_connect_ex9`]).
    pub client_caps: u8,
    /// Always `0`, ignored. Held so the struct keeps its v35 size.
    pub reserved1: u32,
    /// Always `0`. Fills what would otherwise be tail padding: C leaves padding
    /// unspecified even under `= {0}`, so the next appended field would read a
    /// caller's garbage. Spend this before growing the struct again.
    pub reserved0: u32,
}

// No tail padding (append contract). On grow: freeze `CONNECT_OPTS_MIN_SIZE`, update these sizes.
#[cfg(feature = "quic")]
const _: () = {
    #[cfg(target_pointer_width = "64")]
    assert!(core::mem::size_of::<PunktfunkConnectOpts>() == 104);
    #[cfg(target_pointer_width = "32")]
    assert!(core::mem::size_of::<PunktfunkConnectOpts>() == 76);
};

/// Name the settings preset the next connect sends: its stable id and display name. The host
/// shows it and hands it to hooks; the stream is unchanged. A null `id` names none. The value
/// outlives the call, so set it before every connect. ABI v38.
///
/// # Safety
/// `id` and `name` are null or NUL-terminated C strings, read during this call only.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_set_session_preset(
    id: *const std::os::raw::c_char,
    name: *const std::os::raw::c_char,
) {
    // SAFETY: null or NUL-terminated per the contract above.
    let preset = match unsafe { (opt_cstr(id), opt_cstr(name)) } {
        (Ok(Some(id)), name) => {
            crate::quic::SessionPreset::new(id, name.ok().flatten().unwrap_or(""))
        }
        _ => None,
    };
    crate::client::set_session_preset(preset);
}

/// Minimum `struct_size` [`punktfunk_connect_opts`] accepts. Frozen: when the
/// struct grows this stays put so older callers keep connecting; only the size
/// asserts above move.
#[cfg(all(feature = "quic", target_pointer_width = "64"))]
const CONNECT_OPTS_MIN_SIZE: usize = 96;
#[cfg(all(feature = "quic", target_pointer_width = "32"))]
const CONNECT_OPTS_MIN_SIZE: usize = 68;

/// Connect with every option in one growable [`PunktfunkConnectOpts`]. Semantics
/// match [`punktfunk_connect_ex11`] field for field. The `ex` chain stays
/// byte-identical; new options land only in this struct.
///
/// `status_out` (nullable) is written on every path; `observed_sha256_out`
/// (null or 32 bytes) receives the host fingerprint on success.
///
/// # Safety
/// `opts` is null or points to at least `opts->struct_size` readable bytes laid
/// out as its declared [`PunktfunkConnectOpts`]; pointer fields follow
/// [`punktfunk_connect_ex11`]; `observed_sha256_out` is null or valid for 32 bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connect_opts(
    opts: *const PunktfunkConnectOpts,
    observed_sha256_out: *mut u8,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    let set_status = |s: crate::error::PunktfunkStatus| {
        if !status_out.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *status_out = s as i32 };
        }
    };
    if opts.is_null() {
        set_status(crate::error::PunktfunkStatus::NullPointer);
        return std::ptr::null_mut();
    }
    // Size prefix first; a shorter caller gets a zeroed tail, not a misread.
    // SAFETY: `addr_of!` does not form a `&`; the caller may have a different size.
    let declared = unsafe { std::ptr::addr_of!((*opts).struct_size).read_unaligned() } as usize;
    if declared < CONNECT_OPTS_MIN_SIZE {
        set_status(crate::error::PunktfunkStatus::InvalidArg);
        return std::ptr::null_mut();
    }
    // Copy the known prefix over zeros so a shorter caller's missing tail stays unspecified.
    // SAFETY: all-zero is a valid `PunktfunkConnectOpts` — null pointers and zero scalars.
    let mut o: PunktfunkConnectOpts = unsafe { std::mem::zeroed() };
    let take = declared.min(std::mem::size_of::<PunktfunkConnectOpts>());
    // SAFETY: `opts` is readable for `declared >= take`; `o` is a local and cannot overlap.
    unsafe {
        std::ptr::copy_nonoverlapping(
            opts.cast::<u8>(),
            std::ptr::addr_of_mut!(o).cast::<u8>(),
            take,
        );
    }
    // SAFETY: pointer fields forwarded unchanged; the copy did not deref what they point at.
    unsafe {
        connect_ex_impl(
            o.host,
            o.port,
            o.client_caps,
            o.width,
            o.height,
            o.refresh_hz,
            o.compositor,
            o.gamepad,
            o.bitrate_kbps,
            o.video_caps,
            o.audio_channels,
            o.video_codecs,
            o.preferred_codec,
            o.launch_id,
            o.pin_sha256,
            observed_sha256_out,
            o.client_cert_pem,
            o.client_key_pem,
            o.device_name,
            o.audio_rate_hz,
            o.audio_bits,
            0,
            o.timeout_ms,
            status_out,
        )
    }
}

/// Shared body of the connect family. `status_out` is written on every path.
/// Null `device_name` = OS default. `audio_rate_hz`/`audio_bits` 0/0 is unspecified.
#[cfg(feature = "quic")]
#[allow(clippy::too_many_arguments)]
unsafe fn connect_ex_impl(
    host: *const std::os::raw::c_char,
    port: u16,
    client_caps: u8,
    width: u32,
    height: u32,
    refresh_hz: u32,
    compositor: u32,
    gamepad: u32,
    bitrate_kbps: u32,
    video_caps: u8,
    audio_channels: u8,
    video_codecs: u8,
    preferred_codec: u8,
    launch_id: *const std::os::raw::c_char,
    pin_sha256: *const u8,
    observed_sha256_out: *mut u8,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    device_name: *const std::os::raw::c_char,
    audio_rate_hz: u32,
    audio_bits: u8,
    video_fit: u8,
    timeout_ms: u32,
    status_out: *mut i32,
) -> *mut PunktfunkConnection {
    let set_status = |s: crate::error::PunktfunkStatus| {
        if !status_out.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *status_out = s as i32 };
        }
    };
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if host.is_null() {
            set_status(crate::error::PunktfunkStatus::InvalidArg);
            return std::ptr::null_mut();
        }
        // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
        let host = match unsafe { std::ffi::CStr::from_ptr(host) }.to_str() {
            Ok(s) => s,
            Err(_) => {
                set_status(crate::error::PunktfunkStatus::InvalidArg);
                return std::ptr::null_mut();
            }
        };
        // Bad-UTF-8 launch id is non-fatal: treat it as "no game" rather than failing connect.
        // SAFETY: pointers are caller-supplied and null-checked on this path.
        let launch = match unsafe { opt_cstr(launch_id) } {
            Ok(Some(s)) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        };
        // Bad/empty name is non-fatal (OS default). Truncate on a character boundary.
        // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
        let name = match unsafe { opt_cstr(device_name) } {
            Ok(Some(s)) if !s.trim().is_empty() => clamp_device_name(s.trim()),
            _ => crate::client::device_name(),
        };
        let mode = crate::config::Mode {
            width,
            height,
            refresh_hz,
        };
        // Unrecognized = Auto must hold for the full u32 domain: `as u8` would wrap
        // 0x101 into a concrete choice before `from_u8`'s fallback could apply.
        let pref = u8::try_from(compositor)
            .map(crate::config::CompositorPref::from_u8)
            .unwrap_or_default();
        let gamepad = u8::try_from(gamepad)
            .map(crate::config::GamepadPref::from_u8)
            .unwrap_or_default();
        let pin = if pin_sha256.is_null() {
            None
        } else {
            let mut p = [0u8; 32];
            // SAFETY: caller pointer/length; borrowed for this call only.
            p.copy_from_slice(unsafe { std::slice::from_raw_parts(pin_sha256, 32) });
            Some(p)
        };
        // SAFETY: pointers are caller-supplied and null-checked on this path.
        let identity = match (unsafe { opt_cstr(client_cert_pem) }, unsafe {
            opt_cstr(client_key_pem)
        }) {
            (Ok(Some(c)), Ok(Some(k))) => Some((c.to_string(), k.to_string())),
            (Ok(None), Ok(None)) => None,
            _ => {
                // Half an identity / bad UTF-8: fail closed.
                set_status(crate::error::PunktfunkStatus::InvalidArg);
                return std::ptr::null_mut();
            }
        };
        match crate::client::NativeClient::connect_with_audio_format(
            host,
            port,
            mode,
            pref,
            gamepad,
            bitrate_kbps,
            video_caps,
            crate::audio::normalize_channels(audio_channels),
            // Unvalidated on purpose: a bad rate is the host's to decline, not a failed connect.
            audio_rate_hz,
            audio_bits,
            // No C-side ask yet; every embedder decodes whatever coupling the host answers.
            crate::audio::AudioLayout::Legacy,
            crate::video_fit::VideoFit::from_wire(video_fit),
            video_codecs,
            preferred_codec,
            // No display-HDR-volume in the C ABI; host EDID defaults stand.
            None,
            // CLIENT_CAP_CURSOR: host stops compositing; only if the embedder draws the cursor.
            client_caps,
            // No slice-progressive parts: `PunktfunkFrame` cannot tell a part from a whole AU.
            false,
            launch,
            // Knock label: embedder `device_name`, else OS default.
            Some(name),
            pin,
            identity,
            std::time::Duration::from_millis(timeout_ms as u64),
            // No abort switch: connect is blocking with nothing to poll.
            None,
        ) {
            Ok(c) => {
                if !observed_sha256_out.is_null() {
                    // SAFETY: caller output buffer of the documented length, written once.
                    unsafe {
                        std::slice::from_raw_parts_mut(observed_sha256_out, 32)
                            .copy_from_slice(&c.host_fingerprint);
                    }
                }
                set_status(crate::error::PunktfunkStatus::Ok);
                Box::into_raw(Box::new(PunktfunkConnection {
                    inner: c,
                    last: std::sync::Mutex::new(None),
                    last_audio: std::sync::Mutex::new(None),
                    audio_pcm: std::sync::Mutex::new(AudioPcmState::default()),
                    last_clip: std::sync::Mutex::new(None),
                    last_cursor_shape: std::sync::Mutex::new(None),
                    hud_snap: std::sync::Mutex::new(crate::hud::StatsSnapshot::default()),
                }))
            }
            Err(e) => {
                set_status(e.status());
                std::ptr::null_mut()
            }
        }
    }));
    r.unwrap_or_else(|_| {
        set_status(crate::error::PunktfunkStatus::Panic);
        std::ptr::null_mut()
    })
}

/// Generate a persistent client identity: self-signed certificate + private key,
/// both PEM, NUL-terminated, written into the caller's buffers. Generate once,
/// store both, pass them to [`punktfunk_pair`] and every [`punktfunk_connect`].
/// Hosts recognize this client by the certificate fingerprint. 4096-byte buffers
/// are ample.
///
/// # Safety
/// `cert_pem_out` is writable for `cert_cap` bytes; `key_pem_out` for `key_cap`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_generate_identity(
    cert_pem_out: *mut std::os::raw::c_char,
    cert_cap: usize,
    key_pem_out: *mut std::os::raw::c_char,
    key_cap: usize,
) -> PunktfunkStatus {
    guard(|| {
        if cert_pem_out.is_null() || key_pem_out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let (cert, key) = match crate::quic::endpoint::generate_identity() {
            Ok(t) => t,
            Err(_) => return PunktfunkStatus::Io,
        };
        if cert.len() + 1 > cert_cap || key.len() + 1 > key_cap {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: pointers are caller-supplied and null-checked on this path.
        unsafe {
            // `.cast()`: `c_char` is i8 on x86_64 and u8 on aarch64; `as *mut u8` is not portable.
            std::ptr::copy_nonoverlapping(cert.as_ptr(), cert_pem_out.cast::<u8>(), cert.len());
            *cert_pem_out.add(cert.len()) = 0;
            std::ptr::copy_nonoverlapping(key.as_ptr(), key_pem_out.cast::<u8>(), key.len());
            *key_pem_out.add(key.len()) = 0;
        }
        PunktfunkStatus::Ok
    })
}

/// QUIC reachability probe, mDNS-independent. `Ok` if something answered, `Timeout`
/// otherwise. Blocks up to `timeout_ms`; off the UI thread.
///
/// The handshake is unpinned, so `Ok` says an address is occupied, not that it is YOUR
/// host: pass `observed_sha256_out` and compare it against the record's pin. A stranger
/// who inherits a sleeping host's lease answers too, and counting that as the host lights
/// the pip and shuts the wake gate against the machine that needs waking.
///
/// # Safety
/// `host` is a NUL-terminated UTF-8 string; `observed_sha256_out` is null, or writable
/// for 32 bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_probe(
    host: *const std::os::raw::c_char,
    port: u16,
    timeout_ms: u32,
    observed_sha256_out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: pointers are caller-supplied and null-checked on this path.
        let Ok(Some(host)) = (unsafe { opt_cstr(host) }) else {
            return PunktfunkStatus::NullPointer;
        };
        match crate::client::NativeClient::probe_identity(
            host,
            port,
            std::time::Duration::from_millis(timeout_ms as u64),
        ) {
            Some(fp) => {
                if !observed_sha256_out.is_null() {
                    // SAFETY: the caller guarantees 32 writable bytes when non-null.
                    unsafe {
                        std::ptr::copy_nonoverlapping(fp.as_ptr(), observed_sha256_out, 32);
                    }
                }
                PunktfunkStatus::Ok
            }
            None => PunktfunkStatus::Timeout,
        }
    })
}

/// PIN pairing: the host displays a PIN; pass it here. On success the host has
/// stored this client's identity and the verified host fingerprint is written to
/// `host_sha256_out` (32 bytes) — persist it as `pin_sha256` for
/// [`punktfunk_connect`]. [`PunktfunkStatus::Crypto`] for a wrong PIN.
///
/// # Safety
/// `host`/`client_cert_pem`/`client_key_pem`/`pin`/`name` are NUL-terminated UTF-8;
/// `host_sha256_out` is writable for 32 bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_pair(
    host: *const std::os::raw::c_char,
    port: u16,
    client_cert_pem: *const std::os::raw::c_char,
    client_key_pem: *const std::os::raw::c_char,
    pin: *const std::os::raw::c_char,
    name: *const std::os::raw::c_char,
    host_sha256_out: *mut u8,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        let (Ok(Some(host)), Ok(Some(cert)), Ok(Some(key)), Ok(Some(pin)), Ok(Some(name))) = (
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            unsafe { opt_cstr(host) },
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            unsafe { opt_cstr(client_cert_pem) },
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            unsafe { opt_cstr(client_key_pem) },
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            unsafe { opt_cstr(pin) },
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            unsafe { opt_cstr(name) },
        ) else {
            return PunktfunkStatus::NullPointer;
        };
        if host_sha256_out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match crate::client::NativeClient::pair(
            host,
            port,
            (cert, key),
            pin,
            name,
            std::time::Duration::from_millis(timeout_ms as u64),
        ) {
            Ok(fp) => {
                // SAFETY: caller output buffer of the documented length, written once.
                unsafe {
                    std::slice::from_raw_parts_mut(host_sha256_out, 32).copy_from_slice(&fp);
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Pull the next reassembled access unit, waiting up to `timeout_ms`.
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once ended.
/// On `Ok`, `*out` borrows until the next `next_au` on this handle (audio/rumble
/// planes do not invalidate it).
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one thread pulls
/// video; it may run concurrently with one audio and one rumble puller.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_au(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkFrame,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // Shared ref only: video and audio threads must not alias a `&mut`.
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_frame(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(frame) => {
                let mut slot = lock_recover(&c.last);
                let f = slot.insert(frame);
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkFrame {
                        data: f.data.as_ptr(),
                        len: f.data.len(),
                        frame_index: f.frame_index,
                        pts_ns: f.pts_ns,
                        flags: f.flags,
                        received_ns: f.received_ns,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// One audio packet. Opus on ordinary sessions, PCM on `0xD3`. `data` borrows
/// until the next `next_audio`. Plane is `host_caps & HOST_CAP_AUDIO_HIRES`.
#[cfg(feature = "quic")]
#[repr(C)]
pub struct PunktfunkAudioPacket {
    pub data: *const u8,
    pub len: usize,
    pub seq: u32,
    pub pts_ns: u64,
}

/// Pull the next audio packet, waiting up to `timeout_ms`.
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once ended.
/// On `Ok`, `out->data` borrows until the next audio call (independent of video).
/// Drain from a dedicated thread — Opus every 5 ms, lossless every 1–5 ms; queue
/// holds 320 ms.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one audio puller;
/// it may run concurrently with the video/rumble pullers.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_audio(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkAudioPacket,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_audio(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(pkt) => {
                let mut slot = lock_recover(&c.last_audio);
                let p = slot.insert(pkt);
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkAudioPacket {
                        data: p.data.as_ptr(),
                        len: p.data.len(),
                        seq: p.seq,
                        pts_ns: p.pts_ns,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Mute this client's own speakers. Local only: the host keeps encoding and a session joined
/// to the same display keeps hearing the game. Audio keeps arriving and decoding — zero only
/// what you queue for the device — so unmute lands in step instead of re-syncing. Does not
/// clear `PUNKTFUNK_AUDIO_MUTE_HOST`.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_audio_muted(
    c: *mut PunktfunkConnection,
    muted: bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        c.inner.set_audio_muted(muted);
        PunktfunkStatus::Ok
    })
}

/// Why this session is silent: `PUNKTFUNK_AUDIO_MUTE_LOCAL`, `PUNKTFUNK_AUDIO_MUTE_HOST`,
/// both, or `0`. Name the reason in the overlay from this — a local unmute leaves an
/// operator mute standing, and the player is owed the difference.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_mute(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u8`.
            unsafe { *out = c.inner.audio_mute() };
        }
        PunktfunkStatus::Ok
    })
}

/// Host-resolved audio channel count: `2` (stereo), `6` (5.1) or `8` (7.1).
/// `*out` is filled when non-NULL. Raw `0xC9` Opus is encoded for this layout
/// ([`crate::audio::layout_for`]); or use [`punktfunk_connection_next_audio_pcm`].
/// Fixed until a reconfigure.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_channels(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u8`.
            unsafe { *out = c.inner.audio_channels };
        }
        PunktfunkStatus::Ok
    })
}

/// Resolved sample rate. Open the device from this, not `PUNKTFUNK_AUDIO_SAMPLE_RATE_HZ`
/// (the Opus default). Accessor, not a field: `PunktfunkAudioPcm` has no size guard.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u32`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_sample_rate(
    c: *mut PunktfunkConnection,
    out: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u32`.
            unsafe { *out = c.inner.audio_sample_rate_hz };
        }
        PunktfunkStatus::Ok
    })
}

/// Resolved sample depth (`16`, or `24` on lossless). Plane is
/// `host_caps & PUNKTFUNK_HOST_CAP_AUDIO_HIRES`, not this: 48 kHz/16-bit matches both.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_bits(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u8`.
            unsafe { *out = c.inner.audio_bits };
        }
        PunktfunkStatus::Ok
    })
}

/// Resolved frame length in µs (ladder has sub-ms rungs). `0` = use
/// `PUNKTFUNK_AUDIO_FRAME_MS × 1000`. On 44.1 kHz this is a nominal length, not a
/// duration — size rings from it, advance clocks from samples / rate.
/// Not derivable from `next_audio_pcm`'s `frame_count` (that includes concealment).
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u16`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_frame_us(
    c: *mut PunktfunkConnection,
    out: *mut u16,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u16`.
            unsafe { *out = c.inner.audio_frame_us };
        }
        PunktfunkStatus::Ok
    })
}

/// Why the session ended (`PUNKTFUNK_END_REASON_*`). Latches after `Closed`.
/// `LOCAL`/`GAME_EXITED`/`HOST_ENDED` are not failures. Unknown values = `NONE`.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_end_reason(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u8`.
            unsafe { *out = c.inner.end_reason() as u8 };
        }
        PunktfunkStatus::Ok
    })
}

/// One decoded audio frame from [`punktfunk_connection_next_audio_pcm`]: interleaved
/// f32 in wire order `FL FR FC LFE RL RR SL SR` (first `channels` of it). `samples`
/// points at `frame_count * channels` floats and borrows until the next PCM call.
/// Rate/depth are accessors, not fields: this type has no `struct_size`.
#[cfg(feature = "quic")]
#[repr(C)]
pub struct PunktfunkAudioPcm {
    /// Interleaved f32 samples (wire channel order), `frame_count * channels` long.
    pub samples: *const f32,
    /// Samples per channel in this frame.
    pub frame_count: u32,
    /// Channel count (2/6/8) — the negotiated [`punktfunk_connection_audio_channels`].
    pub channels: u8,
    /// Source packet sequence number.
    pub seq: u32,
    /// Capture presentation timestamp (ns).
    pub pts_ns: u64,
}

/// Decode the next audio frame in-core to interleaved f32. Both planes share this
/// call; size the ring from [`punktfunk_connection_audio_sample_rate`]. Seq-gap
/// concealment is prepended in the same buffer. Quiet-wire droughts:
/// [`punktfunk_connection_audio_plc`]. Mutually exclusive with `next_audio`.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one audio puller.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_audio_pcm(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkAudioPcm,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let fmt = AudioFormat::of(&c.inner);
        let channels = fmt.channels;
        let pkt = match c
            .inner
            .next_audio(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(pkt) => pkt,
            Err(e) => return e.status(),
        };
        let mut state = lock_recover(&c.audio_pcm);
        match state.decode_packet(&pkt.data, pkt.seq, fmt) {
            // Nothing to hand out: a DTX silence marker with no loss owed before it.
            Ok(0) => PunktfunkStatus::NoFrame,
            Ok(samples) => {
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkAudioPcm {
                        samples: state.pcm.as_ptr(),
                        frame_count: (samples / channels.max(1) as usize) as u32,
                        channels,
                        seq: pkt.seq,
                        pts_ns: pkt.pts_ns,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(status) => status,
        }
    })
}

/// One drought concealment frame with no packet (`design/host-source-stutter-fixes.md`).
/// Call on `NO_FRAME` when the ring is draining. Policy stays on the embedder:
/// bound in time, gated on a real underrun. `seq`/`pts_ns` are 0 — never feed A/V
/// sync. Same PCM slot as `next_audio_pcm`; drought frames are subtracted from
/// the next packet's gap so a covered loss is not concealed twice.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one audio puller.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_audio_plc(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkAudioPcm,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let fmt = AudioFormat::of(&c.inner);
        let channels = fmt.channels;
        let mut state = lock_recover(&c.audio_pcm);
        match state.conceal(fmt) {
            Ok(0) => PunktfunkStatus::NoFrame,
            Ok(samples) => {
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkAudioPcm {
                        samples: state.pcm.as_ptr(),
                        frame_count: (samples / channels.max(1) as usize) as u32,
                        channels,
                        seq: 0,
                        pts_ns: 0,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(status) => status,
        }
    })
}

/// Next 0xD1 pad-audio Opus frame, copied into `buf`. Return length, `0` =
/// nothing this poll, `-1` = ended. Fan out by pad/kind. One puller.
///
/// # Safety
/// `c` is a valid connection handle; the `out_*` pointers are writable (NULLs skipped);
/// `buf` is writable for `buf_len` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_pad_audio(
    c: *mut PunktfunkConnection,
    out_pad: *mut u8,
    out_kind: *mut u8,
    out_seq: *mut u32,
    out_pts_ns: *mut u64,
    buf: *mut u8,
    buf_len: usize,
    timeout_ms: u32,
) -> i32 {
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return -1,
        };
        if buf.is_null() && buf_len != 0 {
            return -1;
        }
        match c
            .inner
            .next_pad_audio(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Some(f) => {
                if f.opus.is_empty() || f.opus.len() > buf_len {
                    // Empty/oversized: skip like DTX; truncated Opus is undecodable anyway.
                    return 0;
                }
                // SAFETY: optional out-params null-checked; `buf` copy length was just bounded.
                unsafe {
                    if !out_pad.is_null() {
                        *out_pad = f.pad;
                    }
                    if !out_kind.is_null() {
                        *out_kind = f.kind;
                    }
                    if !out_seq.is_null() {
                        *out_seq = f.seq;
                    }
                    if !out_pts_ns.is_null() {
                        *out_pts_ns = f.pts_ns;
                    }
                    std::ptr::copy_nonoverlapping(f.opus.as_ptr(), buf, f.opus.len());
                }
                f.opus.len() as i32
            }
            // `None` folds timeout and closed; the shutdown flag tells them apart so the
            // plane loop can exit instead of polling a dead session forever.
            None if c.inner.is_session_ended() => -1,
            None => 0,
        }
    }));
    r.unwrap_or(-1)
}

/// Declare pad `pad`'s 0xD1 render caps. Call at attach, before arrival; bits
/// fold into arrival flags 8/9. Latest-wins; unknown bits masked.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_pad_audio_caps(
    c: *mut PunktfunkConnection,
    pad: u8,
    audio_caps: u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        c.inner.set_pad_audio_caps(pad, audio_caps);
        PunktfunkStatus::Ok
    })
}

/// Switch the pads in `mask` (bit = wire pad index) to controller mouse: their buttons and sticks
/// drive the host pointer and a few keys while the host pad sits neutral. `0` returns every pad
/// to passthrough. Session-scoped. `Unsupported` without `PUNKTFUNK_GRANT_POINTER`.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_pad_mouse(
    c: *mut PunktfunkConnection,
    mask: u16,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.set_pad_mouse(mask) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Change scroll direction for this session at the shared outbound seam.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_invert_scroll(
    c: *mut PunktfunkConnection,
    invert: bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        c.inner.set_invert_scroll(invert);
        PunktfunkStatus::Ok
    })
}

/// Pads in controller mouse now. A removed pad or a lost pointer grant clears its bit.
///
/// # Safety
/// `c` is a valid connection handle; `mask` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_pad_mouse(
    c: *const PunktfunkConnection,
    mask: *mut u16,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: out-param is optional; null-checked before write.
        unsafe {
            if !mask.is_null() {
                *mask = c.inner.pad_mouse();
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Wire pad indices the host holds now, a bit per pad: declared or driven, not yet removed.
///
/// # Safety
/// `c` is a valid connection handle; `mask` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_live_pads(
    c: *const PunktfunkConnection,
    mask: *mut u16,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: out-param is optional; null-checked before write.
        unsafe {
            if !mask.is_null() {
                *mask = c.inner.live_pads();
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Pull the next rumble update, waiting up to `timeout_ms`. Amplitudes are
/// 0..0xFFFF (`low`/`high` motors), `(0, 0)` = stop. Same timeout/closed as
/// [`punktfunk_connection_next_audio`]. Drops the v2 self-terminating TTL —
/// use [`punktfunk_connection_next_rumble2`] for the host-supplied lease.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs skipped).
/// At most one rumble puller; it may run concurrently with video/audio.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_rumble(
    c: *mut PunktfunkConnection,
    pad: *mut u16,
    low: *mut u16,
    high: *mut u16,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c
            .inner
            .next_rumble(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok((p, l, h)) => {
                // SAFETY: each out-param is optional; null-checked before write.
                unsafe {
                    if !pad.is_null() {
                        *pad = p;
                    }
                    if !low.is_null() {
                        *low = l;
                    }
                    if !high.is_null() {
                        *high = h;
                    }
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// `*ttl_ms` sentinel from [`punktfunk_connection_next_rumble2`] when the host sent
/// no self-termination lease. Fall back to a client-side staleness heuristic.
pub const PUNKTFUNK_RUMBLE_NO_TTL: u32 = 0xFFFF_FFFF;

/// Pull the next rumble update including its self-termination TTL. Same
/// `pad`/`low`/`high` as [`punktfunk_connection_next_rumble`], plus `*ttl_ms`:
/// milliseconds to render this level unless the host renews. [`PUNKTFUNK_RUMBLE_NO_TTL`]
/// = no lease; fall back to a client-side timeout. Reorder gate is applied inside.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs skipped).
/// At most one rumble puller; it may run concurrently with video/audio.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_rumble2(
    c: *mut PunktfunkConnection,
    pad: *mut u16,
    low: *mut u16,
    high: *mut u16,
    ttl_ms: *mut u32,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c
            .inner
            .next_rumble_ttl(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok((p, l, h, ttl)) => {
                // SAFETY: each out-param is optional; null-checked before write.
                unsafe {
                    if !pad.is_null() {
                        *pad = p;
                    }
                    if !low.is_null() {
                        *low = l;
                    }
                    if !high.is_null() {
                        *high = h;
                    }
                    if !ttl_ms.is_null() {
                        *ttl_ms = ttl.map_or(PUNKTFUNK_RUMBLE_NO_TTL, u32::from);
                    }
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// `flags` bit for [`punktfunk_connection_set_rumble_quirks`]: alternate the low
/// motor's LSB on keepalive re-emits so an SDL-class layer that no-ops identical
/// values still writes the device.
pub const PUNKTFUNK_RUMBLE_QUIRK_DEDUP_JITTER: u32 = 1;

/// Effective rumble from the shared policy engine. No TTL: apply `(0, 0)` as
/// stop, else run at this level; `*backstop_ms` is a safety-net duration (`0` on
/// stop). Handle motors only — triggers are [`punktfunk_connection_next_rumble_cmd2`].
/// Mutually exclusive with `next_rumble`/`next_rumble2`.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs skipped).
/// At most one rumble puller; it may run concurrently with video/audio.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_rumble_cmd(
    c: *mut PunktfunkConnection,
    pad: *mut u16,
    low: *mut u16,
    high: *mut u16,
    backstop_ms: *mut u32,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c
            .inner
            .next_rumble_command(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(cmd) => {
                // SAFETY: each out-param is optional; null-checked before write.
                unsafe {
                    if !pad.is_null() {
                        *pad = cmd.pad;
                    }
                    if !low.is_null() {
                        *low = cmd.low;
                    }
                    if !high.is_null() {
                        *high = cmd.high;
                    }
                    if !backstop_ms.is_null() {
                        *backstop_ms = cmd.backstop_ms;
                    }
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// [`punktfunk_connection_next_rumble_cmd`] plus Xbox impulse-trigger motors.
/// New symbol — growing the old signature would stack-corrupt old embedders.
/// Render triggers only on pads that have them; never fold into handles.
/// Same plane as `next_rumble_cmd`; call exactly one.
///
/// # Safety
/// `c` is a valid connection handle; out pointers are writable (NULLs skipped).
/// At most one rumble puller; it may run concurrently with video/audio.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_rumble_cmd2(
    c: *mut PunktfunkConnection,
    pad: *mut u16,
    low: *mut u16,
    high: *mut u16,
    left_trigger: *mut u16,
    right_trigger: *mut u16,
    backstop_ms: *mut u32,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c
            .inner
            .next_rumble_command(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(cmd) => {
                // SAFETY: each out-param is optional; null-checked before write.
                unsafe {
                    if !pad.is_null() {
                        *pad = cmd.pad;
                    }
                    if !low.is_null() {
                        *low = cmd.low;
                    }
                    if !high.is_null() {
                        *high = cmd.high;
                    }
                    if !left_trigger.is_null() {
                        *left_trigger = cmd.left_trigger;
                    }
                    if !right_trigger.is_null() {
                        *right_trigger = cmd.right_trigger;
                    }
                    if !backstop_ms.is_null() {
                        *backstop_ms = cmd.backstop_ms;
                    }
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Per-pad rumble quirks (call at attach). `keepalive_ms`: re-emit non-zero
/// (Steam Deck ≈ 40); `0` = none. `min_pulse_ms`: floor for `backstop_ms`.
/// A renderer that dedupes its own writes cannot use `keepalive_ms`.
///
/// # Safety
/// `c` is a valid connection handle. Callable from any thread.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_rumble_quirks(
    c: *mut PunktfunkConnection,
    pad: u16,
    keepalive_ms: u16,
    min_pulse_ms: u16,
    flags: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        c.inner.set_rumble_quirks(
            pad,
            crate::client::ActuatorQuirks {
                keepalive_ms,
                min_pulse_ms,
                dedup_jitter: flags & PUNKTFUNK_RUMBLE_QUIRK_DEDUP_JITTER != 0,
            },
        );
        PunktfunkStatus::Ok
    })
}

/// Pull the next HID-output feedback (DualSense lightbar / player LEDs / adaptive
/// trigger, or SC2 `PUNKTFUNK_HIDOUT_HID_RAW`) into `*out`.
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once ended.
/// DualSense and SC2 backends only. One puller, may run alongside other planes.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkHidOutput`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_hidout(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkHidOutput,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_hidout(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(h) => {
                // SAFETY: `out` is non-null on this path; written once by value.
                unsafe { *out = PunktfunkHidOutput::from_hid(&h) };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Pull the next static HDR metadata (ST.2086 + content light) into `*out`.
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once ended.
/// Apply the latest to the display. Only an HDR session (PQ transfer from
/// `punktfunk_connection_color_info`) emits these. One puller.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkHdrMeta`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_hdr_meta(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkHdrMeta,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_hdr_meta(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(m) => {
                // SAFETY: caller out-param, non-null on this path, written once.
                unsafe { *out = PunktfunkHdrMeta::from_meta(&m) };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Forwarded host-cursor shape: straight-alpha RGBA8, no padding, `len == w * h * 4`,
/// hotspot within `w`×`h`. `serial` is the identity [`PunktfunkCursorState`] refers
/// to — cache the built OS cursor by it.
#[repr(C)]
pub struct PunktfunkCursorShape {
    pub serial: u32,
    pub w: u16,
    pub h: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    /// Borrows connection memory until the next cursor-shape call.
    pub rgba: *const u8,
    pub len: usize,
}

/// Per-frame host-cursor state: position in host video pixels, visibility, and
/// relative-mode hint. `flags` bit 0 = visible, bit 1 = relative (host app
/// grabbed/hid the pointer — run captured relative; clear = absolute at `x`/`y`).
#[repr(C)]
pub struct PunktfunkCursorState {
    pub serial: u32,
    pub flags: u8,
    pub x: i32,
    pub y: i32,
}

/// Pull the next forwarded cursor shape (pointer-bitmap change on the control
/// stream; only `PUNKTFUNK_CLIENT_CAP_CURSOR` sessions receive any). On `Ok`,
/// `out->rgba` borrows until the next cursor-shape call. One puller per plane.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one cursor-shape
/// puller; it may run concurrently with every other plane.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_cursor_shape(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkCursorShape,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_cursor_shape(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(shape) => {
                let mut slot = lock_recover(&c.last_cursor_shape);
                let sh = slot.insert(shape);
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkCursorShape {
                        serial: sh.serial,
                        w: sh.w,
                        h: sh.h,
                        hot_x: sh.hot_x,
                        hot_y: sh.hot_y,
                        rgba: sh.rgba.as_ptr(),
                        len: sh.rgba.len(),
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Pull the next cursor state (`0xD0` per host encode tick — latest-wins; drain
/// the queue and apply only the newest). Same negotiation gate as
/// [`punktfunk_connection_next_cursor_shape`].
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one cursor-state
/// puller; it may run concurrently with every other plane.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_cursor_state(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkCursorState,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_cursor_state(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(st) => {
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkCursorState {
                        serial: st.serial,
                        flags: st.flags,
                        x: st.x,
                        y: st.y,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Who draws the pointer (`design/remote-desktop-sweep.md`). `true` = client
/// draws (host forwards shape/state); `false` = host composites. Latest-wins.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_cursor_render(
    c: *mut PunktfunkConnection,
    client_draws: bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.set_cursor_render(client_draws) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Pull the next per-AU host timing (0xCF) into `*out`: capture→sent duration,
/// correlated by `pts_ns` (see [`PunktfunkHostTiming`]).
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once ended.
/// Drain non-blockingly (`timeout_ms = 0`). A host that never emits any: keep
/// showing the combined `host+network` stage. One puller.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkHostTiming`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_host_timing(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkHostTiming,
    timeout_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_host_timing(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(t) => {
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkHostTiming {
                        pts_ns: t.pts_ns,
                        host_us: t.host_us,
                    }
                };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Resolved colour signalling + encode bit depth. Each out pointer is filled when
/// non-NULL: `primaries`/`transfer`/`matrix` are CICP (BT.709 = 1; BT.2020 = 9;
/// PQ = 16, HLG = 18; BT.2020-NCL = 9), `full_range` 0/1, `bit_depth` 8 or 10.
/// Transfer 16/18 is HDR — drain [`punktfunk_connection_next_hdr_meta`]. Fixed
/// until a reconfigure.
///
/// # Safety
/// `c` is a valid connection handle; each out pointer is NULL or writable for its scalar.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_color_info(
    c: *mut PunktfunkConnection,
    primaries: *mut u8,
    transfer: *mut u8,
    matrix: *mut u8,
    full_range: *mut u8,
    bit_depth: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let color = c.inner.color;
        // SAFETY: each out-param is optional; null-checked before write.
        unsafe {
            if !primaries.is_null() {
                *primaries = color.primaries;
            }
            if !transfer.is_null() {
                *transfer = color.transfer;
            }
            if !matrix.is_null() {
                *matrix = color.matrix;
            }
            if !full_range.is_null() {
                *full_range = color.full_range;
            }
            if !bit_depth.is_null() {
                *bit_depth = c.inner.bit_depth;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Resolved chroma as HEVC `chroma_format_idc`: `1` = 4:2:0, `3` = 4:4:4.
/// `*out` is filled when non-NULL. In-band SPS is authoritative; this lets the
/// embedder pre-size the decoder. Fixed until a reconfigure.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_chroma_format(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u8`.
            unsafe { *out = c.inner.chroma_format };
        }
        PunktfunkStatus::Ok
    })
}

/// Host-resolved video codec: [`PUNKTFUNK_CODEC_H264`] / [`PUNKTFUNK_CODEC_HEVC`] /
/// [`PUNKTFUNK_CODEC_AV1`]. Build the decoder from this (never assume HEVC).
/// `*out` is filled when non-NULL. A host that did not negotiate reports HEVC.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_codec(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u8`.
            unsafe { *out = c.inner.codec };
        }
        PunktfunkStatus::Ok
    })
}

/// Negotiated wire shard payload (Welcome, bytes). Parse-window size of a
/// chunk-aligned AU (PyroWave datagram-aligned, `design/pyrowave-codec-plan.md`):
/// every `shard_payload`-sized window starts a self-delimiting chunk. Other
/// codecs never need this.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u32`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_shard_payload(
    c: *mut PunktfunkConnection,
    out: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if !out.is_null() {
            // SAFETY: `out` is non-null and the caller guarantees it is writable for one `u32`.
            unsafe { *out = u32::from(c.inner.shard_payload) };
        }
        PunktfunkStatus::Ok
    })
}

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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: `read_input_event` validates the tag before forming `&InputEvent` (else UB).
        let ev = match unsafe { read_input_event(ev) } {
            Ok(e) => e,
            Err(status) => return status,
        };
        match c.inner.send_input(ev) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if opus_data.is_null() && len != 0 {
            return PunktfunkStatus::NullPointer;
        }
        if ffi_slice_bytes::<u8>(len).is_none() {
            return PunktfunkStatus::InvalidArg;
        }
        let opus = if len == 0 {
            Vec::new()
        } else {
            // SAFETY: the ABI contract supplies `len` readable bytes; `ffi_slice_bytes` proved the
            // extent is representable by a Rust slice, copied before this call returns.
            unsafe { std::slice::from_raw_parts(opus_data, len) }.to_vec()
        };
        match c.inner.send_mic(seq, pts_ns, opus) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let rich = match unsafe { rich.as_ref() } {
            Some(r) => r,
            None => return PunktfunkStatus::NullPointer,
        };
        match rich.to_rich() {
            Some(r) => match c.inner.send_rich_input(r) {
                Ok(()) => PunktfunkStatus::Ok,
                Err(e) => e.status(),
            },
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
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
            Some(r) => match c.inner.send_rich_input(r) {
                Ok(()) => PunktfunkStatus::Ok,
                Err(e) => e.status(),
            },
            None => PunktfunkStatus::InvalidArg,
        }
    })
}

/// Clamp `pad` to 16 and the report to `HID_REPORT_MAX` — same rules as the Android shim.
#[cfg(feature = "quic")]
fn hid_report_rich_input(pad: u8, report: &[u8]) -> crate::quic::RichInput {
    let n = report.len().min(crate::quic::HID_REPORT_MAX);
    let mut data = [0u8; crate::quic::HID_REPORT_MAX];
    data[..n].copy_from_slice(&report[..n]);
    crate::quic::RichInput::HidReport {
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if data.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        if len == 0 {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: caller pointer/length; borrowed for this call only. The clamp copies.
        let report =
            unsafe { std::slice::from_raw_parts(data, len.min(crate::quic::HID_REPORT_MAX)) };
        match c.inner.send_rich_input(hid_report_rich_input(pad, report)) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if samples.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        if count == 0 || count > PUNKTFUNK_PEN_BATCH_MAX {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: caller pointer/length; borrowed for this call only.
        let raw = unsafe { std::slice::from_raw_parts(samples, count as usize) };
        let mut batch = [crate::quic::PenSample::default(); crate::quic::PEN_BATCH_MAX];
        for (slot, s) in batch.iter_mut().zip(raw) {
            match s.to_sample() {
                Some(v) => *slot = v,
                None => return PunktfunkStatus::InvalidArg,
            }
        }
        match c.inner.send_pen(&batch[..count as usize]) {
            Ok(()) => PunktfunkStatus::Ok,
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let mode = c.inner.mode();
        // SAFETY: each out-param is optional; null-checked before write.
        unsafe {
            if !width.is_null() {
                *width = mode.width;
            }
            if !height.is_null() {
                *height = mode.height;
            }
            if !refresh_hz.is_null() {
                *refresh_hz = mode.refresh_hz;
            }
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: each out-param is optional; null-checked before write.
        unsafe {
            if !gamepad.is_null() {
                *gamepad = c.inner.resolved_gamepad.to_u8() as u32;
            }
        }
        PunktfunkStatus::Ok
    })
}

// Shared clipboard (`design/clipboard-and-file-transfer.md`). All poll/serve
// bytes ride the mTLS-pinned QUIC session; nothing here opens a new listener.

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
    ev: crate::clipboard::ClipEventCore,
    slot: &mut Option<Vec<u8>>,
) -> PunktfunkClipEvent {
    use crate::clipboard::ClipEventCore as E;
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

/// Host management-API port from `Welcome`. `0` = unknown (do not dial 0; fall
/// back to 47990). Prefer this over mDNS/cached after connect.
///
/// # Safety
/// `c` is a valid connection handle; `port` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_mgmt_port(
    c: *const PunktfunkConnection,
    port: *mut u16,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: out-param is optional; null-checked before write.
        unsafe {
            if !port.is_null() {
                *port = c.inner.mgmt_port();
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Host capability bitfield from `Welcome` (`PUNKTFUNK_HOST_CAP_*`). Test
/// `CLIPBOARD` before offering the toggle, `PEN` before sending stylus batches.
/// Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; `caps` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_host_caps(
    c: *const PunktfunkConnection,
    caps: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: each out-param is optional; null-checked before write.
        unsafe {
            if !caps.is_null() {
                *caps = c.inner.host_caps();
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Second host capability byte from `Welcome` — today `PUNKTFUNK_HOST_CAP2_TOUCH`.
/// `0` toward a host that never sends the byte. Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; `caps` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_host_caps2(
    c: *const PunktfunkConnection,
    caps: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: out-param is optional; null-checked before write.
        unsafe {
            if !caps.is_null() {
                *caps = c.inner.host_caps2();
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Live `PUNKTFUNK_GRANT_*` mask (`design/per-client-access.md`). Latest
/// `AccessUpdate` wins; hosts that omit it read `PUNKTFUNK_GRANT_ALL`. Courtesy
/// only — the host enforces. Poll; do not cache for the session.
///
/// # Safety
/// `c` is a valid connection handle; `grants` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_grants(
    c: *const PunktfunkConnection,
    grants: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: out-param is optional; null-checked before write.
        unsafe {
            if !grants.is_null() {
                *grants = c.inner.access_grants();
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Seconds until access expires. `0` = permanent. While a deadline is set the
/// value never reads `0` (clamps to 1 past expiry until the typed close).
/// Anchored to the client clock at receipt.
///
/// # Safety
/// `c` is a valid connection handle; `secs` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_access_expires_in(
    c: *const PunktfunkConnection,
    secs: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let remaining = match c.inner.access_deadline_unix() {
            None => 0,
            Some(deadline) => {
                let now = crate::quic::wall_clock_ns() / 1_000_000_000;
                // Clamp to ≥ 1 while a deadline is set: 0 means "permanent", never "expired".
                u32::try_from(deadline.saturating_sub(now))
                    .unwrap_or(u32::MAX)
                    .max(1)
            }
        };
        // SAFETY: out-param is optional; null-checked before write.
        unsafe {
            if !secs.is_null() {
                *secs = remaining;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// The sentence the host sent with its mid-session rejection, NUL-terminated, into
/// the caller's buffer; empty when it sent none — render the client's own wording
/// for the code then. Ask alongside [`punktfunk_connection_end_reject`]. A 512-byte
/// buffer is ample: the wire caps this at 256.
///
/// Already stripped of control characters and capped by the core; a host cannot make
/// this longer or make it move a terminal's cursor.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for `cap` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_end_reject_said(
    c: *const PunktfunkConnection,
    out: *mut c_char,
    cap: usize,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() || cap == 0 {
            return PunktfunkStatus::NullPointer;
        }
        let said = c.inner.end_reject_said().unwrap_or_default();
        if said.len() + 1 > cap {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: `out` is non-null and holds `cap` >= said.len() + 1 bytes.
        unsafe {
            // `.cast()`: `c_char` is i8 on x86_64 and u8 on aarch64.
            std::ptr::copy_nonoverlapping(said.as_ptr(), out.cast::<u8>(), said.len());
            *out.add(said.len()) = 0;
        }
        PunktfunkStatus::Ok
    })
}

/// The host's sentence when this session's launch did not give the player their game,
/// NUL-terminated, into the caller's buffer; empty otherwise. The latest verdict wins, so
/// poll it. A 256-byte buffer is ample: the wire caps this at 200.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for `cap` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_launch_notice(
    c: *const PunktfunkConnection,
    out: *mut c_char,
    cap: usize,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() || cap == 0 {
            return PunktfunkStatus::NullPointer;
        }
        let outcome = c.inner.launch_outcome();
        let notice = outcome
            .as_ref()
            .and_then(|o| o.notice())
            .unwrap_or_default();
        if notice.len() + 1 > cap {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: `out` is non-null and holds `cap` >= notice.len() + 1 bytes.
        unsafe {
            // `.cast()`: `c_char` is i8 on x86_64 and u8 on aarch64.
            std::ptr::copy_nonoverlapping(notice.as_ptr(), out.cast::<u8>(), notice.len());
            *out.add(notice.len()) = 0;
        }
        PunktfunkStatus::Ok
    })
}

/// Mid-session typed rejection (`PUNKTFUNK_STATUS_REJECTED_*`); `0` = none.
/// Ask after `Closed`, before free. Connect-time rejections come from connect.
///
/// # Safety
/// `c` is a valid connection handle; `status` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_end_reject(
    c: *const PunktfunkConnection,
    status: *mut i32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let value = match c.inner.end_reject() {
            Some(reason) => crate::error::PunktfunkError::Rejected(reason).status() as i32,
            None => 0,
        };
        // SAFETY: out-param is optional; null-checked before write.
        unsafe {
            if !status.is_null() {
                *status = value;
            }
        }
        PunktfunkStatus::Ok
    })
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.clip_control(enabled, flags) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if kinds.is_null() && n != 0 {
            return PunktfunkStatus::NullPointer;
        }
        if n > crate::quic::CLIP_MAX_KINDS || ffi_slice_bytes::<PunktfunkClipKind>(n).is_none() {
            return PunktfunkStatus::InvalidArg;
        }
        let mut out = Vec::with_capacity(n);
        if n != 0 {
            // SAFETY: `n` is capped and `ffi_slice_bytes`-checked; borrowed for this call.
            let slice = unsafe { std::slice::from_raw_parts(kinds, n) };
            for k in slice {
                let mime = if k.mime.is_null() {
                    String::new()
                } else {
                    // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
                    match unsafe { std::ffi::CStr::from_ptr(k.mime) }.to_str() {
                        Ok(s) => s.to_string(),
                        Err(_) => return PunktfunkStatus::InvalidArg,
                    }
                };
                out.push(crate::quic::ClipKind {
                    mime,
                    size_hint: k.size_hint,
                });
            }
        }
        match c.inner.clip_offer(seq, out) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if mime.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        // SAFETY: caller C string, NUL-terminated or null; borrowed for this call only.
        let mime = match unsafe { std::ffi::CStr::from_ptr(mime) }.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return PunktfunkStatus::InvalidArg,
        };
        match c.inner.clip_fetch(seq, mime, file_index) {
            Ok(xfer_id) => {
                // SAFETY: each out-param is optional; null-checked before write.
                unsafe {
                    if !xfer_id_out.is_null() {
                        *xfer_id_out = xfer_id;
                    }
                }
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if data.is_null() && len != 0 {
            return PunktfunkStatus::NullPointer;
        }
        if ffi_slice_bytes::<u8>(len).is_none() {
            return PunktfunkStatus::InvalidArg;
        }
        let bytes = if len == 0 {
            Vec::new()
        } else {
            // SAFETY: the ABI contract supplies `len` readable bytes; `ffi_slice_bytes` proved the
            // extent is representable by a Rust slice, copied before this call returns.
            unsafe { std::slice::from_raw_parts(data, len) }.to_vec()
        };
        match c.inner.clip_serve(req_id, bytes, last) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.clip_cancel(id) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
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
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
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

/// Compositor the host resolved (`PUNKTFUNK_COMPOSITOR_*`; Welcome echo of
/// [`punktfunk_connect_ex`]). `AUTO` = a host that didn't say. Gamescope PipeWire
/// capture carries no cursor — default to a client-side cursor there.
///
/// # Safety
/// `c` is a valid connection handle; `compositor` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_compositor(
    c: *const PunktfunkConnection,
    compositor: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: each out-param is optional; null-checked before write.
        unsafe {
            if !compositor.is_null() {
                *compositor = c.inner.resolved_compositor.to_u8() as u32;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Video encoder bitrate (kbps) the host configured — the [`punktfunk_connect_ex3`]
/// request clamped to the host range, or its default when `0` was requested.
/// `0` = a host that didn't report it. Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; `bitrate_kbps` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_bitrate(
    c: *const PunktfunkConnection,
    bitrate_kbps: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: each out-param is optional; null-checked before write.
        unsafe {
            if !bitrate_kbps.is_null() {
                *bitrate_kbps = c.inner.resolved_bitrate_kbps;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Connect-time wall-clock offset, ns, host minus client. Add to a local
/// realtime stamp to express it in the host capture clock (`pts_ns`). `0` = none.
///
/// # Safety
/// `c` is a valid connection handle; `offset_ns` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_clock_offset_ns(
    c: *const PunktfunkConnection,
    offset_ns: *mut i64,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: each out-param is optional; null-checked before write.
        unsafe {
            if !offset_ns.is_null() {
                *offset_ns = c.inner.clock_offset_ns;
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Live wall-clock offset (updated by mid-stream re-sync). Use this for ongoing
/// latency math, not the frozen connect-time value.
///
/// # Safety
/// `c` is a valid connection handle; `offset_ns` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_clock_offset_now_ns(
    c: *const PunktfunkConnection,
    offset_ns: *mut i64,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: each out-param is optional; null-checked before write.
        unsafe {
            if !offset_ns.is_null() {
                *offset_ns = c.inner.clock_offset_now_ns();
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Request a live mode switch. On accept, the first new-mode AU is an IDR with
/// in-band parameter sets — rebuild the decoder from it.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_request_mode(
    c: *const PunktfunkConnection,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.request_mode(crate::config::Mode {
            width,
            height,
            refresh_hz,
        }) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Request an IDR now. Infinite GOP has one opening IDR; throttle — a wedged
/// decoder stays stuck for several frames, so per-frame requests flood control.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_request_keyframe(
    c: *const PunktfunkConnection,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.request_keyframe() {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Ask the host to recover `[first_frame, last_frame]` by RFI (P-frame tagged
/// `USER_FLAG_RECOVERY_ANCHOR`) instead of a full IDR. Throttle; keyframe is the backstop.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_request_rfi(
    c: *const PunktfunkConnection,
    first_frame: u32,
    last_frame: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.request_rfi(first_frame, last_frame) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Note each received `frame_index`. A forward gap fires throttled RFI; keyframe
/// on `frames_dropped` is the backstop. `gap_out` (nullable) is whether a gap was seen.
///
/// # Safety
/// `c` is a valid connection handle; `gap_out` is writable or NULL.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_note_frame_index(
    c: *const PunktfunkConnection,
    frame_index: u32,
    gap_out: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let gap = c.inner.note_frame_index(frame_index);
        if !gap_out.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *gap_out = gap > 0 };
        }
        PunktfunkStatus::Ok
    })
}

/// [`punktfunk_connection_note_frame_index`] with the gap width: writes how many
/// frames this arrival revealed as missing (0 = contiguous/straggler). Pass the
/// width to [`punktfunk_reanchor_gate_arm_expecting_drops`] so a later
/// `frames_dropped` climb for the same loss cannot re-freeze a healed stream.
///
/// # Safety
/// `c` is a valid connection handle; `gap_width_out` is writable or NULL.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_note_frame_index_ex(
    c: *const PunktfunkConnection,
    frame_index: u32,
    gap_width_out: *mut u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let gap = c.inner.note_frame_index(frame_index);
        if !gap_width_out.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *gap_width_out = gap };
        }
        PunktfunkStatus::Ok
    })
}

/// Unrecoverable reassembler drops. Poll and request a keyframe when it climbs —
/// infinite GOP conceals missing refs with no decode error. Writes 0 on NULL.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_frames_dropped(
    c: *const PunktfunkConnection,
    out: *mut u64,
) -> PunktfunkStatus {
    guard(|| {
        // Write 0 on a NULL connection before the handle check (header contract).
        if !out.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *out = 0 };
        }
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: each out-param is optional; null-checked before write.
        unsafe {
            if !out.is_null() {
                *out = c.inner.frames_dropped();
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Facts only the embedder knows, for [`punktfunk_connection_hud_text`]. Zero-init, set
/// `struct_size = sizeof(PunktfunkHudFacts)`, then the fields you mean. Append only; bump ABI.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHudFacts {
    /// `sizeof(PunktfunkHudFacts)` as this caller was compiled.
    pub struct_size: u32,
    /// The displayed stamps are true on-glass instants.
    pub on_glass: bool,
    /// Take the measured OS present floor off end-to-end and display (iOS, tvOS).
    pub shave_os_floor: bool,
    /// Decoded audio queued ahead of the speaker, ms, for an embedder that plays audio itself.
    /// `0` keeps the core's reading.
    pub audio_buffer_ms: u32,
    /// Where that buffer puts audio against the picture, ms; positive = audio behind.
    pub av_offset_ms: i32,
    /// NUL-terminated preset name, or null.
    pub preset: *const c_char,
    /// Embedder-only Advanced Detailed lines, `<role>\t<text>\n` each, or null.
    pub extras: *const c_char,
}

#[cfg(all(feature = "quic", target_pointer_width = "64"))]
const _: () = assert!(core::mem::size_of::<PunktfunkHudFacts>() == 32);

/// [`punktfunk_connection_hud_text`]'s facts applied to a drained window.
///
/// # Safety
/// `facts` is null or points to at least its declared `struct_size` bytes, and its strings are
/// NUL-terminated or null.
#[cfg(feature = "quic")]
unsafe fn hud_with_facts(
    mut s: crate::hud::StatsSnapshot,
    facts: *const PunktfunkHudFacts,
) -> Result<crate::hud::StatsSnapshot, PunktfunkStatus> {
    if facts.is_null() {
        return Ok(s);
    }
    // SAFETY: `addr_of!` does not form a `&`; a shorter layout is rejected before the full read.
    let declared = unsafe { std::ptr::addr_of!((*facts).struct_size).read_unaligned() } as usize;
    if declared < std::mem::size_of::<PunktfunkHudFacts>() {
        return Err(PunktfunkStatus::InvalidArg);
    }
    // SAFETY: non-null, and `struct_size` covers this type.
    let f = unsafe { facts.read_unaligned() };
    s.on_glass = f.on_glass;
    s.shave_os_floor = f.shave_os_floor;
    if f.audio_buffer_ms > 0 {
        s.audio_buffer_ms = f.audio_buffer_ms;
        s.av_offset_ms = f.av_offset_ms;
    }
    // SAFETY: caller strings, NUL-terminated or null, borrowed for this call.
    let preset = unsafe { opt_cstr(f.preset) }.map_err(|()| PunktfunkStatus::InvalidArg)?;
    s.preset = preset.filter(|p| !p.is_empty()).map(str::to_owned);
    // SAFETY: as above.
    let extras = unsafe { opt_cstr(f.extras) }.map_err(|()| PunktfunkStatus::InvalidArg)?;
    s.extras
        .extend(extras.unwrap_or_default().lines().filter_map(|line| {
            let (code, text) = line.split_once('\t')?;
            Some(crate::hud::Extra {
                text: text.to_owned(),
                tier: crate::hud::StatsVerbosity::Detailed,
                advanced_only: true,
                role: crate::hud::Role::from_code(code.parse().ok()?),
            })
        }));
    Ok(s)
}

/// Stats overlay: one frame left the decoder. `pts_ns` is its capture stamp; `received_ns` (the
/// AU's reassembly stamp) and `decoded_ns` are client `CLOCK_REALTIME`. A zero `received_ns`
/// counts the frame without a decode sample.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_hud_decoded(
    c: *const PunktfunkConnection,
    pts_ns: u64,
    received_ns: u64,
    decoded_ns: u64,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let hud = c.inner.hud();
        hud.note_decoded(pts_ns, decoded_ns);
        if received_ns > 0 && decoded_ns >= received_ns {
            hud.note_decode_us((decoded_ns - received_ns) / 1000, false);
        }
        PunktfunkStatus::Ok
    })
}

/// Stats overlay: one frame reached the screen at `displayed_ns`, client `CLOCK_REALTIME`.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_hud_displayed(
    c: *const PunktfunkConnection,
    pts_ns: u64,
    decoded_ns: u64,
    displayed_ns: u64,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        c.inner
            .hud()
            .note_displayed(pts_ns, decoded_ns, 0, displayed_ns);
        PunktfunkStatus::Ok
    })
}

/// Stats overlay: one sample of the OS present pipeline's depth, ns: how far ahead of glass the
/// compositor takes a frame. Shaved off the shown figures when the facts ask for it.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_hud_os_floor(
    c: *const PunktfunkConnection,
    floor_ns: u64,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        c.inner.hud().note_os_floor_us(floor_ns / 1000);
        PunktfunkStatus::Ok
    })
}

/// Close the stats overlay's window, about once a second. [`punktfunk_connection_hud_text`]
/// formats what this kept as often as the tier changes.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_hud_drain(
    c: *const PunktfunkConnection,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        *lock_recover(&c.hud_snap) = c.inner.hud_snapshot();
        PunktfunkStatus::Ok
    })
}

/// The last drained window as overlay lines, `<role>\t<text>\n` each (role 0 primary, 1 detail,
/// 2 muted, 3 warning), NUL-terminated into `out`. `tier` is 0 off to 3 detailed; `advanced`
/// picks the Advanced vocabulary; `facts` may be null. When `cap` is too small nothing is
/// written and the status is `InvalidArg`; `*needed` (when non-null) always holds the size.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for `cap` bytes; `facts` is null or
/// valid per [`PunktfunkHudFacts`]; `needed` is null or writable.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_hud_text(
    c: *const PunktfunkConnection,
    tier: u32,
    advanced: bool,
    facts: *const PunktfunkHudFacts,
    out: *mut c_char,
    cap: usize,
    needed: *mut usize,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        let snap = lock_recover(&c.hud_snap).clone();
        // SAFETY: the caller's `facts` contract, checked inside.
        let snap = match unsafe { hud_with_facts(snap, facts) } {
            Ok(s) => s,
            Err(status) => return status,
        };
        let tier = crate::hud::StatsVerbosity::from_index(tier);
        let text = crate::hud::encode_lines(&crate::hud::format(&snap, tier, advanced));
        if !needed.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *needed = text.len() + 1 };
        }
        if out.is_null() || text.len() + 1 > cap {
            return PunktfunkStatus::InvalidArg;
        }
        // SAFETY: `out` is non-null and holds `cap` >= text.len() + 1 bytes.
        unsafe {
            // `.cast()`: `c_char` is i8 on x86_64 and u8 on aarch64.
            std::ptr::copy_nonoverlapping(text.as_ptr(), out.cast::<u8>(), text.len());
            *out.add(text.len()) = 0;
        }
        PunktfunkStatus::Ok
    })
}

/// Decode-stage latency in µs: AU leave [`next_au`] to decoded output. Include
/// decoder-input backlog; exclude vsync wait. Feeds Automatic bitrate. Skip if
/// [`punktfunk_connection_wants_decode_latency`] is false.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_report_decode_us(
    c: *const PunktfunkConnection,
    us: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        c.inner.report_decode_us(us);
        PunktfunkStatus::Ok
    })
}

/// Report the display-latch grid (`design/phase-locked-capture.md`).
/// `next_latch_host_ns` is already host clock. ~1 Hz; no-op if unnegotiated.
///
/// # Safety
/// `c` is a caller handle or null (error, not UB).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_report_phase(
    c: *const PunktfunkConnection,
    next_latch_host_ns: u64,
    latch_period_ns: u32,
    uncertainty_ns: u32,
    arrival_lead_ns: u32,
    coherence_milli: u16,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        c.inner.report_phase(
            next_latch_host_ns,
            latch_period_ns,
            uncertainty_ns,
            arrival_lead_ns,
            coherence_milli,
        );
        PunktfunkStatus::Ok
    })
}

/// Whether [`punktfunk_connection_report_decode_us`] is worth calling: writes true
/// only when Automatic bitrate is armed (non-PyroWave). Skip the per-frame
/// measurement otherwise. Constant for the session. Writes false on a NULL connection.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_wants_decode_latency(
    c: *const PunktfunkConnection,
    out: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // Write false on a NULL connection before the handle check (uninitialized is not a bool).
        if !out.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *out = false };
        }
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: each out-param is optional; null-checked before write.
        unsafe {
            if !out.is_null() {
                *out = c.inner.wants_decode_latency();
            }
        }
        PunktfunkStatus::Ok
    })
}

/// Speed-test measurement from [`punktfunk_connection_probe_result`]. `done` is 0
/// until the host's end-of-burst report, then 1. `throughput_kbps` is delivered
/// wire throughput; `loss_pct` is link loss; `host_drop_pct` is send-buffer drop
/// (raise `net.core.wmem_max`). Measured separately so a host that can't keep up
/// reads differently from a lossy link.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PunktfunkProbeResult {
    /// 1 once the host's end-of-burst report arrived (measurement final); else 0 (partial).
    pub done: u8,
    /// Delivered wire bytes (header + shard) / packets the client received during the burst.
    pub recv_bytes: u64,
    pub recv_packets: u32,
    /// Application goodput bytes / access units the host offered.
    pub host_bytes: u64,
    pub host_packets: u32,
    /// Throughput denominator, ms: client-measured burst receive interval, live
    /// while bursting and final once `done`; host send-window duration when fewer
    /// than two probe packets arrived. Host duration alone overstates throughput —
    /// its window closes while the bottleneck queue is still draining.
    pub elapsed_ms: u32,
    /// Delivered wire throughput = `recv_bytes * 8 / elapsed_ms` (kilobits/second).
    pub throughput_kbps: u32,
    /// Link loss `(wire_packets_sent − recv_packets) / wire_packets_sent` as a percentage.
    pub loss_pct: f32,
    /// Host-side send-buffer drop `send_dropped / (wire_packets_sent + send_dropped)`, percent.
    pub host_drop_pct: f32,
    /// Wire packets the host put on the link, and the ones its send buffer dropped.
    pub wire_packets_sent: u32,
    pub send_dropped: u32,
}

/// Start a bandwidth speed test: host bursts filler at `target_kbps` goodput for
/// `duration_ms` (clamped ≤ 10 Gbps / ≤ 5 s), briefly pausing video. Non-blocking —
/// poll [`punktfunk_connection_probe_result`] until `done` is 1. Starting a probe
/// resets any prior measurement.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_speed_test(
    c: *const PunktfunkConnection,
    target_kbps: u32,
    duration_ms: u32,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        match c.inner.request_probe(target_kbps, duration_ms) {
            Ok(()) => PunktfunkStatus::Ok,
            Err(e) => e.status(),
        }
    })
}

/// Read the current speed-test measurement into `*out` (partial until `out->done == 1`).
/// Safe to poll after [`punktfunk_connection_speed_test`]; before any probe it reports zeros.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkProbeResult`
/// (NULL is an error).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_probe_result(
    c: *const PunktfunkConnection,
    out: *mut PunktfunkProbeResult,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let o = c.inner.probe_result();
        // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
        unsafe {
            *out = PunktfunkProbeResult {
                done: o.done as u8,
                recv_bytes: o.recv_bytes,
                recv_packets: o.recv_packets,
                host_bytes: o.host_bytes,
                host_packets: o.host_packets,
                elapsed_ms: o.elapsed_ms,
                throughput_kbps: o.throughput_kbps,
                loss_pct: o.loss_pct,
                host_drop_pct: o.host_drop_pct,
                wire_packets_sent: o.wire_packets_sent,
                send_dropped: o.send_dropped,
            };
        }
        PunktfunkStatus::Ok
    })
}

/// Signal a deliberate quit (user stop, not a network drop) before closing: the
/// connection closes with [`QUIT_CLOSE_CODE`] instead of 0, so the host skips the
/// keep-alive linger. Call before [`punktfunk_connection_close`] on a user disconnect;
/// a plain close leaves the linger intact. NULL is a no-op.
///
/// # Safety
/// `c` was returned by [`punktfunk_connect`] and remains valid until `punktfunk_connection_close`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_disconnect_quit(c: *mut PunktfunkConnection) {
    guard_void(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        if let Some(c) = unsafe { c.as_ref() } {
            c.inner.disconnect_quit();
        }
    });
}

/// Close the connection and free the handle (joins the internal threads). NULL is a no-op.
///
/// # Safety
/// `c` was returned by [`punktfunk_connect`] and is not used after this call.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_close(c: *mut PunktfunkConnection) {
    guard_void(|| {
        if !c.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(c) });
        }
    });
}

// C wrapper for [`H265Concealer`]: a platform decoder that reads the RPS itself (VideoToolbox)
// gets every access unit through `conceal` first, in decode order, and decodes what it says.

/// Outcome of [`punktfunk_h265_concealer_conceal`].
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PunktfunkConcealment {
    /// Decode the access unit as it came.
    Intact = 0,
    /// Decode the returned bytes instead: a missing current reference was moved to a picture
    /// the decoder holds.
    Rewritten = 1,
    /// Keep this and every following delta off the decoder and ask for an IDR.
    Unrecoverable = 2,
}

/// Opaque handle: one elementary stream's [`H265Concealer`]. The type lives in another crate,
/// so the header needs a core-owned name for it.
pub struct PunktfunkH265Concealer {
    inner: H265Concealer,
}

/// Create an HEVC reference concealer for one elementary stream (IDR first). Free with
/// [`punktfunk_h265_concealer_free`]. Never returns NULL.
#[unsafe(no_mangle)]
pub extern "C" fn punktfunk_h265_concealer_new() -> *mut PunktfunkH265Concealer {
    Box::into_raw(Box::new(PunktfunkH265Concealer {
        inner: H265Concealer::new(),
    }))
}

/// Free a concealer created by [`punktfunk_h265_concealer_new`]. NULL is a no-op.
///
/// # Safety
/// `c` was returned by [`punktfunk_h265_concealer_new`] and is not used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_h265_concealer_free(c: *mut PunktfunkH265Concealer) {
    guard_void(|| {
        if !c.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(c) });
        }
    });
}

/// Fold one Annex-B access unit.
///
/// `out_kind` says what to decode. For `Rewritten`, `out_buf` and `out_len`
/// hold the bytes until [`punktfunk_h265_concealer_release`]. A length that
/// cannot fit a Rust slice returns [`PunktfunkStatus::InvalidArg`].
///
/// # Safety
/// `c` is a valid handle; `au` points to `len` readable bytes; the out pointers are writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_h265_concealer_conceal(
    c: *mut PunktfunkH265Concealer,
    au: *const u8,
    len: usize,
    out_kind: *mut PunktfunkConcealment,
    out_buf: *mut *mut u8,
    out_len: *mut usize,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let (Some(c), Some(out_kind), Some(out_buf), Some(out_len)) = (
            unsafe { c.as_mut() },
            unsafe { out_kind.as_mut() },
            unsafe { out_buf.as_mut() },
            unsafe { out_len.as_mut() },
        ) else {
            return PunktfunkStatus::NullPointer;
        };
        if au.is_null() && len != 0 {
            return PunktfunkStatus::NullPointer;
        }
        if ffi_slice_bytes::<u8>(len).is_none() {
            return PunktfunkStatus::InvalidArg;
        }
        let bytes: &[u8] = if len == 0 {
            &[]
        } else {
            // SAFETY: `au` is non-null and `ffi_slice_bytes` proved the extent fits a Rust slice.
            unsafe { std::slice::from_raw_parts(au, len) }
        };
        *out_buf = std::ptr::null_mut();
        *out_len = 0;
        *out_kind = match c.inner.conceal(bytes) {
            Concealment::Intact => PunktfunkConcealment::Intact,
            Concealment::Rewritten(v) => {
                let boxed = v.into_boxed_slice();
                *out_len = boxed.len();
                *out_buf = Box::into_raw(boxed).cast::<u8>();
                PunktfunkConcealment::Rewritten
            }
            Concealment::Unrecoverable => PunktfunkConcealment::Unrecoverable,
        };
        PunktfunkStatus::Ok
    })
}

/// Release a buffer [`punktfunk_h265_concealer_conceal`] returned. NULL is a no-op.
///
/// # Safety
/// `buf`/`len` are exactly what one `conceal` call returned, released once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_h265_concealer_release(buf: *mut u8, len: usize) {
    guard_void(|| {
        if !buf.is_null() {
            // SAFETY: the pair came from `Box::<[u8]>::into_raw` above, released once.
            drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(buf, len)) });
        }
    });
}

// C wrapper for [`ReanchorGate`]. Time stays inside (`Instant::now`).
// `arm` on loss, `on_decoded` per frame, `on_no_output` per empty AU, `poll` each tick.

/// Create a re-anchor gate seeded with the session's current `frames_dropped` (so
/// the first [`punktfunk_reanchor_gate_poll`] doesn't read the baseline as a loss).
/// Free with [`punktfunk_reanchor_gate_free`]. Never returns NULL.
#[unsafe(no_mangle)]
pub extern "C" fn punktfunk_reanchor_gate_new(frames_dropped: u64) -> *mut ReanchorGate {
    Box::into_raw(Box::new(ReanchorGate::new(frames_dropped)))
}

/// Free a gate created by [`punktfunk_reanchor_gate_new`]. NULL is a no-op.
///
/// # Safety
/// `g` was returned by [`punktfunk_reanchor_gate_new`] and is not used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_free(g: *mut ReanchorGate) {
    guard_void(|| {
        if !g.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(g) });
        }
    });
}

/// Arm the freeze: a loss was detected (frame-index gap, or decoder wedge/demotion).
/// Zeroes the recovery-mark count and (re-)sets the backstop deadline. NULL is a no-op.
///
/// # Safety
/// `g` is a valid gate handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_arm(g: *mut ReanchorGate) {
    guard_void(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        if let Some(g) = unsafe { g.as_mut() } {
            g.arm(std::time::Instant::now());
        }
    });
}

/// Arm for a frame-index gap, pre-crediting `expected_drops` so a later
/// `frames_dropped` climb is not a second loss (double-arm race). Plain `arm`
/// for decoder wedge/demotion. NULL is a no-op.
///
/// # Safety
/// `g` is a valid gate handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_arm_expecting_drops(
    g: *mut ReanchorGate,
    expected_drops: u64,
) {
    guard_void(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        if let Some(g) = unsafe { g.as_mut() } {
            g.arm_expecting_drops(std::time::Instant::now(), expected_drops);
        }
    });
}

/// Fold one decoded frame; `out_present` is whether to display it. Reads `FLAG_SOF`,
/// `USER_FLAG_RECOVERY_ANCHOR`, `USER_FLAG_RECOVERY_POINT`. Platform decoders that
/// do not flag IDRs pass `decoder_keyframe = false` — wire `FLAG_SOF` covers it.
/// Uncorroborated: C callers cannot supply bitstream evidence.
///
/// # Safety
/// `g` is a valid gate handle; `out_present` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_on_decoded(
    g: *mut ReanchorGate,
    flags: u32,
    decoder_keyframe: bool,
    out_present: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let g = match unsafe { g.as_mut() } {
            Some(g) => g,
            None => return PunktfunkStatus::NullPointer,
        };
        let present = g.on_decoded(flags, decoder_keyframe, std::time::Instant::now())
            == GateVerdict::Present;
        if !out_present.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *out_present = present };
        }
        PunktfunkStatus::Ok
    })
}

/// A received AU produced no decoded frame. Writes to `out_request_kf` whether the
/// no-output streak has tripped and the client should (throttled) request a keyframe
/// — the gate arms the freeze at the same time.
///
/// # Safety
/// `g` is a valid gate handle; `out_request_kf` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_on_no_output(
    g: *mut ReanchorGate,
    out_request_kf: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let g = match unsafe { g.as_mut() } {
            Some(g) => g,
            None => return PunktfunkStatus::NullPointer,
        };
        let request = g.on_no_output(std::time::Instant::now());
        if !out_request_kf.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *out_request_kf = request };
        }
        PunktfunkStatus::Ok
    })
}

/// Periodic fold of `frames_dropped` plus the overdue backstop. Writes to
/// `out_request_kf` whether the client should (throttled) request a keyframe
/// (a drop-count climb armed a freeze, or the freeze is overdue and re-asks).
///
/// # Safety
/// `g` is a valid gate handle; `out_request_kf` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_poll(
    g: *mut ReanchorGate,
    frames_dropped: u64,
    out_request_kf: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let g = match unsafe { g.as_mut() } {
            Some(g) => g,
            None => return PunktfunkStatus::NullPointer,
        };
        let request = g.poll(frames_dropped, std::time::Instant::now());
        if !out_request_kf.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *out_request_kf = request };
        }
        PunktfunkStatus::Ok
    })
}

/// Whether the gate is currently withholding concealed frames (frozen on the last
/// good picture). Writes `false` on a NULL gate.
///
/// # Safety
/// `g` is a valid gate handle; `out_holding` is writable or NULL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_reanchor_gate_is_holding(
    g: *const ReanchorGate,
    out_holding: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let holding = unsafe { g.as_ref() }.is_some_and(ReanchorGate::is_holding);
        if !out_holding.is_null() {
            // SAFETY: caller out-param, non-null on this path, written once.
            unsafe { *out_holding = holding };
        }
        PunktfunkStatus::Ok
    })
}

// Demo host: a loopback host the embedder feeds for its demo mode (`crate::demo_host`).

/// A running demo host from [`punktfunk_demo_host_start`].
#[cfg(feature = "quic")]
pub struct PunktfunkDemoHost {
    inner: crate::demo_host::DemoHost,
}

/// The demo session to render, from [`punktfunk_demo_host_session`].
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PunktfunkDemoSession {
    /// Bumped on each new session and accepted mode switch: rebuild the encoder, start on an IDR.
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub bitrate_kbps: u32,
    /// `PUNKTFUNK_CODEC_H264` or `PUNKTFUNK_CODEC_HEVC`.
    pub codec: u8,
}

/// Start a demo host on a free loopback port. `codecs` is the `PUNKTFUNK_CODEC_*` mask the
/// embedder can encode. NULL on failure. Free with [`punktfunk_demo_host_stop`].
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub extern "C" fn punktfunk_demo_host_start(codecs: u8) -> *mut PunktfunkDemoHost {
    let started = std::panic::catch_unwind(|| crate::demo_host::DemoHost::start(codecs));
    match started {
        Ok(Ok(inner)) => Box::into_raw(Box::new(PunktfunkDemoHost { inner })),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "demo host did not start");
            ptr::null_mut()
        }
        Err(_) => ptr::null_mut(),
    }
}

/// Stop the host, ending any session, and free the handle. NULL is a no-op.
///
/// # Safety
/// `h` came from [`punktfunk_demo_host_start`] and is not used after this call.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_demo_host_stop(h: *mut PunktfunkDemoHost) {
    guard_void(|| {
        if !h.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(h) });
        }
    });
}

/// The loopback UDP port the host listens on. 0 for NULL.
///
/// # Safety
/// `h` is a live handle or NULL.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_demo_host_port(h: *const PunktfunkDemoHost) -> u16 {
    // SAFETY: caller handle or null; `as_ref` never dereferences null.
    unsafe { h.as_ref() }.map_or(0, |h| h.inner.port())
}

/// Write the host certificate's SHA-256 to `out_sha256`: the pin to dial it with.
///
/// # Safety
/// `h` is a live handle; `out_sha256` is writable for 32 bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_demo_host_fingerprint(
    h: *const PunktfunkDemoHost,
    out_sha256: *mut u8,
) -> PunktfunkStatus {
    guard(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let Some(h) = (unsafe { h.as_ref() }) else {
            return PunktfunkStatus::NullPointer;
        };
        if out_sha256.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        // SAFETY: the caller guarantees 32 writable bytes; non-null on this path.
        unsafe { ptr::copy_nonoverlapping(h.inner.fingerprint().as_ptr(), out_sha256, 32) };
        PunktfunkStatus::Ok
    })
}

/// Write the live session to `out` and return true. False, with `out` untouched, while no
/// client is streaming.
///
/// # Safety
/// `h` is a live handle; `out` is writable.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_demo_host_session(
    h: *const PunktfunkDemoHost,
    out: *mut PunktfunkDemoSession,
) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let Some(h) = (unsafe { h.as_ref() }) else {
            return false;
        };
        let Some(s) = h.inner.session().filter(|_| !out.is_null()) else {
            return false;
        };
        // SAFETY: caller out-param, non-null on this path, written once.
        unsafe {
            *out = PunktfunkDemoSession {
                generation: s.generation,
                width: s.mode.width,
                height: s.mode.height,
                refresh_hz: s.mode.refresh_hz,
                bitrate_kbps: s.bitrate_kbps,
                codec: s.codec,
            }
        };
        true
    }))
    .unwrap_or(false)
}

/// Copy the live session's launch id, NUL-terminated, into `buf`. Returns its length without
/// the NUL; 0 when no session or no launch. Writes nothing when `cap` < length + 1.
///
/// # Safety
/// `h` is a live handle; `buf` is writable for `cap` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_demo_host_launch(
    h: *const PunktfunkDemoHost,
    buf: *mut c_char,
    cap: usize,
) -> usize {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let Some(h) = (unsafe { h.as_ref() }) else {
            return 0;
        };
        let Some(launch) = h.inner.session().and_then(|s| s.launch) else {
            return 0;
        };
        if !buf.is_null() && launch.len() < cap {
            // SAFETY: `buf` is writable for `cap` > `launch.len()` bytes.
            unsafe {
                ptr::copy_nonoverlapping(launch.as_ptr(), buf.cast::<u8>(), launch.len());
                *buf.add(launch.len()) = 0;
            }
        }
        launch.len()
    }))
    .unwrap_or(0)
}

/// True once per client keyframe request or dropped AU: encode the next frame as an IDR.
///
/// # Safety
/// `h` is a live handle or NULL.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_demo_host_take_keyframe_request(
    h: *const PunktfunkDemoHost,
) -> bool {
    // SAFETY: caller handle or null; `as_ref` never dereferences null.
    unsafe { h.as_ref() }.is_some_and(|h| h.inner.take_keyframe_request())
}

/// Pop the oldest input event the client sent into `out`. False when none is waiting.
///
/// # Safety
/// `h` is a live handle; `out` is writable.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_demo_host_next_input(
    h: *const PunktfunkDemoHost,
    out: *mut InputEvent,
) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let Some(h) = (unsafe { h.as_ref() }) else {
            return false;
        };
        if out.is_null() {
            return false;
        }
        let Some(ev) = h.inner.next_input() else {
            return false;
        };
        // SAFETY: caller out-param, non-null on this path, written once.
        unsafe { *out = ev };
        true
    }))
    .unwrap_or(false)
}

/// Queue one Annex-B access unit, parameter sets in-band on an IDR. False: no session, or the
/// queue was full and the AU dropped — make the next frame an IDR.
///
/// # Safety
/// `h` is a live handle; `data` is readable for `len` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_demo_host_submit_video(
    h: *const PunktfunkDemoHost,
    data: *const u8,
    len: usize,
    keyframe: bool,
) -> bool {
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller handle or null; `as_ref` never dereferences null.
        let Some(h) = (unsafe { h.as_ref() }) else {
            return false;
        };
        if data.is_null() || ffi_slice_bytes::<u8>(len).is_none() {
            return false;
        }
        // SAFETY: the caller guarantees `len` readable bytes at non-null `data`.
        let au = unsafe { std::slice::from_raw_parts(data, len) };
        h.inner.submit_video(au, keyframe)
    }))
    .unwrap_or(false)
}

#[cfg(test)]
mod abi_version_tests {
    /// Pin [`crate::ABI_VERSION`]. A bump must update this test in the same change.
    #[test]
    fn abi_version_is_pinned() {
        // Current ABI. A bump must update this pin.
        assert_eq!(crate::ABI_VERSION, 38);
        assert_eq!(super::punktfunk_abi_version(), 38);
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
        assert_eq!(status, crate::error::PunktfunkStatus::InvalidArg);
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

    /// The facts land on the snapshot; a short `struct_size` is a status, not a read.
    #[cfg(feature = "quic")]
    #[test]
    fn hud_facts_apply_to_a_drained_window() {
        let preset = std::ffi::CString::new("Work").unwrap();
        let extras =
            std::ffi::CString::new("2\tlink latency ask 1.00\nno tab\n3\tclock\n").unwrap();
        // SAFETY: an all-zero struct is a valid value (null pointers, zero scalars).
        let mut f: PunktfunkHudFacts = unsafe { std::mem::zeroed() };
        f.struct_size = std::mem::size_of::<PunktfunkHudFacts>() as u32;
        f.on_glass = true;
        f.shave_os_floor = true;
        f.audio_buffer_ms = 28;
        f.av_offset_ms = -3;
        f.preset = preset.as_ptr();
        f.extras = extras.as_ptr();
        // SAFETY: `f` and its strings outlive the call.
        let s = unsafe { hud_with_facts(Default::default(), &f) }.unwrap();
        assert!(s.on_glass && s.shave_os_floor);
        assert_eq!((s.audio_buffer_ms, s.av_offset_ms), (28, -3));
        assert_eq!(s.preset.as_deref(), Some("Work"));
        assert_eq!(s.extras.len(), 2);
        assert_eq!(s.extras[1].role, crate::hud::Role::Warn);
        assert!(s.extras.iter().all(|e| e.advanced_only));
        f.struct_size = 8;
        // SAFETY: as above; the short size is the documented rejected case.
        let short = unsafe { hud_with_facts(Default::default(), &f) };
        assert_eq!(short.unwrap_err(), PunktfunkStatus::InvalidArg);
        // SAFETY: null facts are the documented no-op.
        assert!(unsafe { hud_with_facts(Default::default(), std::ptr::null()) }.is_ok());
    }

    #[cfg(feature = "quic")]
    #[test]
    fn hud_calls_report_a_null_connection() {
        let null = std::ptr::null();
        let mut buf = [0 as std::os::raw::c_char; 8];
        // SAFETY: null handles are the documented reported-not-UB case.
        let statuses = unsafe {
            [
                punktfunk_connection_hud_decoded(null, 1, 1, 2),
                punktfunk_connection_hud_displayed(null, 1, 2, 3),
                punktfunk_connection_hud_os_floor(null, 1),
                punktfunk_connection_hud_drain(null),
                punktfunk_connection_hud_text(
                    null,
                    3,
                    true,
                    std::ptr::null(),
                    buf.as_mut_ptr(),
                    buf.len(),
                    std::ptr::null_mut(),
                ),
            ]
        };
        assert!(statuses.iter().all(|s| *s == PunktfunkStatus::NullPointer));
    }

    /// Size-prefix guard: null/undersized is a status, not a read.
    #[test]
    fn connect_opts_guards_size_prefix() {
        let mut status = 0i32;
        // SAFETY: null `opts` is the documented reported-not-UB case.
        let c =
            unsafe { punktfunk_connect_opts(std::ptr::null(), std::ptr::null_mut(), &mut status) };
        assert!(c.is_null());
        assert_eq!(status, PunktfunkStatus::NullPointer as i32);

        // SAFETY: an all-zero struct is a valid value (null pointers, zero scalars).
        let mut o: PunktfunkConnectOpts = unsafe { std::mem::zeroed() };
        o.struct_size = 4; // smaller than CONNECT_OPTS_MIN_SIZE
                           // SAFETY: `o` outlives the call; out-params are null or a live local.
        let c = unsafe { punktfunk_connect_opts(&o, std::ptr::null_mut(), &mut status) };
        assert!(c.is_null());
        assert_eq!(status, PunktfunkStatus::InvalidArg as i32);

        o.struct_size = std::mem::size_of::<PunktfunkConnectOpts>() as u32;
        // SAFETY: as above; the null `host` field is the documented InvalidArg path.
        let c = unsafe { punktfunk_connect_opts(&o, std::ptr::null_mut(), &mut status) };
        assert!(c.is_null());
        assert_eq!(status, PunktfunkStatus::InvalidArg as i32);
    }

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

    #[test]
    fn identity_rejects_undersized_buffers_without_writing() {
        let mut cert: [std::os::raw::c_char; 2] = [0x5a; 2];
        let mut key: [std::os::raw::c_char; 2] = [0x5a; 2];

        // SAFETY: each array is writable for the one-byte capacity reported to the core.
        let status =
            unsafe { punktfunk_generate_identity(cert.as_mut_ptr(), 1, key.as_mut_ptr(), 1) };

        assert_eq!(status, PunktfunkStatus::InvalidArg);
        assert_eq!(cert, [0x5a; 2]);
        assert_eq!(key, [0x5a; 2]);
    }

    #[test]
    fn guard_maps_panics_to_panic_status() {
        assert_eq!(
            guard(|| panic!("test panic must stay inside the ABI guard")),
            PunktfunkStatus::Panic
        );
    }

    /// Truncation lands on a character boundary; `s[..HELLO_NAME_MAX]` would panic mid-scalar.
    #[test]
    fn device_name_truncates_on_a_character_boundary() {
        let max = crate::quic::HELLO_NAME_MAX;
        assert_eq!(clamp_device_name("Enrico's iPad"), "Enrico's iPad");

        // Straddling: 2-byte characters over an odd-length prefix, so the cap lands mid-scalar.
        let straddle = format!("{}{}", "x".repeat(max - 1), "ü".repeat(4));
        let cut = clamp_device_name(&straddle);
        assert!(cut.len() <= max, "{} bytes exceeds the cap", cut.len());
        assert_eq!(
            cut,
            "x".repeat(max - 1),
            "must drop the whole ü, not half of it"
        );

        // A name whose first character already exceeds the cap has nothing to keep —
        // `unwrap_or(0)` must yield "" rather than panicking on an empty iterator.
        assert_eq!(clamp_device_name(&"あ".repeat(max)), "あ".repeat(max / 3));
    }

    /// Invalid `kind` is a status, not UB. Staged in `MaybeUninit` so no `&InputEvent` to 42.
    #[test]
    fn read_input_event_rejects_null_and_bad_discriminant() {
        // SAFETY: null is the documented reported-not-UB case.
        let null_result = unsafe { read_input_event(std::ptr::null()) };
        assert_eq!(null_result.unwrap_err(), PunktfunkStatus::NullPointer);

        let mut slot = core::mem::MaybeUninit::<InputEvent>::zeroed();
        let p = slot.as_mut_ptr();
        // SAFETY: writing one byte at offset 0 of aligned, sized storage.
        unsafe { p.cast::<u8>().write(42) };
        // SAFETY: `p` is aligned and readable for the full struct.
        let bad_tag = unsafe { read_input_event(p) };
        assert_eq!(bad_tag.unwrap_err(), PunktfunkStatus::InvalidArg);

        // SAFETY: as above; tag 0 (KeyDown) + zeroed fields is a fully valid event.
        unsafe { p.cast::<u8>().write(0) };
        // SAFETY: as above.
        let ev = unsafe { read_input_event(p) }.expect("valid tag must pass");
        assert_eq!(ev.kind, crate::input::InputKind::KeyDown);
    }

    /// AudioCtl packs as kind 5, `which` = flags, `effect[0..6]`, `effect_len = 6`.
    #[test]
    fn hidout_abi_maps_audio_ctl() {
        let out = PunktfunkHidOutput::from_hid(&crate::quic::HidOutput::AudioCtl {
            pad: 3,
            flags: 0x17,
            raw: [0x50, 0x60, 0x70, 0x05, 0, 0],
        });
        assert_eq!(out.kind, PUNKTFUNK_HIDOUT_AUDIO_CTL);
        assert_eq!(out.pad, 3);
        assert_eq!(out.which, 0x17);
        assert_eq!(out.effect_len, 6);
        assert_eq!(out.effect[..6], [0x50, 0x60, 0x70, 0x05, 0, 0]);
        assert_eq!(out.effect[6..], [0; 5]);
        assert_eq!(out.raw_len, 0);
    }

    /// HidRaw maps to kind 6 + `hid_kind`/`raw`/`raw_len`, not a skip.
    #[test]
    fn hidout_abi_maps_hid_raw() {
        // OUTPUT report (id 0x80), host-trimmed to its declared 10 bytes.
        let rumble: Vec<u8> = vec![0x80, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let out = PunktfunkHidOutput::from_hid(&crate::quic::HidOutput::HidRaw {
            pad: 2,
            kind: crate::quic::HID_RAW_OUTPUT,
            data: rumble.clone(),
        });
        assert_eq!(out.kind, PUNKTFUNK_HIDOUT_HID_RAW);
        assert_eq!(out.pad, 2);
        assert_eq!(out.hid_kind, crate::quic::HID_RAW_OUTPUT);
        assert_eq!(out.raw_len, 10);
        assert_eq!(out.raw[..10], rumble[..]);
        assert_eq!(out.raw[10..], [0; crate::quic::HID_REPORT_MAX - 10]);
        // The other fields stay zero — `kind` alone says which ones are meaningful.
        assert_eq!(out.effect_len, 0);

        // A FEATURE frame arrives whole (zero-padded) and must round-trip whole;
        // anything longer clamps instead of overrunning.
        let mut lizard = vec![0u8; crate::quic::HID_REPORT_MAX + 8];
        lizard[..6].copy_from_slice(&[0x01, 0x87, 0x03, 0x09, 0x00, 0x00]);
        let out = PunktfunkHidOutput::from_hid(&crate::quic::HidOutput::HidRaw {
            pad: 0,
            kind: crate::quic::HID_RAW_FEATURE,
            data: lizard.clone(),
        });
        assert_eq!(out.hid_kind, crate::quic::HID_RAW_FEATURE);
        assert_eq!(out.raw_len as usize, crate::quic::HID_REPORT_MAX);
        assert_eq!(out.raw[..], lizard[..crate::quic::HID_REPORT_MAX]);
    }

    /// `punktfunk_connection_send_hid_report`'s clamp: `pad` masked to 16 and the
    /// report bounded to `HID_REPORT_MAX` — same rules as the Android JNI shim.
    #[test]
    fn send_hid_report_clamps_like_the_android_shim() {
        // A 46-byte state report (id 0x45) passes through unclamped.
        let mut state = vec![0u8; 46];
        state[0] = 0x45;
        state[1] = 0xE5; // seq
        match hid_report_rich_input(3, &state) {
            crate::quic::RichInput::HidReport { pad, len, data } => {
                assert_eq!(pad, 3);
                assert_eq!(len, 46);
                assert_eq!(data[..46], state[..]);
                assert_eq!(data[46..], [0; crate::quic::HID_REPORT_MAX - 46]);
            }
            other => panic!("expected HidReport, got {other:?}"),
        }
        // Oversize input truncates to the wire body; a pad above the wire space wraps into it.
        let big = vec![0xAB; 100];
        match hid_report_rich_input(0x17, &big) {
            crate::quic::RichInput::HidReport { pad, len, data } => {
                assert_eq!(pad, 0x7);
                assert_eq!(len as usize, crate::quic::HID_REPORT_MAX);
                assert_eq!(data, [0xAB; crate::quic::HID_REPORT_MAX]);
            }
            other => panic!("expected HidReport, got {other:?}"),
        }
    }

    /// Opus on `0xC9`, 48 kHz, 16-bit, stereo — what an embedder that does not call
    /// `punktfunk_connect_ex11` still gets.
    const OPUS_48K: AudioFormat = AudioFormat {
        codec: crate::quic::AUDIO_CODEC_OPUS,
        rate_hz: crate::audio::SAMPLE_RATE_HZ,
        bits: crate::audio::pcm::BITS_16,
        channels: 2,
        frame_us: crate::audio::FRAME_MS * 1000,
        layout: 0,
    };

    /// Lossless session at 48 kHz / 24-bit.
    const PCM_48K_24: AudioFormat = AudioFormat {
        codec: crate::quic::AUDIO_CODEC_PCM,
        rate_hz: crate::audio::SAMPLE_RATE_HZ,
        bits: crate::audio::pcm::BITS_24,
        channels: 2,
        frame_us: crate::audio::pcm::FRAME_US_LADDER[0],
        layout: 0,
    };

    /// Concealment run a 5 ms session owes: ten frames (50 ms cap).
    const CONCEAL_RUN: u32 = crate::audio::max_conceal_packets(crate::audio::FRAME_MS * 1000);

    /// One `0xD3` payload of `n` interleaved stereo samples at `bits`, from a
    /// deterministic ramp so any stride or sign-extension error is visible.
    fn pcm_frame(n: usize, bits: u8) -> (Vec<f32>, Vec<u8>) {
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            samples.push((i as f32 / n as f32) * 1.8 - 0.9);
        }
        let mut wire = Vec::new();
        crate::audio::pcm::from_f32(&samples, bits, &mut wire);
        // Quantised once, so the expectation is what the wire carries rather than the
        // pre-quantisation floats.
        let mut expect = Vec::new();
        crate::audio::pcm::to_f32(&wire, bits, &mut expect).expect("whole samples");
        (expect, wire)
    }

    /// PCM decode is bit-exact at the ABI boundary.
    #[test]
    fn the_pcm_plane_decodes_bit_exactly() {
        for bits in [crate::audio::pcm::BITS_16, crate::audio::pcm::BITS_24] {
            let fmt = AudioFormat { bits, ..PCM_48K_24 };
            // 5 ms at 48 kHz stereo — the longest rung of the ladder.
            let (expect, wire) = pcm_frame(240 * 2, bits);
            let mut state = AudioPcmState::default();
            let n = state.decode_packet(&wire, 0, fmt).expect("decodes");
            assert_eq!(n, expect.len(), "{bits}-bit: sample count");
            assert_eq!(
                &state.pcm[..n],
                &expect[..],
                "{bits}-bit PCM must reach the embedder unchanged"
            );

            // Stays exact packet after packet, at the buffer offsets a real session uses.
            let (expect2, wire2) = pcm_frame(240 * 2, bits);
            let n2 = state.decode_packet(&wire2, 1, fmt).expect("decodes");
            assert_eq!(&state.pcm[..n2], &expect2[..]);
        }
    }

    /// PCM gaps use `PcmConceal`, never libopus. No decoder is built; the conceal
    /// frame repeats the last real one (`design/hi-res-audio.md`).
    #[test]
    fn a_missing_pcm_frame_is_concealed_without_libopus() {
        let bits = crate::audio::pcm::BITS_24;
        let (expect, wire) = pcm_frame(240 * 2, bits);
        let mut state = AudioPcmState::default();
        assert_eq!(state.decode_packet(&wire, 0, PCM_48K_24), Ok(expect.len()));

        // Seq 2 is lost: one concealed frame lands in front of the real one,
        // same shape as Opus so nothing downstream branches on the plane.
        let (expect3, wire3) = pcm_frame(240 * 2, bits);
        let n = state.decode_packet(&wire3, 2, PCM_48K_24).expect("decodes");
        assert_eq!(
            n,
            2 * expect3.len(),
            "one concealed frame, then the real one"
        );
        assert!(
            state.decoder.is_none(),
            "a libopus decoder must never be built on the lossless plane"
        );
        // Concealed frame is the previous one faded: head matches last good sample
        // (raised cosine ~1.0), tail is silence. PLC-synthesized would match neither.
        assert!(
            (state.pcm[0] - expect[0]).abs() < 1e-3,
            "concealment must repeat the last good frame, got {} vs {}",
            state.pcm[0],
            expect[0]
        );
        let tail = state.pcm[expect.len() - 1];
        assert!(
            tail.abs() < 0.01,
            "the fade must land on silence, got {tail}"
        );
        // The real frame follows it, still bit-exact.
        assert_eq!(&state.pcm[expect3.len()..n], &expect3[..]);

        // Drought path takes the same route: PcmConceal, one frame, credited against
        // the next arrival's loss so the gap is never concealed twice.
        let before = state.drought_frames;
        assert_eq!(state.conceal(PCM_48K_24), Ok(expect.len()));
        assert_eq!(state.drought_frames, before + 1);
        assert!(state.decoder.is_none());
    }

    /// `pcm` must never reallocate: the embedder holds a pointer into it.
    #[test]
    fn the_pcm_buffer_is_never_reallocated() {
        for fmt in [OPUS_48K, PCM_48K_24] {
            let mut state = AudioPcmState::default();
            let (_, wire) = pcm_frame(240 * 2, fmt.bits);
            // Prime it. On the Opus arm the payload is undecodable: the buffer is
            // sized before the packet is looked at, which is the property under test.
            let _ = state.decode_packet(&wire, 0, fmt);
            let ptr = state.pcm.as_ptr();
            let len = state.pcm.len();
            assert!(len > 0, "sizing must have happened on the first packet");

            // A datagram far longer than any ladder rung — the case that would otherwise
            // grow the buffer.
            let (_, huge) = pcm_frame(200_000, fmt.bits);
            let _ = state.decode_packet(&huge, 1, fmt);
            // A long concealment run, at the far end of the buffer.
            for seq in 2..40 {
                let _ = state.decode_packet(&wire, seq * 7, fmt);
                let _ = state.conceal(fmt);
            }
            assert_eq!(state.pcm.as_ptr(), ptr, "the buffer moved");
            assert_eq!(state.pcm.len(), len, "the buffer was resized");
        }
    }

    /// A truncated `0xD3` payload (not a whole number of samples) must be refused
    /// rather than decoded as a shifted frame. Concealment already earned still goes out.
    #[test]
    fn a_torn_pcm_datagram_is_refused_not_shifted() {
        let mut state = AudioPcmState::default();
        let (_, wire) = pcm_frame(240 * 2, crate::audio::pcm::BITS_24);
        assert_eq!(
            state.decode_packet(&wire[..wire.len() - 1], 0, PCM_48K_24),
            Err(PunktfunkStatus::BadPacket)
        );
        // With a gap owed, the concealment survives the bad packet instead of dying with it.
        assert_eq!(state.decode_packet(&wire, 1, PCM_48K_24), Ok(240 * 2));
        assert_eq!(
            state.decode_packet(&wire[..wire.len() - 1], 3, PCM_48K_24),
            Ok(240 * 2),
            "the gap before a torn packet is still concealed"
        );
        // Empty payload must not wipe the concealment source: PCM has no DTX, so this
        // is a torn datagram; clearing `prev` would leave the next loss with nothing to repeat.
        assert_eq!(state.decode_packet(&[], 4, PCM_48K_24), Ok(0));
        assert_eq!(
            state.decode_packet(&wire, 6, PCM_48K_24),
            Ok(2 * 240 * 2),
            "the frame before the empty one must still be there to conceal from"
        );
    }

    /// An Opus session stays on libopus at 48 kHz with the same buffer geometry;
    /// `PcmConceal` is never involved.
    #[test]
    fn an_opus_session_is_unaffected_by_the_lossless_plane() {
        let l = crate::audio::LAYOUT_STEREO;
        let mut enc = opus::MSEncoder::new(
            48_000,
            l.streams,
            l.coupled,
            l.mapping,
            opus::Application::LowDelay,
        )
        .expect("MSEncoder");
        enc.set_vbr(false).unwrap();
        let mut frame = vec![0f32; 240 * 2];
        for (i, s) in frame.iter_mut().enumerate() {
            *s = 0.25 * (i as f32 * 0.05).sin();
        }
        let mut out = vec![0u8; 1500];
        let n = enc.encode_float(&frame, &mut out).unwrap();
        out.truncate(n);

        let mut state = AudioPcmState::default();
        assert_eq!(state.decode_packet(&out, 0, OPUS_48K), Ok(240 * 2));
        assert!(state.decoder.is_some(), "still a libopus decoder");
        // 120 ms of Opus plus a full concealment run.
        assert_eq!(state.pcm.len(), (1 + CONCEAL_RUN as usize) * 5760 * 2);
        // Gaps and droughts still go through libopus PLC; PcmConceal is never fed.
        assert_eq!(state.decode_packet(&out, 2, OPUS_48K), Ok(2 * 240 * 2));
        assert_eq!(state.conceal(OPUS_48K), Ok(240 * 2));
        assert_eq!(state.conceal_pcm.run(), 0, "PcmConceal must be untouched");
        assert!(state.scratch_pcm.is_empty());

        // Accessors report 48 kHz / 16-bit, matching `PUNKTFUNK_AUDIO_SAMPLE_RATE_HZ`.
        assert_eq!(OPUS_48K.rate_hz, crate::audio::SAMPLE_RATE_HZ);
        assert!(!OPUS_48K.is_pcm());
    }

    /// Buffer is sized from the negotiated rate, not a hardcoded 48 kHz.
    #[test]
    fn the_buffer_follows_the_negotiated_rate() {
        let hi = AudioFormat {
            rate_hz: 96_000,
            ..PCM_48K_24
        };
        let mut state = AudioPcmState::default();
        // The longest ladder rung at 96 kHz is 5 ms = 480 samples/ch.
        let (expect, wire) = pcm_frame(480 * 2, hi.bits);
        assert_eq!(state.decode_packet(&wire, 0, hi), Ok(expect.len()));
        assert_eq!(&state.pcm[..expect.len()], &expect[..]);
        assert_eq!(
            state.pcm.len(),
            (1 + CONCEAL_RUN as usize) * 480 * 2,
            "sized from 96 kHz, not from the 48 kHz default"
        );
        // A full concealment run fits, which is the point of sizing it that way.
        let n = state
            .decode_packet(&wire, 1 + CONCEAL_RUN, hi)
            .expect("decodes");
        assert_eq!(n, (1 + CONCEAL_RUN as usize) * expect.len());
    }

    /// 50 ms cap at 2 ms/frame is 25 frames, not 10. Buffer must be sized for
    /// that run: growth would dangle the embedder pointer.
    #[test]
    fn a_short_frame_owes_more_concealment_and_the_buffer_was_sized_for_it() {
        let short = AudioFormat {
            rate_hz: 44_100,
            frame_us: 2_000,
            ..PCM_48K_24
        };
        let run = crate::audio::max_conceal_packets(short.frame_us);
        assert_eq!(run, 25, "50 ms of 2 ms frames");

        // 2 ms at 44 100 Hz stereo: 88 samples per channel, not 88.2.
        let frame = crate::audio::pcm::samples_per_frame(44_100, 2_000, 2);
        assert_eq!(frame, 176);
        let (expect, wire) = pcm_frame(frame, short.bits);

        let mut state = AudioPcmState::default();
        assert_eq!(state.decode_packet(&wire, 0, short), Ok(expect.len()));
        // Sized for the run at this frame, from the longest rung's frame size (5 ms = 220/ch).
        assert_eq!(state.pcm.len(), (1 + run as usize) * 220 * 2);
        let base = state.pcm.as_ptr();

        // A maximal gap: 25 concealed frames plus the real one, contiguous; the buffer
        // did not move under the embedder's pointer.
        let n = state.decode_packet(&wire, 1 + run, short).expect("decodes");
        assert_eq!(n, (1 + run as usize) * expect.len());
        assert!(n <= state.pcm.len(), "the run must fit what was allocated");
        assert!(
            std::ptr::eq(base, state.pcm.as_ptr()),
            "pcm was reallocated"
        );
    }

    /// In-core PCM decoder heals seq gaps with concealment: a lost packet's PLC
    /// lands in front of the arriving frame, DTX markers advance accounting without
    /// being decoded, and a gap is capped at 50 ms.
    #[test]
    fn audio_pcm_decode_conceals_seq_gaps() {
        const FRAME: usize = 240; // 5 ms @ 48 kHz, per channel
        let l = crate::audio::LAYOUT_STEREO;
        let mut enc = opus::MSEncoder::new(
            48_000,
            l.streams,
            l.coupled,
            l.mapping,
            opus::Application::LowDelay,
        )
        .expect("MSEncoder");
        enc.set_vbr(false).unwrap();
        let mut packet = |tone: f32| {
            let mut frame = vec![0f32; FRAME * 2];
            for (i, s) in frame.iter_mut().enumerate() {
                *s = 0.25 * (i as f32 * tone).sin();
            }
            let mut out = vec![0u8; 1500];
            let n = enc.encode_float(&frame, &mut out).unwrap();
            out.truncate(n);
            out
        };

        let mut state = AudioPcmState::default();
        // In-order packets decode to exactly one frame each.
        assert_eq!(
            state.decode_packet(&packet(0.05), 0, OPUS_48K),
            Ok(FRAME * 2)
        );
        assert_eq!(
            state.decode_packet(&packet(0.05), 1, OPUS_48K),
            Ok(FRAME * 2)
        );
        // Seq 2 lost: one concealed frame precedes the real one, contiguously.
        assert_eq!(
            state.decode_packet(&packet(0.06), 3, OPUS_48K),
            Ok(2 * FRAME * 2)
        );
        // A duplicate conceals nothing.
        assert_eq!(
            state.decode_packet(&packet(0.06), 3, OPUS_48K),
            Ok(FRAME * 2)
        );
        // DTX marker, nothing lost before it: nothing to emit (the ABI maps 0 to NoFrame).
        assert_eq!(state.decode_packet(&[], 4, OPUS_48K), Ok(0));
        // A DTX marker after a loss still flushes the concealment owed (seq 5 lost).
        assert_eq!(state.decode_packet(&[], 6, OPUS_48K), Ok(FRAME * 2));
        // The DTX slot itself was accounted, not treated as a loss.
        assert_eq!(
            state.decode_packet(&packet(0.07), 7, OPUS_48K),
            Ok(FRAME * 2)
        );
        // A huge gap is capped at 50 ms of concealment — ten frames at this session's 5 ms.
        assert_eq!(
            state.decode_packet(&packet(0.07), 1000, OPUS_48K),
            Ok((CONCEAL_RUN as usize + 1) * FRAME * 2)
        );
    }

    /// Drought frames must be subtracted from the next gap so a covered loss is not concealed twice.
    #[test]
    fn drought_concealment_is_not_charged_again_by_the_loss_path() {
        const FRAME: usize = 240; // 5 ms @ 48 kHz, per channel
        let l = crate::audio::LAYOUT_STEREO;
        let mut enc = opus::MSEncoder::new(
            48_000,
            l.streams,
            l.coupled,
            l.mapping,
            opus::Application::LowDelay,
        )
        .expect("MSEncoder");
        enc.set_vbr(false).unwrap();
        let mut packet = |tone: f32| {
            let mut frame = vec![0f32; FRAME * 2];
            for (i, s) in frame.iter_mut().enumerate() {
                *s = 0.25 * (i as f32 * tone).sin();
            }
            let mut out = vec![0u8; 1500];
            let n = enc.encode_float(&frame, &mut out).unwrap();
            out.truncate(n);
            out
        };

        let mut state = AudioPcmState::default();
        // Nothing has decoded: PLC has no state to extrapolate from, and the ABI reports NoFrame.
        assert_eq!(state.conceal(OPUS_48K), Ok(0));

        assert_eq!(
            state.decode_packet(&packet(0.05), 0, OPUS_48K),
            Ok(FRAME * 2)
        );
        // The wire goes quiet; the embedder covers four frames of it.
        for _ in 0..4 {
            assert_eq!(state.conceal(OPUS_48K), Ok(FRAME * 2));
        }
        // It comes back at seq 7 — six packets missing, four of them already in the ring.
        assert_eq!(
            state.decode_packet(&packet(0.06), 7, OPUS_48K),
            Ok(3 * FRAME * 2)
        );
        // The next drought starts from nothing owed, not from a stale credit.
        assert_eq!(
            state.decode_packet(&packet(0.06), 9, OPUS_48K),
            Ok(2 * FRAME * 2)
        );
    }
}
