//! GDI cursor poller: the Windows cursor-SHAPE source for the cursor-forward
//! channel. Off-thread `GetCursorInfo` + `HCURSOR` rasterise, published as
//! [`pf_frame::CursorOverlay`].
//!
//! IddCx hardware-cursor query (`CursorShm`) is alpha-only: `IDDCX_CURSOR_SHAPE_TYPE`
//! has no monochrome, the OS pre-converts mono to masked-color, and MASKED_COLOR
//! delivery is dead on modern builds. The driver still declares XOR FULL so DWM
//! excludes every cursor type from the IDD frame; this poller is the shape that
//! then gets forwarded or host-composited. DXGI `GetFramePointerShape` is not
//! used: `PointerPosition.Visible` goes stale under injected input, and it burns
//! one of four duplication slots.
//!
//! The host runs as SYSTEM inside the interactive session on `winsta0\default`
//! (`windows/service.rs` `spawn_host`), so this thread reads the session cursor
//! directly. Pin via the overlay snapshot and `pf_win_display::secure_desktop`. Invert/mask
//! contracts live in `cursor_raster.rs`; design in `design/remote-desktop-sweep.md`.

use super::*;
use crate::cursor_raster::{
    alpha_is_empty, compose_overlay, masked_color_to_rgba, mono_planes_to_rgba, Shape,
};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, HDESK,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CopyIcon, DestroyIcon, GetCursorInfo, GetIconInfo, CURSORINFO, HICON, ICONINFO,
};

const CURSOR_SHOWING: u32 = 0x1;
const CURSOR_SUPPRESSED: u32 = 0x2;

/// Off-thread GDI cursor poller. User32/gdi32 stay off the capture/encode thread;
/// the capture tick is one uncontended mutex read plus an `Arc` clone.
pub(super) struct CursorPoller {
    slot: Arc<Mutex<Option<pf_frame::CursorOverlay>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl CursorPoller {
    /// 4 ms ≈ 250 Hz. The polled position is also the composite-blend position, so
    /// it must out-pace a 240 fps session; 16 ms reused a stale spot for ~4 frames.
    const INTERVAL: Duration = Duration::from_millis(4);
    /// Unconditional input-desktop reattach. `GetCursorInfo` on a stale desktop
    /// *succeeds* with stale data, so there is no failure signal. Each reattach also
    /// refreshes `pf_win_display::secure_desktop`, the fallback for the
    /// desktop-switch WinEvent; 250 ms keeps a missed event's freeze short.
    const REATTACH: Duration = Duration::from_millis(250);
    /// Same-handle extent re-probe. Scale changes are human-timescale; the probe
    /// reads dimensions only, so 4 Hz is ample and a ≤250 ms size lag is invisible.
    const EXTENT_PROBE: Duration = Duration::from_millis(250);

    /// Spawn for virtual display `target_id`. `rect` seeds the desktop rect
    /// (`source_desktop_rect` order: x, y, w, h). Positions are desktop-global;
    /// the overlay is frame-relative, and a pointer outside the rect is
    /// `visible: false` (per-output, matching shm and the Linux portal).
    ///
    /// A SEED, not the value: the poll thread re-queries the rect on its [`Self::REATTACH`] cadence.
    /// It used to be captured once here and used forever for BOTH the desktop→frame offset and the
    /// `in_rect` test, while both mid-session mode-change paths (`resize_output` and
    /// `poll_display_hdr`) keep the same poller — so after an in-place resize the
    /// pointer was clipped to the OLD rect and offset by a stale origin. Re-querying on the poll
    /// thread is what keeps the CCD call off the capture/encode thread, which is the whole reason
    /// this poller exists (see `DescriptorPoller`).
    pub(super) fn spawn(
        ccd: pf_win_display::win_display::CcdTargetKey,
        rect: (i32, i32, i32, i32),
    ) -> Self {
        let slot: Arc<Mutex<Option<pf_frame::CursorOverlay>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (slot_t, stop_t) = (slot.clone(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("pf-cursor-poll".into())
            .spawn(move || run(ccd, rect, &slot_t, &stop_t))
            .ok();
        if thread.is_none() {
            tracing::warn!("cursor poller thread spawn failed — cursor falls back to driver shm");
        }
        Self { slot, stop, thread }
    }

    /// Latest overlay; `None` until the first successful rasterise.
    pub(super) fn read(&self) -> Option<pf_frame::CursorOverlay> {
        self.slot.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Worker still running; `false` degrades the capturer to the shm read.
    pub(super) fn alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }
}

impl Drop for CursorPoller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join(); // worker sleeps ≤ INTERVAL — a bounded join
        }
    }
}

fn run(
    ccd: pf_win_display::win_display::CcdTargetKey,
    mut rect: (i32, i32, i32, i32),
    slot: &Mutex<Option<pf_frame::CursorOverlay>>,
    stop: &AtomicBool,
) {
    // Physical pixels on this thread: `rect` is CCD (always physical), and a virtualized
    // `GetCursorInfo` misses the frame pixel on a scaled display. The BITMAP is a process
    // matter: an unaware or system-aware process is handed the cursor for its launch-time
    // DPI whatever this thread says, so the host manifest declares v2 (build.rs).

    // SAFETY: takes and returns only a by-value context handle; affects this thread only.
    let _ = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let mut desktop = DesktopBinding::default();
    // best-effort: already on winsta0\default if this fails
    desktop.reattach();
    let mut last_attach = Instant::now();
    let mut cache = ShapeCache::new();

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(CursorPoller::INTERVAL);
        if last_attach.elapsed() >= CursorPoller::REATTACH {
            last_attach = Instant::now();
            desktop.reattach();
            // Let a failed handle be tried again. The skip is cleared only by a SUCCESSFUL
            // rasterise of a DIFFERENT handle, and the arrow's HCURSOR is stable for the
            // session, so a transient GDI failure (a null `GetDC` across a desktop switch)
            // would otherwise freeze the shape until the session ended.
            cache.failed = 0;
            // …and re-read the target's desktop rect from the display actor's snapshot (no CCD
            // call here): a resize, an HDR recreate or the user moving this display changes BOTH
            // the origin positions are made relative to and the extent `in_rect` tests against,
            // and this poller outlives all of them. `None` keeps the last good value — a target
            // briefly absent must not park the pointer at a `(0, 0, 0, 0)` rect (all invisible).
            let fresh = pf_win_display::display_events::snapshot().source_rect(ccd);
            if let Some(fresh) = fresh {
                if fresh != rect {
                    tracing::info!(
                        target = %ccd,
                        from = ?rect,
                        to = ?fresh,
                        "cursor poller: target desktop rect changed — re-basing pointer positions"
                    );
                    rect = fresh;
                }
            }
        }

        let mut ci = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `ci` is a live, correctly-sized out-param for this synchronous call; no pointer
        // escapes it.
        if unsafe { GetCursorInfo(&mut ci) }.is_err() {
            // Desktop gone (secure-desktop switch mid-call) — rebind next tick;
            // the slot keeps its last snapshot.
            desktop.reattach();
            last_attach = Instant::now();
            continue;
        }

        let flags = ci.flags.0;
        // Touch or pen input: Windows stops drawing the pointer, but no app hid it. Publishing
        // a hide would read as a grab and flip the client to relative; keep the last snapshot.
        if flags & CURSOR_SUPPRESSED != 0 {
            continue;
        }
        let showing = flags & CURSOR_SHOWING != 0;
        cache.refresh(ci.hCursor, showing, ccd);
        // `SetCursor(NULL)` (game/video hide) leaves `CURSOR_SHOWING` set with a NULL
        // `hCursor`; flags alone would publish the last shape.
        let shown = showing && ci.hCursor.0 as isize != 0;
        let pos = (ci.ptScreenPos.x - rect.0, ci.ptScreenPos.y - rect.1);
        let overlay = compose_overlay(pos, rect, shown, cache.shape.as_ref());
        *slot.lock().unwrap_or_else(|p| p.into_inner()) = overlay;
    }
}

/// The rasterised shape and when to redo it.
struct ShapeCache {
    shape: Option<Shape>,
    /// Handle `shape` was rasterised from.
    cached: isize,
    /// A handle whose rasterise failed; not retried every tick.
    failed: isize,
    serial: u64,
    logged_live: bool,
    last_extent: Instant,
}

impl ShapeCache {
    fn new() -> Self {
        Self {
            shape: None,
            cached: 0,
            failed: 0,
            serial: 0,
            logged_live: false,
            last_extent: Instant::now(),
        }
    }

    /// Rasterise `cursor` on a handle change only; a hidden cursor keeps the cached shape, and
    /// an animated one publishes frame 0. Handle identity cannot see a re-render: Windows
    /// rebuilds system cursors when the scale under the pointer changes but keeps the shared
    /// handle for the session (arrow is 0x10003 throughout), so the extent is re-read every
    /// [`CursorPoller::EXTENT_PROBE`] and a move drops the cache.
    fn refresh(
        &mut self,
        cursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR,
        showing: bool,
        ccd: pf_win_display::win_display::CcdTargetKey,
    ) {
        let handle = cursor.0 as isize;
        if showing && handle != 0 && handle == self.cached {
            if self.last_extent.elapsed() >= CursorPoller::EXTENT_PROBE {
                self.last_extent = Instant::now();
                if let (Some(now), Some(s)) = (cursor_extent(cursor), self.shape.as_ref()) {
                    if now != (s.w, s.h) {
                        tracing::info!(
                            target = %ccd,
                            "cursor: the pointer bitmap resized under a stable handle \
                             ({}x{} -> {}x{}) — re-rasterising (the scale under the pointer moved)",
                            s.w,
                            s.h,
                            now.0,
                            now.1
                        );
                        self.cached = 0; // re-rasterise below, on this same tick
                    }
                }
            }
        } else {
            // A handle change re-rasterises on its own — hold the probe off so it can't fire on
            // the very next tick against a shape that is current by construction.
            self.last_extent = Instant::now();
        }
        if !showing || handle == 0 || handle == self.cached || handle == self.failed {
            return;
        }
        match rasterize(cursor) {
            Some((rgba, w, h, hot_x, hot_y)) => {
                self.serial += 1;
                self.shape = Some(Shape {
                    rgba: std::sync::Arc::new(rgba),
                    w,
                    h,
                    hot_x,
                    hot_y,
                    serial: self.serial,
                });
                self.cached = handle;
                self.failed = 0;
                if !self.logged_live {
                    self.logged_live = true;
                    tracing::info!(
                        target = %ccd,
                        "cursor poller live — GDI shape source publishing (serial 1: {w}x{h})"
                    );
                }
            }
            // The owning app may have destroyed the cursor mid-read; keep the previous
            // shape and don't hammer this handle again until it changes.
            None => self.failed = handle,
        }
    }
}

/// Owned input-desktop handle: keep the current binding, swap on demand, close
/// exactly once (same reattach model as [`SendInputInjector`] in `pf-inject`).
///
/// `.1` is the desktop this thread started on, captured before the first rebind and only ever
/// restored — `GetThreadDesktop` returns a borrowed handle, so it is never closed.
#[derive(Default)]
struct DesktopBinding(Option<HDESK>, Option<HDESK>);

impl DesktopBinding {
    /// Rebind to the current input desktop (the binding stays put if it cannot be
    /// opened), then refresh the process-wide secure-desktop verdict.
    fn reattach(&mut self) {
        const GENERIC_ALL: u32 = 0x1000_0000;
        // SAFETY: `OpenInputDesktop`/`SetThreadDesktop`/`CloseDesktop` take only by-value args.
        // `OpenInputDesktop` yields an owned `HDESK` only on `Ok`; it is either installed (and the
        // previously-owned handle closed exactly once) or closed on failure — no handle is leaked
        // or used after close. `SetThreadDesktop` rebinds only this calling thread (which owns
        // no windows/hooks, so the rebind cannot fail on that account).
        unsafe {
            // Where this thread started, captured once and BEFORE the first rebind — after it,
            // `GetThreadDesktop` would just hand back the desktop we are about to own.
            if self.1.is_none() {
                self.1 = GetThreadDesktop(GetCurrentThreadId()).ok();
            }
            if let Ok(h) = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(GENERIC_ALL),
            ) {
                if SetThreadDesktop(h).is_ok() {
                    if let Some(old) = self.0.replace(h) {
                        let _ = CloseDesktop(old);
                    }
                } else {
                    let _ = CloseDesktop(h);
                }
            }
        }
        pf_win_display::refresh_secure_desktop();
    }
}

impl Drop for DesktopBinding {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // `CloseDesktop` refuses a desktop still assigned to the calling thread, so put the
            // thread back on the one it started on — otherwise the handle leaks, one per
            // session. Same order as `input_desktop.rs`: restore, then close.
            // SAFETY: both are FFI calls on by-value args. `self.1` is borrowed (GetThreadDesktop
            // creates no handle, so it is never closed); `h` is ours, closed exactly once.
            unsafe {
                if let Some(previous) = self.1 {
                    let _ = SetThreadDesktop(previous);
                }
                let _ = CloseDesktop(h);
            }
        }
    }
}

/// Rasterise `hcursor` to straight-alpha RGBA. `None` on any failure (caller
/// keeps the previous shape).
fn rasterize(hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR) -> RasterOut {
    // CopyIcon first: the owner can destroy its HCURSOR between GetCursorInfo
    // and the reads below; the copy is ours.

    // SAFETY: `HICON(hcursor.0)` reinterprets the cursor handle as an icon handle (cursors ARE
    // icons in user32); CopyIcon yields an owned HICON we destroy below.
    let Ok(icon) = (unsafe { CopyIcon(HICON(hcursor.0)) }) else {
        return None;
    };
    let mut ii = ICONINFO::default();
    // SAFETY: `ii` is a live out-param. On Ok it hands us COPIES of the mask/color bitmaps —
    // both deleted below (GDI-handle leak otherwise).
    let got = unsafe { GetIconInfo(icon, &mut ii) };
    let out = if got.is_ok() { convert(&ii) } else { None };
    // SAFETY: deleting the two bitmap copies GetIconInfo returned (null-safe: DeleteObject on a
    // null HGDIOBJ fails harmlessly) and the icon copy — each exactly once.
    unsafe {
        let _ = DeleteObject(ii.hbmColor.into());
        let _ = DeleteObject(ii.hbmMask.into());
        let _ = DestroyIcon(icon);
    }
    out.map(|(rgba, w, h)| {
        let hot_x = ii.xHotspot.min(w.saturating_sub(1));
        let hot_y = ii.yHotspot.min(h.saturating_sub(1));
        (rgba, w, h, hot_x, hot_y)
    })
}

type RasterOut = Option<(Vec<u8>, u32, u32, u32, u32)>;

/// Bitmap extent of `hcursor` — the `(w, h)` [`convert`] would derive, no pixel
/// read. A re-render keeps the handle; `None` is "no verdict" (keep the cache).
fn cursor_extent(hcursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR) -> Option<(u32, u32)> {
    // CopyIcon first, same reason as `rasterize`: the owner can destroy the
    // HCURSOR between GetCursorInfo and the reads below.

    // SAFETY: `HICON(hcursor.0)` reinterprets the cursor handle as an icon handle (cursors ARE
    // icons in user32); CopyIcon yields an owned HICON destroyed below.
    let icon = unsafe { CopyIcon(HICON(hcursor.0)) }.ok()?;
    let mut ii = ICONINFO::default();
    // SAFETY: `ii` is a live out-param. On Ok it hands us COPIES of the mask/color bitmaps — both
    // deleted below (GDI-handle leak otherwise).
    let got = unsafe { GetIconInfo(icon, &mut ii) };
    // Mirrors `convert`'s two families: a color cursor's extent is its color bitmap's; a
    // monochrome one's mask carries the AND plane OVER the XOR plane, so its height is doubled.
    let extent = got.is_ok().then_some(()).and_then(|()| {
        if !ii.hbmColor.is_invalid() {
            bitmap_extent(ii.hbmColor)
        } else {
            let (w, h) = bitmap_extent(ii.hbmMask)?;
            (h >= 2 && h % 2 == 0).then_some((w, h / 2))
        }
    });
    // SAFETY: deleting the two bitmap copies GetIconInfo returned (null-safe: DeleteObject on a
    // null HGDIOBJ fails harmlessly) and the icon copy — each exactly once.
    unsafe {
        let _ = DeleteObject(ii.hbmColor.into());
        let _ = DeleteObject(ii.hbmMask.into());
        let _ = DestroyIcon(icon);
    }
    extent
}

/// Dimensions under [`read_bitmap_32`]'s caps so the two agree: a bitmap
/// `rasterize` would reject must not read here as a size change.
fn bitmap_extent(hbm: HBITMAP) -> Option<(u32, u32)> {
    let mut bm = BITMAP::default();
    // SAFETY: `bm` is a live out-param sized exactly as passed; GetObjectW only writes into it.
    let n = unsafe {
        GetObjectW(
            hbm.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        )
    };
    if n == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 || bm.bmWidth > 512 || bm.bmHeight > 1024 {
        return None;
    }
    Some((bm.bmWidth as u32, bm.bmHeight as u32))
}

/// Convert ICONINFO bitmaps to straight RGBA. Two families:
///
/// - color (`hbmColor` set): 32bpp BGRA. If alpha is entirely empty (old-style
///   masked-color, including Win11's coloured I-beam) the AND mask is the same
///   four-state table as monochrome, with the colour bitmap as XOR. Treating
///   AND=1 as "always transparent" drops invert pixels; the I-beam is almost
///   entirely invert.
/// - monochrome (`hbmColor` null): `hbmMask` is double height — AND over XOR.
///   (0,0) black, (0,1) white, (1,0) transparent, (1,1) invert. Invert is
///   unrepresentable in straight alpha, so it becomes opaque black with a white
///   outline grown into adjacent transparency (keeps the I-beam legible).
fn convert(ii: &ICONINFO) -> Option<(Vec<u8>, u32, u32)> {
    // SAFETY: GetDC(None) yields the screen DC, released below on every path; it is only used
    // as the GetDIBits reference DC.
    let dc = unsafe { GetDC(None) };
    let result = (|| {
        if !ii.hbmColor.is_invalid() {
            let color = read_bitmap_32(dc, ii.hbmColor)?;
            let (w, h) = (color.w as u32, color.h as u32);
            let mut rgba = bgra_to_rgba(&color.bgra);
            if alpha_is_empty(&rgba) {
                let mask = read_bitmap_32(dc, ii.hbmMask)?;
                if mask.w != color.w || mask.h < color.h {
                    return None;
                }
                rgba = masked_color_to_rgba(&rgba, &mask.bgra, w as usize, h as usize);
            }
            Some((rgba, w, h))
        } else {
            let mask = read_bitmap_32(dc, ii.hbmMask)?;
            if mask.h < 2 || mask.h % 2 != 0 {
                return None;
            }
            let (w, h) = (mask.w as usize, (mask.h / 2) as usize);
            let (and_plane, xor_plane) = mask.bgra.split_at(h * w * 4);
            let rgba = mono_planes_to_rgba(and_plane, xor_plane, w, h);
            Some((rgba, w as u32, h as u32))
        }
    })();
    // SAFETY: releasing the screen DC obtained above, exactly once.
    unsafe {
        ReleaseDC(None, dc);
    }
    result
}

struct RawBitmap {
    w: i32,
    h: i32,
    /// 32bpp top-down BGRA rows, `w*h*4` (monochrome sources arrive expanded: 0x00/0xFF channels).
    bgra: Vec<u8>,
}

/// 32bpp top-down via `GetDIBits` (1bpp→32bpp expansion for the mask planes).
fn read_bitmap_32(dc: HDC, hbm: HBITMAP) -> Option<RawBitmap> {
    let mut bm = BITMAP::default();
    // SAFETY: `bm` is a live out-param sized exactly as passed; GetObjectW only writes into it.
    let n = unsafe {
        GetObjectW(
            hbm.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&mut bm as *mut BITMAP).cast()),
        )
    };
    if n == 0 || bm.bmWidth <= 0 || bm.bmHeight <= 0 || bm.bmWidth > 512 || bm.bmHeight > 1024 {
        return None; // 512/1024: sanity caps (256² is the wire max; XL accessibility ≤ that)
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight);
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    // SAFETY: `buf` spans exactly `h` rows of `w` 32bpp pixels as described by `info`; both are
    // live locals for this synchronous call, `hbm` is a live bitmap not selected into any DC
    // (fresh GetIconInfo copies).
    let rows = unsafe {
        GetDIBits(
            dc,
            hbm,
            0,
            h as u32,
            Some(buf.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    (rows != 0).then_some(RawBitmap { w, h, bgra: buf })
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut out = bgra.to_vec();
    for px in out.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    out
}
