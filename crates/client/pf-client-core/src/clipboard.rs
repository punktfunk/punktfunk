//! OS-clipboard bridge for the spawned session client.
//!
//! Protocol and fetch streams live in `punktfunk_core::clipboard`. This
//! thread talks to the pasteboard. Evidence: `design/clipboard-and-file-transfer.md`.
//!
//! Local copies announce a format list (`clip_offer`); bytes cross only on
//! `FetchRequest`. Remote offers are fetched and placed as real bytes —
//! delayed rendering needs a clipboard-owning window and its own message
//! pump, which this thread is not. [`EAGER_FETCH_CAP`] skips a payload too
//! large to pull for a paste that may never happen.
//!
//! After our `SetClipboardData`, that sequence number is ignored, or each
//! side re-offers the other's apply. A clipboard marked
//! `ExcludeClipboardContentFromMonitorProcessing` is never announced or
//! served. `run` is a no-op without `HOST_CAP_CLIPBOARD`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use punktfunk_core::client::NativeClient;
use punktfunk_core::clipboard::ClipEventCore;
use punktfunk_core::quic::{ClipKind, CLIP_FILE_INDEX_NONE, HOST_CAP_CLIPBOARD};

/// UTF-8 text — every peer must handle this mime.
const MIME_TEXT: &str = "text/plain;charset=utf-8";
/// Registered "PNG" clipboard format, not `CF_DIB`.
const MIME_PNG: &str = "image/png";

/// 4 MiB. Eager remote fetches above this are skipped; the paste may never happen.
const EAGER_FETCH_CAP: u64 = 4 << 20;

/// 400 ms. Spec floor between offers is 100 ms; this stays ahead of copy→paste.
const POLL: Duration = Duration::from_millis(400);

/// 120 ms. Bounds how long `stop` waits; Win32 is polled on [`POLL`], not this.
const EVENT_WAIT: Duration = Duration::from_millis(120);

/// Safe to spawn unconditionally: no-op without `HOST_CAP_CLIPBOARD`.
pub fn run(client: Arc<NativeClient>, stop: Arc<AtomicBool>) {
    if client.host_caps() & HOST_CAP_CLIPBOARD == 0 {
        tracing::info!("host has no clipboard capability — shared clipboard off");
        return;
    }
    // Opt-in: nothing is announced or served until the host sees enabled.
    if let Err(e) = client.clip_control(true, 0) {
        tracing::warn!(error = %e, "clipboard: enable failed");
        return;
    }
    tracing::info!("shared clipboard enabled");

    let mut state = State {
        // Seed the live sequence without offering: a pre-session clipboard is not a copy for this stream.
        last_seq: os::sequence_number(),
        ..Default::default()
    };
    let mut next_poll = Instant::now() + POLL;

    while !stop.load(Ordering::SeqCst) {
        // FetchRequest first — the host is blocked on us.
        match client.next_clip(EVENT_WAIT) {
            Ok(ev) => handle_event(&client, &mut state, ev),
            Err(punktfunk_core::error::PunktfunkError::NoFrame) => {}
            Err(_) => break,
        }
        // Poll Win32 on [`POLL`], not every wait. This wait is short so teardown is not delayed.
        let now = Instant::now();
        if now >= next_poll {
            poll_local(&client, &mut state);
            next_poll = now + POLL;
        }
    }
    let _ = client.clip_control(false, 0);
}

#[derive(Default)]
struct State {
    last_seq: u32,
    /// Sequence we wrote; swallow that one change or offers echo.
    self_written_seq: Option<u32>,
    /// Newest-wins offer id.
    offer_seq: u32,
    /// Skip a new offer under 100 ms.
    last_offer: Option<Instant>,
    remote_offer: Option<u32>,
    /// In-flight fetch id and the mime `Data` will place.
    pending_fetch: Option<(u32, String)>,
    /// Last local apply, reused when the host fetches our echo (CF_UNICODETEXT round-trips are lossy).
    last_applied: Option<(String, Vec<u8>)>,
}

/// Announce the local format list; never the bytes.
fn poll_local(client: &NativeClient, state: &mut State) {
    let seq = os::sequence_number();
    if seq == state.last_seq {
        return;
    }
    state.last_seq = seq;
    // Our apply — drop it or both sides re-offer forever.
    if state.self_written_seq == Some(seq) {
        state.self_written_seq = None;
        return;
    }
    if let Some(t) = state.last_offer {
        if t.elapsed() < Duration::from_millis(100) {
            return;
        }
    }
    if os::is_concealed() {
        tracing::debug!("clipboard: concealed content — not announced");
        return;
    }
    let mut kinds: Vec<ClipKind> = Vec::new();
    for (mime, size) in os::available_kinds() {
        kinds.push(ClipKind {
            mime: mime.to_string(),
            size_hint: size,
        });
    }
    if kinds.is_empty() {
        return;
    }
    state.offer_seq = state.offer_seq.wrapping_add(1);
    state.last_offer = Some(Instant::now());
    let seq_id = state.offer_seq;
    tracing::debug!(
        seq = seq_id,
        kinds = kinds.len(),
        "clipboard: offering local copy"
    );
    if let Err(e) = client.clip_offer(seq_id, kinds) {
        tracing::warn!(error = %e, "clipboard: offer failed");
    }
}

fn handle_event(client: &NativeClient, state: &mut State, ev: ClipEventCore) {
    match ev {
        ClipEventCore::State {
            enabled,
            policy,
            reason,
        } => {
            tracing::info!(enabled, policy, reason, "clipboard: host state");
        }
        ClipEventCore::RemoteOffer { seq, kinds } => {
            state.remote_offer = Some(seq);
            let pick = kinds
                .iter()
                .find(|k| k.mime == MIME_TEXT)
                .or_else(|| kinds.iter().find(|k| k.mime == MIME_PNG));
            let Some(kind) = pick else {
                tracing::debug!("clipboard: remote offer has no format we can place");
                return;
            };
            if kind.size_hint > EAGER_FETCH_CAP {
                tracing::info!(
                    mime = %kind.mime,
                    size = kind.size_hint,
                    "clipboard: remote payload over the eager-fetch cap — not mirrored"
                );
                return;
            }
            match client.clip_fetch(seq, kind.mime.clone(), CLIP_FILE_INDEX_NONE) {
                Ok(xfer) => state.pending_fetch = Some((xfer, kind.mime.clone())),
                Err(e) => tracing::warn!(error = %e, "clipboard: fetch failed"),
            }
        }
        ClipEventCore::Data {
            xfer_id,
            bytes,
            last,
        } => {
            let Some((pending, mime)) = state.pending_fetch.clone() else {
                return;
            };
            if pending != xfer_id {
                return;
            }
            if last {
                state.pending_fetch = None;
            }
            match os::set(&mime, &bytes) {
                Ok(()) => {
                    // This sequence number is ours; ignore exactly it.
                    state.self_written_seq = Some(os::sequence_number());
                    state.last_applied = Some((mime.clone(), bytes));
                    tracing::debug!(mime = %mime, "clipboard: applied remote content");
                }
                Err(e) => tracing::warn!(error = %e, mime = %mime, "clipboard: apply failed"),
            }
        }
        // A failed read still cancels, or the host waits forever.
        ClipEventCore::FetchRequest {
            req_id,
            seq: _,
            file_index: _,
            mime,
        } => {
            if os::is_concealed() {
                let _ = client.clip_cancel(req_id);
                return;
            }
            // Prefer the payload we just applied: a CF_UNICODETEXT round-trip is lossy.
            let bytes = match &state.last_applied {
                Some((m, b)) if *m == mime && state.self_written_seq.is_some() => Ok(b.clone()),
                _ => os::get(&mime),
            };
            match bytes {
                Ok(b) => {
                    tracing::debug!(mime = %mime, len = b.len(), "clipboard: serving to host");
                    if let Err(e) = client.clip_serve(req_id, b, true) {
                        tracing::warn!(error = %e, "clipboard: serve failed");
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, mime = %mime, "clipboard: nothing to serve");
                    let _ = client.clip_cancel(req_id);
                }
            }
        }
        ClipEventCore::Cancelled { id } => tracing::debug!(id, "clipboard: transfer cancelled"),
        ClipEventCore::Error { id, code } => {
            tracing::debug!(id, code, "clipboard: transfer error");
            if state.pending_fetch.as_ref().is_some_and(|(x, _)| *x == id) {
                state.pending_fetch = None;
            }
        }
    }
}

/// Best-effort local text set ("Copy link"). Not the session bridge; never fails the caller.
pub fn set_text(text: &str) {
    if let Err(e) = os::set(MIME_TEXT, text.as_bytes()) {
        tracing::warn!(error = %format!("{e:#}"), "copying to the clipboard");
    }
}

#[cfg(windows)]
mod os {
    //! Win32 clipboard. Open, one operation, close — holding across a network fetch blocks every other app.

    use super::{MIME_PNG, MIME_TEXT};
    use anyhow::{anyhow, bail, Result};
    use windows::core::PCWSTR;
    use windows::Win32::minwindef::HGLOBAL;
    use windows::Win32::winbase::{
        GlobalAlloc, GlobalFree, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows::Win32::winuser::{
        CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
        IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };

    const CF_UNICODETEXT: u32 = 13;

    fn registered(name: &str) -> u32 {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a local NUL-terminated UTF-16 buffer that outlives this synchronous call.
        unsafe { RegisterClipboardFormatW(PCWSTR(wide.as_ptr())) }
    }

    fn png_format() -> u32 {
        registered("PNG")
    }

    /// Opened clipboard; `CloseClipboard` runs on every drop, including error paths.
    struct Clip;
    impl Clip {
        fn open() -> Result<Clip> {
            // Another process may hold the clipboard; fail after ~100 ms.
            for _ in 0..10 {
                // SAFETY: takes no pointer; `None` is the documented "associate with no window" argument.
                if unsafe { OpenClipboard(None) }.as_bool() {
                    return Ok(Clip);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            bail!("clipboard busy")
        }
    }
    impl Drop for Clip {
        fn drop(&mut self) {
            // SAFETY: pairs with the `OpenClipboard` that built this guard; closing is what `Drop` is for.
            let _ = unsafe { CloseClipboard() };
        }
    }

    /// A global memory block held by `GlobalLock`; `GlobalUnlock` runs on drop.
    struct GlobalLockGuard {
        mem: HGLOBAL,
        ptr: *mut u8,
    }
    impl GlobalLockGuard {
        /// `None` when `GlobalLock` refuses the block.
        ///
        /// # Safety
        ///
        /// `mem` is a live global memory handle, neither freed nor handed to the
        /// clipboard while the guard lives.
        unsafe fn lock(mem: HGLOBAL) -> Option<Self> {
            // SAFETY: fn contract; the pointer stays valid until the matching unlock in drop.
            let ptr = unsafe { GlobalLock(mem) }.cast::<u8>();
            (!ptr.is_null()).then_some(Self { mem, ptr })
        }

        /// The whole block: `GlobalSize` bytes.
        fn as_slice(&self) -> &[u8] {
            // SAFETY: a size query on the handle `lock`'s contract keeps live.
            let len = unsafe { GlobalSize(self.mem) };
            // SAFETY: `ptr` is the locked block, valid for its `GlobalSize` bytes until drop.
            unsafe { std::slice::from_raw_parts(self.ptr, len) }
        }

        /// Copy `bytes` to the start of the block.
        fn write(&mut self, bytes: &[u8]) {
            // SAFETY: as in `as_slice`.
            let len = unsafe { GlobalSize(self.mem) };
            assert!(
                bytes.len() <= len,
                "clipboard block smaller than its payload"
            );
            // SAFETY: `ptr` is the locked block of `len >= bytes.len()` bytes, a different
            // allocation from `bytes`.
            unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr, bytes.len()) };
        }
    }
    impl Drop for GlobalLockGuard {
        fn drop(&mut self) {
            // SAFETY: releases the lock `lock` took, exactly once.
            let _ = unsafe { GlobalUnlock(self.mem) };
        }
    }

    pub fn sequence_number() -> u32 {
        // SAFETY: a no-argument query that only reads the OS clipboard's sequence counter.
        unsafe { GetClipboardSequenceNumber() }
    }

    /// Password managers mark secrets with this format; skip those clipboards.
    pub fn is_concealed() -> bool {
        let fmt = registered("ExcludeClipboardContentFromMonitorProcessing");
        // SAFETY: a scalar format id in, a status out; reads nothing through a pointer.
        unsafe { IsClipboardFormatAvailable(fmt) }.as_bool()
    }

    pub fn available_kinds() -> Vec<(&'static str, u64)> {
        let mut out = Vec::new();
        // SAFETY: as above — a scalar format id, status out.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.as_bool() {
            out.push((MIME_TEXT, 0));
        }
        // SAFETY: as above — a scalar format id, status out.
        if unsafe { IsClipboardFormatAvailable(png_format()) }.as_bool() {
            out.push((MIME_PNG, 0));
        }
        out
    }

    pub fn get(mime: &str) -> Result<Vec<u8>> {
        let _clip = Clip::open()?;
        match mime {
            MIME_TEXT => {
                // SAFETY: the clipboard is open (the `Clip` guard); the handle returned is BORROWED from the clipboard and stays valid while it is open, so it is never freed here.
                let g: HGLOBAL = unsafe { GetClipboardData(CF_UNICODETEXT) };
                if g.0.is_null() {
                    bail!("clipboard text unavailable");
                }
                // SAFETY: `g` is that borrowed handle; `_clip` keeps it live past the guard.
                let Some(locked) = (unsafe { GlobalLockGuard::lock(g) }) else {
                    bail!("clipboard text lock failed");
                };
                // A NUL-terminated UTF-16 buffer; `GlobalSize` counts bytes.
                let wide: Vec<u16> = locked
                    .as_slice()
                    .chunks_exact(2)
                    .map(|c| u16::from_ne_bytes([c[0], c[1]]))
                    .take_while(|&c| c != 0)
                    .collect();
                Ok(String::from_utf16_lossy(&wide).into_bytes())
            }
            MIME_PNG => {
                // SAFETY: as the text path — the handle is borrowed from the open clipboard, never freed here.
                let g: HGLOBAL = unsafe { GetClipboardData(png_format()) };
                if g.0.is_null() {
                    bail!("clipboard png unavailable");
                }
                // SAFETY: `g` is that borrowed handle; `_clip` keeps it live past the guard.
                let Some(locked) = (unsafe { GlobalLockGuard::lock(g) }) else {
                    bail!("clipboard png lock failed");
                };
                Ok(locked.as_slice().to_vec())
            }
            other => Err(anyhow!("unsupported clipboard format {other}")),
        }
    }

    pub fn set(mime: &str, bytes: &[u8]) -> Result<()> {
        let (fmt, payload) = match mime {
            MIME_TEXT => {
                let text = String::from_utf8_lossy(bytes);
                let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let raw: Vec<u8> = wide.iter().flat_map(|c| c.to_le_bytes()).collect();
                (CF_UNICODETEXT, raw)
            }
            MIME_PNG => (png_format(), bytes.to_vec()),
            other => bail!("unsupported clipboard format {other}"),
        };
        let _clip = Clip::open()?;
        // SAFETY: no arguments; the clipboard is open and owned by this thread via the `Clip` guard.
        if !unsafe { EmptyClipboard() }.as_bool() {
            bail!("clipboard clear failed");
        }
        // The clipboard OWNS this block once SetClipboardData succeeds; until then it is ours.
        // SAFETY: a size in, an owned moveable handle out.
        let g = unsafe { GlobalAlloc(GMEM_MOVEABLE as u32, payload.len()) };
        if g.0.is_null() {
            bail!("clipboard alloc failed");
        }
        // SAFETY: `g` is the block just allocated; the guard drops before `g` is handed on.
        let Some(mut locked) = (unsafe { GlobalLockGuard::lock(g) }) else {
            // SAFETY: `g` is still ours: allocated above and never handed to the clipboard.
            let _ = unsafe { GlobalFree(g) };
            bail!("clipboard alloc lock failed");
        };
        locked.write(&payload);
        // Unlocked before the clipboard takes the handle.
        drop(locked);
        // SAFETY: on success ownership of `g` transfers to the clipboard, which frees it.
        if unsafe { SetClipboardData(fmt, Some(g)) }.0.is_null() {
            // SAFETY: the clipboard refused `g`, so it is still ours, and unlocked above.
            let _ = unsafe { GlobalFree(g) };
            bail!("clipboard set failed");
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod os {
    //! Stub. Sequence stays 0; get/set fail. The session loop still runs.
    use anyhow::{bail, Result};

    pub fn sequence_number() -> u32 {
        0
    }
    pub fn is_concealed() -> bool {
        false
    }
    pub fn available_kinds() -> Vec<(&'static str, u64)> {
        Vec::new()
    }
    pub fn get(_mime: &str) -> Result<Vec<u8>> {
        bail!("clipboard unsupported on this platform")
    }
    pub fn set(_mime: &str, _bytes: &[u8]) -> Result<()> {
        bail!("clipboard unsupported on this platform")
    }
}
