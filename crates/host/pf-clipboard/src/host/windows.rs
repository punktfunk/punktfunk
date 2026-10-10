//! Win32 shared-clipboard backend (`design/clipboard-and-file-transfer.md` §4).
//!
//! The clipboard is thread-affine, so one message-loop thread owns a hidden message-only
//! window. `AddClipboardFormatListener` announces host copies; `Cmd::SetOffer` installs
//! client formats via delayed rendering (`SetClipboardData(fmt, NULL)`); `WM_RENDERFORMAT`
//! blocks this thread (bounded) while the coordinator fetches paste bytes.
//!
//! The async side pokes the pump with `PostMessage(WM_APP_CMD)`. Per-window state hangs
//! off `GWLP_USERDATA`, so concurrent sessions each get their own window.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;

use ::windows::core::{w, PCWSTR};
use ::windows::Win32::Foundation::{
    GetLastError, GlobalFree, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM,
};
use ::windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use ::windows::Win32::System::LibraryLoader::GetModuleHandleW;
use ::windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
};
use ::windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use ::windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, PostMessageW, PostQuitMessage, RegisterClassW, SetWindowLongPtrW,
    TranslateMessage, GWLP_USERDATA, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLIPBOARDUPDATE, WM_DESTROY, WM_RENDERFORMAT, WNDCLASSW,
};

use super::winfmt;
use super::{
    ClipEvent, PasteResponder, WIRE_GIF, WIRE_HTML, WIRE_JPEG, WIRE_PNG, WIRE_RTF, WIRE_TEXT,
};

/// `WM_APP + 1` so a `PostMessage` poke does not collide with `WM_APP` itself.
const WM_APP_CMD: u32 = WM_APP + 1;
/// 10 s cap on a `WM_RENDERFORMAT` wait. The pasting app is frozen until we answer.
const RENDER_TIMEOUT: Duration = Duration::from_secs(10);
/// Clipboard managers hold the clipboard briefly; 20 tries × 5 ms before giving up.
const OPEN_RETRIES: u32 = 20;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(5);

/// `RegisterClassW` error 1410: class already exists. Expected on a 2nd session; we never unregister.
const ERROR_CLASS_ALREADY_EXISTS: u32 = 1410;

type ClipTx = tokio::sync::mpsc::UnboundedSender<ClipEvent>;

enum Cmd {
    /// Empty vec clears. Otherwise delayed-render the listed wire MIMEs.
    SetOffer(Vec<String>),
    Clear,
    Read {
        wire: String,
        resp: tokio::sync::oneshot::Sender<anyhow::Result<Vec<u8>>>,
    },
    Shutdown,
}

/// Message-thread state at `GWLP_USERDATA`. `RefCell`s are this thread only; `current_wire` is the
/// shared `Arc<Mutex>` the async handle also reads.
struct WinClip {
    clip_tx: ClipTx,
    current_wire: Arc<Mutex<Vec<String>>>,
    cmd_rx: RefCell<tokio::sync::mpsc::UnboundedReceiver<Cmd>>,
    offered: RefCell<Vec<u32>>,
    fmt_html: u32,
    fmt_rtf: u32,
    fmt_png: u32,
    /// `"JFIF"` atom — the conventional raw-JPEG clipboard format.
    fmt_jfif: u32,
    /// `"GIF"` atom — raw bytes so animation survives.
    fmt_gif: u32,
    /// Delayed-render wires. Synthesized `CF_DIB` picks the richest image among them.
    offered_wires: RefCell<Vec<String>>,
    own_hwnd: HWND,
}

impl WinClip {
    fn on_clipboard_update(&self, hwnd: HWND) {
        // SAFETY: GetClipboardOwner has no preconditions and needs no open clipboard.
        let owner = unsafe { GetClipboardOwner() }.unwrap_or_default();
        if owner.0 == hwnd.0 {
            return;
        }
        let mimes = self.available_wire_mimes();
        *self.current_wire.lock().unwrap() = mimes.clone();
        let _ = self.clip_tx.send(ClipEvent::Selection { mimes });
    }

    fn available_wire_mimes(&self) -> Vec<String> {
        // SAFETY: IsClipboardFormatAvailable has no preconditions and needs no open clipboard.
        let avail = |fmt: u32| unsafe { IsClipboardFormatAvailable(fmt) }.is_ok();
        let mut out = Vec::new();
        if avail(CF_UNICODETEXT.0 as u32) {
            out.push(WIRE_TEXT.to_string());
        }
        if avail(self.fmt_html) {
            out.push(WIRE_HTML.to_string());
        }
        if avail(self.fmt_rtf) {
            out.push(WIRE_RTF.to_string());
        }
        // Most apps offer only CF_DIB (Windows synthesizes CF_BITMAP/CF_DIBV5). Advertise PNG
        // either way; `read` converts DIB → PNG when the PNG atom is missing.
        if avail(self.fmt_png) || avail(CF_DIB.0 as u32) {
            out.push(WIRE_PNG.to_string());
        }
        // JPEG/GIF stay beside PNG so a lossy copy is not inflated and a GIF keeps animation.
        if avail(self.fmt_jfif) {
            out.push(WIRE_JPEG.to_string());
        }
        if avail(self.fmt_gif) {
            out.push(WIRE_GIF.to_string());
        }
        out
    }

    /// Drain `cmd_rx` into a `Vec` first so the `RefCell` borrow is gone before any command runs.
    fn drain_commands(&self, hwnd: HWND) {
        let mut cmds = Vec::new();
        {
            let mut rx = self.cmd_rx.borrow_mut();
            while let Ok(c) = rx.try_recv() {
                cmds.push(c);
            }
        }
        for c in cmds {
            match c {
                Cmd::SetOffer(wire) => self.apply_offer(hwnd, &wire),
                Cmd::Clear => self.clear(hwnd),
                Cmd::Read { wire, resp } => {
                    let _ = resp.send(self.read(&wire));
                }
                Cmd::Shutdown => {
                    // Clear first: otherwise WM_RENDERALLFORMATS leaves the client's bytes on the host.
                    self.clear(hwnd);
                    // SAFETY: our own live window; triggers WM_DESTROY → PostQuitMessage → pump exit.
                    unsafe {
                        let _ = DestroyWindow(hwnd);
                    }
                    return;
                }
            }
        }
    }

    fn apply_offer(&self, hwnd: HWND, wire: &[String]) {
        let fmts = self.formats_for_offer(wire);
        if fmts.is_empty() {
            self.clear(hwnd);
            return;
        }
        if open_clipboard_retry(hwnd).is_err() {
            tracing::debug!("clipboard: OpenClipboard for set_offer failed");
            return;
        }
        let _guard = ClipboardGuard;
        // SAFETY: the clipboard is open (ClipboardGuard closes it); EmptyClipboard makes us the owner,
        // then each SetClipboardData(_, None) registers a delayed-render promise for that format.
        unsafe {
            let _ = EmptyClipboard();
            for &f in &fmts {
                let _ = SetClipboardData(f, None);
            }
        }
        *self.offered.borrow_mut() = fmts;
        *self.offered_wires.borrow_mut() = wire.to_vec();
    }

    /// Empty the clipboard only if we still own it.
    fn clear(&self, hwnd: HWND) {
        let had = {
            let mut o = self.offered.borrow_mut();
            let was = !o.is_empty();
            o.clear();
            was
        };
        if !had {
            return;
        }
        // SAFETY: GetClipboardOwner has no preconditions.
        let owner = unsafe { GetClipboardOwner() }.unwrap_or_default();
        if owner.0 != hwnd.0 {
            return;
        }
        if open_clipboard_retry(hwnd).is_err() {
            return;
        }
        let _guard = ClipboardGuard;
        // SAFETY: the clipboard is open (ClipboardGuard closes it); empty it to drop our promises.
        unsafe {
            let _ = EmptyClipboard();
        }
    }

    fn read(&self, wire: &str) -> anyhow::Result<Vec<u8>> {
        let mut fmt = self
            .format_for_wire(wire)
            .context("unsupported wire MIME")?;
        // PNG fetch with no PNG atom: read CF_DIB and convert.
        let mut via_dib = false;
        // SAFETY: IsClipboardFormatAvailable has no preconditions and needs no open clipboard.
        if wire == WIRE_PNG && unsafe { IsClipboardFormatAvailable(fmt) }.is_err() {
            fmt = CF_DIB.0 as u32;
            via_dib = true;
        }
        // Own delayed-render offer: GetClipboardData would re-enter our WM_RENDERFORMAT.
        // SAFETY: GetClipboardOwner has no preconditions.
        if unsafe { GetClipboardOwner() }.unwrap_or_default().0 == self.own_hwnd.0 {
            anyhow::bail!("clipboard currently held by our own offer");
        }
        open_clipboard_retry(self.own_hwnd)?;
        let _guard = ClipboardGuard;
        // SAFETY: the clipboard is open (ClipboardGuard closes it). GetClipboardData hands back a
        // clipboard-owned HGLOBAL (we must NOT free it); GlobalLock/Size/Unlock are balanced and we
        // copy exactly GlobalSize bytes out before the lock is released.
        let raw = unsafe {
            let handle = GetClipboardData(fmt).context("GetClipboardData")?;
            let hg = HGLOBAL(handle.0);
            let p = GlobalLock(hg);
            if p.is_null() {
                anyhow::bail!("GlobalLock failed");
            }
            let n = GlobalSize(hg);
            let mut buf = vec![0u8; n];
            std::ptr::copy_nonoverlapping(p as *const u8, buf.as_mut_ptr(), n);
            let _ = GlobalUnlock(hg);
            buf
        };
        if via_dib {
            return winfmt::dib_to_png(&raw).context("CF_DIB -> PNG conversion failed");
        }
        Ok(convert_from_win(wire, &raw))
    }

    /// `WM_RENDERFORMAT`: block this thread (bounded) for client bytes, then `SetClipboardData`.
    fn on_render_format(&self, fmt: u32) {
        // Synthesized CF_DIB has no wire kind: PNG (lossless+alpha), then JPEG, then GIF frame 1.
        let wire: &str = if fmt == CF_DIB.0 as u32 {
            let offered = self.offered_wires.borrow();
            match [WIRE_PNG, WIRE_JPEG, WIRE_GIF]
                .into_iter()
                .find(|w| offered.iter().any(|o| o == w))
            {
                Some(w) => w,
                None => return,
            }
        } else {
            match self.wire_for_format(fmt) {
                Some(w) => w,
                None => return,
            }
        };
        let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let ev = ClipEvent::Paste {
            mime: wire.to_string(),
            responder: PasteResponder::Sync(tx),
        };
        if self.clip_tx.send(ev).is_err() {
            return;
        }
        let bytes = match rx.recv_timeout(RENDER_TIMEOUT) {
            Ok(b) => b,
            Err(_) => return, // unrendered = empty paste
        };
        let win_bytes = if fmt == CF_DIB.0 as u32 {
            // Convert the served image; a decode miss leaves the format unrendered (empty paste).
            match winfmt::image_to_dib(&bytes) {
                Some(d) => d,
                None => return,
            }
        } else {
            convert_to_win(wire, &bytes)
        };
        let Ok(hg) = alloc_hglobal(&win_bytes) else {
            return;
        };
        // Do NOT OpenClipboard here — the pasting app already holds it open across WM_RENDERFORMAT.
        // SAFETY: `hg` is a freshly-filled moveable HGLOBAL. On success the clipboard takes ownership
        // (we must not free it); on failure ownership stays with us, so we free it.
        unsafe {
            if SetClipboardData(fmt, Some(HANDLE(hg.0))).is_err() {
                let _ = GlobalFree(Some(hg));
            }
        }
    }

    fn format_for_wire(&self, wire: &str) -> Option<u32> {
        match wire {
            WIRE_TEXT => Some(CF_UNICODETEXT.0 as u32),
            WIRE_HTML => Some(self.fmt_html),
            WIRE_RTF => Some(self.fmt_rtf),
            WIRE_PNG => Some(self.fmt_png),
            WIRE_JPEG => Some(self.fmt_jfif),
            WIRE_GIF => Some(self.fmt_gif),
            _ => None,
        }
    }

    fn wire_for_format(&self, fmt: u32) -> Option<&'static str> {
        if fmt == CF_UNICODETEXT.0 as u32 {
            Some(WIRE_TEXT)
        } else if fmt == self.fmt_html {
            Some(WIRE_HTML)
        } else if fmt == self.fmt_rtf {
            Some(WIRE_RTF)
        } else if fmt == self.fmt_png {
            Some(WIRE_PNG)
        } else if fmt == self.fmt_jfif {
            Some(WIRE_JPEG)
        } else if fmt == self.fmt_gif {
            Some(WIRE_GIF)
        } else {
            None
        }
    }

    /// Promise one id per wire MIME. OS synthesizes CF_TEXT/CF_OEMTEXT from CF_UNICODETEXT; do not fan out.
    fn formats_for_offer(&self, wire: &[String]) -> Vec<u32> {
        let mut out = Vec::new();
        for w in wire {
            if let Some(f) = self.format_for_wire(w) {
                if !out.contains(&f) {
                    out.push(f);
                }
            }
            // Most pasting apps ask for CF_DIB, not PNG/JFIF/GIF. Windows synthesizes
            // CF_BITMAP/CF_DIBV5 from it; `on_render_format` converts the richest offered image.
            if matches!(w.as_str(), WIRE_PNG | WIRE_JPEG | WIRE_GIF)
                && !out.contains(&(CF_DIB.0 as u32))
            {
                out.push(CF_DIB.0 as u32);
            }
        }
        out
    }
}

/// Async-side handle. All Win32 work runs on the message-loop thread.
pub struct WindowsClipboard {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<Cmd>,
    /// `HWND` as `isize` so this handle stays `Send`/`Sync`. Rebuilt only for `PostMessage` wakeups.
    hwnd: isize,
    current_wire: Arc<Mutex<Vec<String>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl WindowsClipboard {
    pub async fn open() -> anyhow::Result<(
        WindowsClipboard,
        tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
    )> {
        let (clip_tx, clip_rx) = tokio::sync::mpsc::unbounded_channel::<ClipEvent>();
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<Cmd>();
        let current_wire = Arc::new(Mutex::new(Vec::new()));

        // Process-global atoms: fine off the message thread, and bring-up fails before the pump starts.
        let fmt_html = register_format(w!("HTML Format"))?;
        let fmt_rtf = register_format(w!("Rich Text Format"))?;
        let fmt_png = register_format(w!("PNG"))?;
        let fmt_jfif = register_format(w!("JFIF"))?;
        let fmt_gif = register_format(w!("GIF"))?;

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<anyhow::Result<isize>>();
        let cw = Arc::clone(&current_wire);
        let join = std::thread::Builder::new()
            .name("punktfunk-clipboard-win".into())
            .spawn(move || {
                pump_thread(
                    clip_tx, cmd_rx, cw, fmt_html, fmt_rtf, fmt_png, fmt_jfif, fmt_gif, ready_tx,
                )
            })
            .context("spawn windows clipboard thread")?;

        let hwnd = match tokio::time::timeout(Duration::from_secs(3), ready_rx).await {
            Ok(Ok(Ok(h))) => h,
            Ok(Ok(Err(e))) => return Err(e),
            Ok(Err(_)) => anyhow::bail!("windows clipboard thread exited during bring-up"),
            Err(_) => anyhow::bail!("windows clipboard bring-up timed out"),
        };

        Ok((
            WindowsClipboard {
                cmd_tx,
                hwnd,
                current_wire,
                join: Some(join),
            },
            clip_rx,
        ))
    }

    pub fn current_wire_mimes(&self) -> Vec<String> {
        self.current_wire.lock().unwrap().clone()
    }

    pub fn set_offer(&self, wire_mimes: &[String]) {
        let _ = self.cmd_tx.send(Cmd::SetOffer(wire_mimes.to_vec()));
        self.wake();
    }

    pub fn clear_offer(&self) {
        let _ = self.cmd_tx.send(Cmd::Clear);
        self.wake();
    }

    pub async fn read_current(&self, wire_mime: &str) -> anyhow::Result<Vec<u8>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(Cmd::Read {
                wire: wire_mime.to_string(),
                resp: tx,
            })
            .map_err(|_| anyhow::anyhow!("clipboard thread gone"))?;
        self.wake();
        rx.await
            .map_err(|_| anyhow::anyhow!("clipboard read dropped"))?
    }

    fn wake(&self) {
        // SAFETY: PostMessageW is documented thread-safe; `hwnd` is our message window (or already
        // destroyed, in which case the post harmlessly fails and is ignored).
        let _ = unsafe {
            PostMessageW(
                Some(HWND(self.hwnd as *mut core::ffi::c_void)),
                WM_APP_CMD,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

impl Drop for WindowsClipboard {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        self.wake();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Pairs with a successful `open_clipboard_retry`; `CloseClipboard` on every exit, including `?`.
struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: constructed only after a successful OpenClipboard on this thread.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn register_format(name: PCWSTR) -> anyhow::Result<u32> {
    // SAFETY: RegisterClipboardFormatW is thread-agnostic and process-global; `name` is a static
    // NUL-terminated wide literal.
    let id = unsafe { RegisterClipboardFormatW(name) };
    if id == 0 {
        anyhow::bail!("RegisterClipboardFormatW failed");
    }
    Ok(id)
}

/// Moveable HGLOBAL. Zero-init so a 0-byte payload still locks; ownership moves on `SetClipboardData`.
fn alloc_hglobal(bytes: &[u8]) -> anyhow::Result<HGLOBAL> {
    // SAFETY: allocate at least one byte (GlobalLock of a 0-size block is unreliable), lock it, copy
    // the payload in, unlock. Alloc/lock/unlock are balanced; on lock failure we free before erroring.
    unsafe {
        let hg = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1))
            .context("GlobalAlloc")?;
        let p = GlobalLock(hg);
        if p.is_null() {
            let _ = GlobalFree(Some(hg));
            anyhow::bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
        let _ = GlobalUnlock(hg);
        Ok(hg)
    }
}

fn open_clipboard_retry(hwnd: HWND) -> anyhow::Result<()> {
    for _ in 0..OPEN_RETRIES {
        // SAFETY: OpenClipboard with our window as owner; balanced by ClipboardGuard/CloseClipboard.
        if unsafe { OpenClipboard(Some(hwnd)) }.is_ok() {
            return Ok(());
        }
        std::thread::sleep(OPEN_RETRY_DELAY);
    }
    anyhow::bail!("OpenClipboard failed after retries")
}

fn convert_from_win(wire: &str, raw: &[u8]) -> Vec<u8> {
    match wire {
        WIRE_TEXT => winfmt::text_from_utf16(raw),
        WIRE_HTML => winfmt::html_from_cf(raw),
        WIRE_RTF => winfmt::rtf_from_cf(raw),
        _ => raw.to_vec(), // PNG and unknown: identity
    }
}

fn convert_to_win(wire: &str, wire_bytes: &[u8]) -> Vec<u8> {
    match wire {
        WIRE_TEXT => winfmt::text_to_utf16(wire_bytes),
        WIRE_HTML => winfmt::html_to_cf(wire_bytes),
        _ => wire_bytes.to_vec(), // RTF/PNG and unknown: identity
    }
}

fn create_window() -> anyhow::Result<HWND> {
    // SAFETY: standard window-class registration + message-only window creation; every argument is a
    // valid handle / static literal, and `wndproc` matches the WNDPROC ABI.
    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(PCWSTR::null())
            .context("GetModuleHandleW")?
            .into();
        let class_name = w!("PunktfunkClipboardWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class_name,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            let code = GetLastError();
            if code.0 != ERROR_CLASS_ALREADY_EXISTS {
                anyhow::bail!("RegisterClassW failed: {code:?}");
            }
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!(""),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance),
            None,
        )
        .context("CreateWindowExW")?;
        Ok(hwnd)
    }
}

#[allow(clippy::too_many_arguments)]
fn pump_thread(
    clip_tx: ClipTx,
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<Cmd>,
    current_wire: Arc<Mutex<Vec<String>>>,
    fmt_html: u32,
    fmt_rtf: u32,
    fmt_png: u32,
    fmt_jfif: u32,
    fmt_gif: u32,
    ready_tx: tokio::sync::oneshot::Sender<anyhow::Result<isize>>,
) {
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    // Outlives the boxed state so `Closed` can fire after the pump ends.
    let closed_tx = clip_tx.clone();

    let state = Box::new(WinClip {
        clip_tx,
        current_wire,
        cmd_rx: RefCell::new(cmd_rx),
        offered: RefCell::new(Vec::new()),
        offered_wires: RefCell::new(Vec::new()),
        fmt_html,
        fmt_rtf,
        fmt_png,
        fmt_jfif,
        fmt_gif,
        own_hwnd: hwnd,
    });
    let ptr = Box::into_raw(state);
    // SAFETY: stash the state pointer for the WndProc; the window was created on this thread and the
    // pointer stays valid until we reclaim the Box after the pump exits.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, ptr as isize);
    }

    // Listener delivers only *subsequent* changes; seed `current_wire` from what is already there.
    {
        // SAFETY: `ptr` is the live state we just stored; only this thread dereferences it.
        let st = unsafe { &*ptr };
        *st.current_wire.lock().unwrap() = st.available_wire_mimes();
    }

    // SAFETY: `hwnd` is our live window; start receiving WM_CLIPBOARDUPDATE.
    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
        // SAFETY: tear down the half-built window and reclaim the leaked state box.
        unsafe {
            let _ = DestroyWindow(hwnd);
            drop(Box::from_raw(ptr));
        }
        let _ = ready_tx.send(Err(
            anyhow::Error::new(e).context("AddClipboardFormatListener")
        ));
        return;
    }

    let _ = ready_tx.send(Ok(hwnd.0 as isize));

    // SAFETY: the standard Win32 message pump. GetMessageW returns >0 for a message, 0 for WM_QUIT,
    // and -1 on error — `.0 > 0` exits on both 0 and -1.
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // SAFETY: `ptr` came from Box::into_raw above, is dereferenced only on this thread, and the
    // message loop has ended so no further access occurs.
    unsafe {
        drop(Box::from_raw(ptr));
    }
    let _ = closed_tx.send(ClipEvent::Closed);
}

/// Class `WNDPROC`. State at `GWLP_USERDATA`; this thread only. A safe fn coerces to the system ABI.
extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: GWLP_USERDATA holds the `*const WinClip` stored right after window creation (0/null for
    // the WM_(NC)CREATE messages that fire before that — handled by the null check below).
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WinClip;
    if ptr.is_null() {
        // SAFETY: default processing before our state pointer is attached.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: `ptr` is the live Box<WinClip> leaked in pump_thread, owned by this (the only) message
    // thread and freed only after the pump exits; the WndProc is not re-entered for this window, so
    // `&*ptr` is a valid shared borrow.
    let st = unsafe { &*ptr };
    match msg {
        WM_CLIPBOARDUPDATE => {
            st.on_clipboard_update(hwnd);
            LRESULT(0)
        }
        WM_RENDERFORMAT => {
            st.on_render_format(wparam.0 as u32);
            LRESULT(0)
        }
        WM_APP_CMD => {
            st.drain_commands(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends the GetMessageW pump by posting WM_QUIT to this thread's queue.
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        // SAFETY: arguments are the ones Windows delivered for this window.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
