//! The recovery ladder's composition canary: a host-owned 1×1 layered window shown for a moment
//! in a corner of the target display, so DWM must compose that head. It is drawn content, not
//! input: no pointer moves, and it works whether or not DWM composites the cursor.
//!
//! One thread owns the window and pumps its messages. The pixel is black at alpha 1–2 and hides
//! again after [`SHOWN_FOR`], so nobody sees it and nothing stays topmost over a game.

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, BLACK_BRUSH, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW,
    SetLayeredWindowAttributes, SetWindowPos, ShowWindow, HWND_TOPMOST, LWA_ALPHA, MSG, PM_REMOVE,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, SW_HIDE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Showing and hiding are two composes; the first stays long enough for DWM to sample it.
const SHOWN_FOR: Duration = Duration::from_millis(250);

/// `(x, y, w, h)` in desktop coordinates.
type Rect = (i32, i32, i32, i32);

/// Show the canary in the bottom-right pixel of `rect`. `false` when there is no probe window.
pub fn present(rect: Rect) -> bool {
    static PROBE: OnceLock<Option<Sender<Rect>>> = OnceLock::new();
    PROBE
        .get_or_init(spawn)
        .as_ref()
        .is_some_and(|tx| tx.send(rect).is_ok())
}

fn spawn() -> Option<Sender<Rect>> {
    let (tx, rx) = mpsc::channel();
    let (made_tx, made_rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("pf-compose-probe".into())
        .spawn(move || {
            let hwnd = create();
            let _ = made_tx.send(hwnd.is_some());
            if let Some(hwnd) = hwnd {
                run(hwnd, &rx);
            }
        })
        .ok()?;
    made_rx.recv().ok()?.then_some(tx)
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: forwards the framework's own arguments for this window.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn create() -> Option<HWND> {
    let class = w!("pf-compose-probe");
    // SAFETY: window bring-up on this thread. `class` is a static literal and every handle is
    // the preceding call's return.
    unsafe {
        let hinstance = GetModuleHandleW(None).ok()?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            tracing::warn!("compose probe: RegisterClassW refused");
            return None;
        }
        let ex = WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
        CreateWindowExW(
            ex | WS_EX_NOACTIVATE,
            class,
            class,
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(wc.hInstance),
            None,
        )
        .inspect_err(|e| tracing::warn!(error = %e, "compose probe window not created"))
        .ok()
    }
}

fn run(hwnd: HWND, rx: &Receiver<Rect>) {
    let mut alpha = 1u8;
    let mut hide_at: Option<Instant> = None;
    loop {
        let wait = hide_at.map_or(Duration::from_millis(100), |t| {
            t.saturating_duration_since(Instant::now())
        });
        match rx.recv_timeout(wait) {
            Ok((x, y, w, h)) => {
                // A changed window is a dirty one even when it is still showing.
                alpha = 3 - alpha;
                // SAFETY: `hwnd` is this thread's live window.
                unsafe {
                    let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        x + w.max(1) - 1,
                        y + h.max(1) - 1,
                        1,
                        1,
                        SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                }
                hide_at = Some(Instant::now() + SHOWN_FOR);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if hide_at.is_some_and(|t| Instant::now() >= t) {
            // SAFETY: as above.
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            hide_at = None;
        }
        let mut msg = MSG::default();
        // SAFETY: `msg` is a live local; this thread's window messages are dispatched here.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = DispatchMessageW(&msg);
            }
        }
    }
}
