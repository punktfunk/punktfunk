//! OS-clipboard bridge for the spawned session client: Win32 on Windows, the
//! session window's SDL3 clipboard on Linux.
//!
//! Protocol and fetch streams live in `punktfunk_core::clipboard`. This
//! thread talks to the OS clipboard. Evidence: `design/clipboard-and-file-transfer.md`.
//!
//! Local copies announce a format list (`clip_offer`); bytes cross only on
//! `FetchRequest`. Remote offers are fetched and placed as real bytes: lazy
//! placement needs a clipboard-owning window pumping on this thread (Win32), or
//! a network fetch on the presenter's thread (SDL). [`EAGER_FETCH_CAP`] skips a
//! payload too large to pull for a paste that may never happen.
//!
//! After our own write, that sequence number is ignored, or each side re-offers
//! the other's apply. A copy a password manager marks secret is never announced
//! or served. `run` is a no-op without `HOST_CAP_CLIPBOARD`.

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

/// How often the local clipboard is checked. Win32: 400 ms, ahead of copy→paste (the spec
/// floor between offers is 100 ms). Linux: every pass, an atomic load. Wayland shows a copy
/// only when the window gains focus, and the offer must beat the first paste.
const POLL: Duration = if cfg!(target_os = "linux") {
    Duration::ZERO
} else {
    Duration::from_millis(400)
};

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
    // SDL reports clipboard changes only as events; the watch counts them.
    #[cfg(target_os = "linux")]
    let _watch = os::Watch::install();

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
        // Check the OS clipboard on [`POLL`]. This wait is short so teardown is not delayed.
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
    /// A change within [`OFFER_SPACING`] of this waits for a later pass.
    last_offer: Option<Instant>,
    remote_offer: Option<u32>,
    /// In-flight fetch id and the mime `Data` will place.
    pending_fetch: Option<(u32, String)>,
    /// Last local apply, reused when the host fetches our echo (CF_UNICODETEXT round-trips are lossy).
    last_applied: Option<(String, Vec<u8>)>,
}

/// The spec floor between two offers.
const OFFER_SPACING: Duration = Duration::from_millis(100);

impl State {
    /// Whether `seq` is a new local copy to announce at `now`. A change inside
    /// [`OFFER_SPACING`] stays unseen, so the next pass finds it.
    fn take_local_change(&mut self, seq: u32, now: Instant) -> bool {
        if seq == self.last_seq {
            return false;
        }
        if self
            .last_offer
            .is_some_and(|t| now.saturating_duration_since(t) < OFFER_SPACING)
        {
            return false;
        }
        self.last_seq = seq;
        // Our apply — drop it or both sides re-offer forever.
        if self.self_written_seq == Some(seq) {
            self.self_written_seq = None;
            return false;
        }
        true
    }
}

/// Announce the local format list; never the bytes.
fn poll_local(client: &NativeClient, state: &mut State) {
    if !state.take_local_change(os::sequence_number(), Instant::now()) {
        return;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_inside_the_offer_spacing_is_taken_by_a_later_pass() {
        let t0 = Instant::now();
        let mut state = State {
            last_seq: 1,
            last_offer: Some(t0),
            ..Default::default()
        };
        assert!(!state.take_local_change(2, t0 + Duration::from_millis(30)));
        assert_eq!(state.last_seq, 1, "a skipped change is not consumed");
        assert!(state.take_local_change(2, t0 + OFFER_SPACING));
        assert!(!state.take_local_change(2, t0 + 2 * OFFER_SPACING));
    }

    #[test]
    fn our_own_write_is_swallowed_once() {
        let t0 = Instant::now();
        let mut state = State {
            last_seq: 1,
            self_written_seq: Some(2),
            ..Default::default()
        };
        assert!(!state.take_local_change(2, t0));
        assert_eq!(state.self_written_seq, None);
        assert!(state.take_local_change(3, t0));
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
                if g.is_null() {
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
                if g.is_null() {
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
        if g.is_null() {
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
        if unsafe { SetClipboardData(fmt, Some(g)) }.is_null() {
            // SAFETY: the clipboard refused `g`, so it is still ours, and unlocked above.
            let _ = unsafe { GlobalFree(g) };
            bail!("clipboard set failed");
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod os {
    //! The session window's SDL3 clipboard: `wl_data_device` on Wayland, selections on X11.
    //!
    //! SDL's clipboard belongs to the main thread, where the presenter pumps events. Reads and
    //! writes run there through `SDL_RunOnMainThread`; this thread waits on a channel, never on
    //! SDL, because `end_session` joins it. Changes arrive only as events: an event watch
    //! counts them and keeps the mime list. SDL drops the Wayland/X11 echo of its own
    //! selection and reports `SDL_SetClipboardData` synchronously, inside `set`, so the
    //! counter already includes our write when `set` returns.

    use super::{MIME_PNG, MIME_TEXT};
    use anyhow::{anyhow, bail, Result};
    use sdl3::sys::clipboard::{
        SDL_GetClipboardData, SDL_GetClipboardText, SDL_SetClipboardData, SDL_SetClipboardText,
    };
    use sdl3::sys::error::SDL_GetError;
    use sdl3::sys::events::{
        SDL_AddEventWatch, SDL_Event, SDL_RemoveEventWatch, SDL_EVENT_CLIPBOARD_UPDATE,
    };
    use sdl3::sys::init::{SDL_RunOnMainThread, SDL_WasInit, SDL_INIT_VIDEO};
    use sdl3::sys::stdinc::{SDL_free, SDL_strlen};
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{mpsc, Mutex, PoisonError};
    use std::time::Duration;

    /// SDL's text getter reads any of these as UTF-8.
    const TEXT_MIMES: [&str; 5] = [MIME_TEXT, "text/plain", "TEXT", "UTF8_STRING", "STRING"];
    /// Read back verbatim: Wayland and X11 name these as the wire does.
    const PASSTHROUGH: [&str; 5] = ["text/html", "text/rtf", MIME_PNG, "image/jpeg", "image/gif"];
    /// KeePassXC and other KDE-aware password managers add this to a secret copy.
    const SECRET_HINT: &str = "x-kde-passwordManagerHint";
    /// 1 s. A posted call runs within one presenter pass; past this the session is ending.
    const MAIN_WAIT: Duration = Duration::from_secs(1);

    /// Clipboard changes seen by the watch, ours included.
    static SEQ: AtomicU32 = AtomicU32::new(0);
    /// Mime types of the clipboard at the last change.
    static MIMES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// The installed event watch; removed on drop.
    pub struct Watch;
    impl Watch {
        pub fn install() -> Watch {
            // SAFETY: `on_event` takes no userdata and reads only the event SDL hands it;
            // adding a watch is safe from any thread.
            unsafe { SDL_AddEventWatch(Some(on_event), std::ptr::null_mut()) };
            Watch
        }
    }
    impl Drop for Watch {
        fn drop(&mut self) {
            // SAFETY: removes the pair `install` added; safe from any thread.
            unsafe { SDL_RemoveEventWatch(Some(on_event), std::ptr::null_mut()) };
        }
    }

    /// Runs on the thread that pushes the event: the main thread for clipboard updates.
    unsafe extern "C" fn on_event(_: *mut c_void, event: *mut SDL_Event) -> bool {
        // SAFETY: SDL passes a valid event for the duration of the call; every variant
        // starts with its type.
        let ty = unsafe { (*event).r#type };
        if ty != SDL_EVENT_CLIPBOARD_UPDATE.0 {
            return true;
        }
        // SAFETY: the type says this is the clipboard variant.
        let clip = unsafe { &(*event).clipboard };
        let count = usize::try_from(clip.num_mime_types).unwrap_or(0);
        let mut mimes = Vec::with_capacity(count);
        for i in 0..count {
            if clip.mime_types.is_null() {
                break;
            }
            // SAFETY: `mime_types` holds `num_mime_types` entries, valid during this call.
            let p = unsafe { *clip.mime_types.add(i) };
            if !p.is_null() {
                // SAFETY: each non-null entry is a NUL-terminated string, valid during this call.
                mimes.push(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
            }
        }
        *MIMES.lock().unwrap_or_else(PoisonError::into_inner) = mimes;
        SEQ.fetch_add(1, Ordering::SeqCst);
        true
    }

    pub fn sequence_number() -> u32 {
        SEQ.load(Ordering::SeqCst)
    }

    pub fn is_concealed() -> bool {
        concealed(&MIMES.lock().unwrap_or_else(PoisonError::into_inner))
    }

    pub fn available_kinds() -> Vec<(&'static str, u64)> {
        let mimes = MIMES.lock().unwrap_or_else(PoisonError::into_inner);
        wire_kinds(&mimes).into_iter().map(|m| (m, 0)).collect()
    }

    fn concealed(mimes: &[String]) -> bool {
        mimes.iter().any(|m| m == SECRET_HINT)
    }

    /// The wire mimes this clipboard can serve, text first.
    fn wire_kinds(mimes: &[String]) -> Vec<&'static str> {
        let has = |want: &str| mimes.iter().any(|m| m == want);
        let text = TEXT_MIMES.into_iter().any(has).then_some(MIME_TEXT);
        text.into_iter()
            .chain(PASSTHROUGH.into_iter().filter(|m| has(m)))
            .collect()
    }

    pub fn get(mime: &str) -> Result<Vec<u8>> {
        let mime = CString::new(mime)?;
        on_main(move || read(&mime))?
    }

    pub fn set(mime: &str, bytes: &[u8]) -> Result<()> {
        let payload = match mime {
            // `SDL_SetClipboardText` takes a C string; a NUL would cut it short.
            MIME_TEXT => Payload::Text(CString::new(
                String::from_utf8_lossy(bytes).replace('\0', ""),
            )?),
            MIME_PNG => Payload::Png(bytes.to_vec()),
            other => bail!("unsupported clipboard format {other}"),
        };
        on_main(move || write(payload))?
    }

    enum Payload {
        Text(CString),
        Png(Vec<u8>),
    }

    fn read(mime: &CStr) -> Result<Vec<u8>> {
        video_up()?;
        let mut size = 0usize;
        let ptr: *mut c_void = if mime.to_bytes() == MIME_TEXT.as_bytes() {
            // SAFETY: main thread, video up; SDL returns an owned NUL-terminated string.
            let text = unsafe { SDL_GetClipboardText() };
            if !text.is_null() {
                // SAFETY: non-null and NUL-terminated, per SDL.
                size = unsafe { SDL_strlen(text) };
            }
            text.cast()
        } else {
            // SAFETY: main thread, video up; `mime` outlives the call and `size` is writable.
            unsafe { SDL_GetClipboardData(mime.as_ptr(), &mut size) }
        };
        if ptr.is_null() {
            bail!("clipboard has no {}", mime.to_string_lossy());
        }
        // SAFETY: SDL hands back `size` readable bytes that we own until `SDL_free`.
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) }.to_vec();
        // SAFETY: the buffer SDL allocated above, freed once.
        unsafe { SDL_free(ptr) };
        if bytes.is_empty() {
            bail!("clipboard {} is empty", mime.to_string_lossy());
        }
        Ok(bytes)
    }

    fn write(payload: Payload) -> Result<()> {
        video_up()?;
        let ok = match payload {
            // SAFETY: main thread, video up; SDL copies the string before returning.
            Payload::Text(text) => unsafe { SDL_SetClipboardText(text.as_ptr()) },
            Payload::Png(bytes) => {
                let mimes: [*const c_char; 1] = [c"image/png".as_ptr()];
                let data = Box::into_raw(Box::new(bytes));
                // SAFETY: main thread, video up. SDL copies the mime list and owns `data` from
                // this call on, success or not, and frees it once through `release`.
                unsafe {
                    SDL_SetClipboardData(
                        Some(serve),
                        Some(release),
                        data.cast(),
                        mimes.as_ptr(),
                        mimes.len(),
                    )
                }
            }
        };
        if !ok {
            bail!("clipboard set: {}", sdl_error());
        }
        Ok(())
    }

    /// SDL asks for the bytes when an app pastes.
    unsafe extern "C" fn serve(
        userdata: *mut c_void,
        _mime: *const c_char,
        size: *mut usize,
    ) -> *const c_void {
        // SAFETY: `userdata` is the buffer boxed in `write`, alive until `release`.
        let bytes = unsafe { &*userdata.cast::<Vec<u8>>() };
        // SAFETY: SDL passes a writable size.
        unsafe { *size = bytes.len() };
        bytes.as_ptr().cast()
    }

    /// SDL is done with the buffer: replaced, cleared or shut down.
    unsafe extern "C" fn release(userdata: *mut c_void) {
        // SAFETY: SDL calls this once per buffer `write` boxed, after its last `serve`.
        drop(unsafe { Box::from_raw(userdata.cast::<Vec<u8>>()) });
    }

    type Job = Box<dyn FnOnce() + Send>;

    /// Run `f` on SDL's main thread and wait up to [`MAIN_WAIT`] for its answer.
    fn on_main<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
        let (tx, rx) = mpsc::sync_channel(1);
        let job: Job = Box::new(move || {
            let _ = tx.send(f());
        });
        let job = Box::into_raw(Box::new(job));
        // Not `wait_complete`: if the main thread is joining us, both would wait forever.
        // SAFETY: `trampoline` takes ownership of `job` and runs it once.
        if !unsafe { SDL_RunOnMainThread(Some(trampoline), job.cast(), false) } {
            // SAFETY: SDL refused, so `trampoline` never runs and `job` is still ours.
            drop(unsafe { Box::from_raw(job) });
            bail!("main-thread call: {}", sdl_error());
        }
        rx.recv_timeout(MAIN_WAIT)
            .map_err(|_| anyhow!("main thread did not answer"))
    }

    unsafe extern "C" fn trampoline(userdata: *mut c_void) {
        // SAFETY: `userdata` is the boxed job `on_main` leaked; SDL runs this once.
        let job = unsafe { Box::from_raw(userdata.cast::<Job>()) };
        job();
    }

    fn video_up() -> Result<()> {
        // SAFETY: a flag query; reads no pointer.
        if unsafe { SDL_WasInit(SDL_INIT_VIDEO) }.0 == 0 {
            bail!("SDL video is not up");
        }
        Ok(())
    }

    fn sdl_error() -> String {
        // SAFETY: SDL returns this thread's NUL-terminated error string, never null.
        unsafe { CStr::from_ptr(SDL_GetError()) }
            .to_string_lossy()
            .into_owned()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn list(m: &[&str]) -> Vec<String> {
            m.iter().map(|s| s.to_string()).collect()
        }

        #[test]
        fn wire_kinds_maps_text_variants_and_keeps_wire_names() {
            let x11 = list(&["TARGETS", "UTF8_STRING", "STRING", "text/html"]);
            assert_eq!(wire_kinds(&x11), [MIME_TEXT, "text/html"]);
            let image = list(&["image/png", "image/bmp", "image/gif"]);
            assert_eq!(wire_kinds(&image), [MIME_PNG, "image/gif"]);
            assert!(wire_kinds(&list(&["application/x-kde-cutselection"])).is_empty());
        }

        #[test]
        fn secret_hint_conceals() {
            let secret = list(&["text/plain;charset=utf-8", SECRET_HINT]);
            assert!(concealed(&secret));
            assert!(!concealed(&list(&["text/plain;charset=utf-8"])));
        }
    }
}
