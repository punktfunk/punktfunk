//! Absolute-input mapping onto the streamed display
//! (`design/pen-tablet-input.md`).
//!
//! Pen, touch, and absolute-mouse samples arrive normalized to the streamed
//! output's frame. Mapping over the whole virtual desktop is only correct when
//! that output is the sole display (Exclusive, origin 0). In Extend the
//! streamed output has a non-zero origin; a stylus has no cursor-style
//! closed-loop correction, so a miss-scale is visible.
//!
//! The host publishes the CCD target id at capture bring-up
//! ([`set_stream_target`]). Sites resolve the live rect from the display actor's
//! snapshot (`pf_win_display::display_events::snapshot`, the same source as
//! cursor-readback) — no display-config query on the input path — TTL-cached so
//! a layout rearrange is seen mid-session without re-reading per sample. No
//! target / unresolved: whole virtual desktop.

use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// `(x, y, w, h)` in desktop pixels (`source_desktop_rect` order).
type Rect = (i32, i32, i32, i32);

/// 250 ms ≈ six 40 ms refresh ticks. Long enough the CCD query vanishes at
/// input rate; short enough a layout move is seen mid-session.
const RECT_TTL: Duration = Duration::from_millis(250);

struct State {
    target: Option<pf_win_display::win_display::CcdTargetKey>,
    rect: Option<Rect>,
    queried: Option<Instant>,
}

const NO_TARGET: State = State {
    target: None,
    rect: None,
    queried: None,
};

/// One slot per screen of the session, the first at 0 ([`crate::EXTRA_SCREENS`] more).
static STATE: Mutex<[State; 1 + crate::EXTRA_SCREENS]> =
    Mutex::new([NO_TARGET, NO_TARGET, NO_TARGET, NO_TARGET]);

/// Publish the streamed output (its CCD target id) that absolute input maps into, at capture
/// bring-up, then bump the aim generation: whoever sees the new generation finds this target.
/// Never cleared at teardown: a deactivated target stops resolving, the last-known rect stays,
/// and nothing injects between sessions. One slot per process: with parallel sessions the last
/// bring-up wins for every session's absolute input; per-session routing needs source-tagged
/// input events (parallel-displays plan).
pub fn set_stream_target(target: Option<pf_win_display::win_display::CcdTargetKey>) {
    set_slot(0, target);
    crate::publish_aim(true, |_| {});
}

/// [`set_stream_target`] for further screen `screen` (1 or more), with no warp: the pointer
/// stays where the first screen put it.
pub fn set_screen_target(screen: u8, target: Option<pf_win_display::win_display::CcdTargetKey>) {
    if screen > 0 {
        set_slot(usize::from(screen), target);
    }
}

fn set_slot(i: usize, target: Option<pf_win_display::win_display::CcdTargetKey>) {
    let mut slots = STATE.lock().unwrap();
    let Some(st) = slots.get_mut(i) else {
        return;
    };
    if st.target != target {
        tracing::info!(screen = i, ?target, "absolute-input stream target set");
        st.target = target;
        st.rect = None;
        st.queried = None;
    }
}

/// Cached rect of screen `screen`'s output. `None` = no target / never resolved (callers fall
/// back to the first screen's, then the whole virtual desktop).
fn stream_rect(screen: u8) -> Option<Rect> {
    let mut slots = STATE.lock().unwrap();
    let st = slots.get_mut(usize::from(screen))?;
    let target = st.target?;
    let fresh = st.queried.is_some_and(|at| at.elapsed() < RECT_TTL);
    if !fresh {
        st.queried = Some(Instant::now());
        match pf_win_display::display_events::snapshot().source_rect(target) {
            Some(r) => {
                if st.rect != Some(r) {
                    tracing::info!(target = %target, rect = ?r, "stream-target desktop rect resolved");
                }
                st.rect = Some(r);
            }
            // Teardown or a topology commit in flight: keep the last rect.
            // Snapping mid-stroke to the whole-desktop mapping would jump.
            None => {
                if st.rect.is_some() {
                    tracing::debug!(
                        target = %target,
                        "stream target not an active path — keeping last rect"
                    );
                }
            }
        }
    }
    st.rect
}

/// `[0,1]²` of screen `screen`'s picture onto its desktop rect.
pub(crate) fn map_normalized(screen: u8, nx: f64, ny: f64) -> (i32, i32) {
    let rect = (screen > 0)
        .then(|| stream_rect(screen))
        .flatten()
        .or_else(|| stream_rect(0))
        .unwrap_or_else(virtual_desktop_rect);
    map_into(rect, nx, ny)
}

/// `[0,1]²` over `(x, y, w, h)`. Inclusive edges: 1.0 lands on the last pixel.
fn map_into((x, y, w, h): Rect, nx: f64, ny: f64) -> (i32, i32) {
    (
        x + (nx.clamp(0.0, 1.0) * (w - 1).max(0) as f64).round() as i32,
        y + (ny.clamp(0.0, 1.0) * (h - 1).max(0) as f64).round() as i32,
    )
}

/// Virtual-desktop bounds — mapping fallback, and the surface
/// `MOUSEEVENTF_VIRTUALDESK` absolute coordinates normalize over.
pub(crate) fn virtual_desktop_rect() -> Rect {
    // SAFETY: each `GetSystemMetrics` takes a single by-value `SYSTEM_METRICS_INDEX` constant and
    // returns an `i32`; it dereferences no pointer and has no side effects — FFI-`unsafe` only.
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1),
        )
    }
}

/// Desktop pixel as `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK` 0..65535.
pub(crate) fn desktop_px_to_virtualdesk(px: (i32, i32)) -> (i32, i32) {
    px_to_abs(virtual_desktop_rect(), px)
}

/// Desktop pixel counted from the virtual screen's top-left, the space
/// `InjectSyntheticPointerInput` reads `ptPixelLocation` in.
pub(crate) fn desktop_px_to_virtual_screen(px: (i32, i32)) -> (i32, i32) {
    from_origin(virtual_desktop_rect(), px)
}

fn from_origin((vx, vy, _, _): Rect, (px, py): (i32, i32)) -> (i32, i32) {
    (px - vx, py - vy)
}

/// SendInput absolute coordinates span 0..65535 over the chosen surface, and win32k maps them
/// back as `px = ax * vw / 65536` (floor). The ceiling of `px * 65536 / vw` is the smallest
/// value that lands on `px`, for every column.
fn px_to_abs((vx, vy, vw, vh): Rect, (px, py): (i32, i32)) -> (i32, i32) {
    let axis = |p: i32, n: i32| {
        let n = i64::from(n.max(1));
        ((i64::from(p) * 65536 + n - 1) / n).clamp(0, 65535) as i32
    };
    (axis(px - vx, vw), axis(py - vy, vh))
}

/// Desktop pixel as the absolute HID collection's `0..=32767`, or `None` off the primary
/// monitor: mouhid maps an absolute HID pointer onto the primary monitor only.
pub(crate) fn primary_hid_abs(px: (i32, i32)) -> Option<(u16, u16)> {
    // SAFETY: as `virtual_desktop_rect` — by-value metric indices, no pointers.
    let size = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    px_to_hid_abs(size, px)
}

/// Aims at the pixel's centre. win32k lands on `L * w / 32768` (floor), give or take mouhid's
/// rescale to 0..65535; the centre absorbs both up to 16K-wide monitors.
fn px_to_hid_abs((w, h): (i32, i32), (px, py): (i32, i32)) -> Option<(u16, u16)> {
    let axis = |p: i32, n: i32| {
        if !(0..n).contains(&p) {
            return None;
        }
        let (p, n) = (i64::from(p), i64::from(n));
        Some(((2 * p + 1) * 32768 / (2 * n)).min(32767) as u16)
    };
    Some((axis(px, w)?, axis(py, h)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Physical 1920×1080 at (0,0), streamed 2560×1440 at (1920,0) — samples
    /// must land in the virtual output, not at the desktop origin.
    #[test]
    fn maps_over_the_streamed_rect_not_the_desktop() {
        let r = (1920, 0, 2560, 1440);
        assert_eq!(map_into(r, 0.0, 0.0), (1920, 0));
        assert_eq!(map_into(r, 1.0, 1.0), (1920 + 2559, 1439));
        assert_eq!(map_into(r, 0.5, 0.5), (1920 + 1280, 720));
    }

    #[test]
    fn clamps_out_of_range_and_handles_negative_origins() {
        // Output left of / above the primary has a negative desktop origin.
        let r = (-2560, -100, 2560, 1440);
        assert_eq!(map_into(r, 0.0, 0.0), (-2560, -100));
        assert_eq!(map_into(r, 2.0, -1.0), (-2560 + 2559, -100));
    }

    /// A monitor left of and above the primary moves the virtual screen's origin negative;
    /// a touch on the primary's top-left pixel lands that far into the virtual screen.
    #[test]
    fn pointer_pixels_count_from_the_virtual_screen() {
        let v = (-2560, -100, 4480, 1540);
        assert_eq!(from_origin(v, (0, 0)), (2560, 100));
        assert_eq!(from_origin(v, (-2560, -100)), (0, 0));
        assert_eq!(from_origin((0, 0, 1920, 1080), (1919, 1079)), (1919, 1079));
    }

    #[test]
    fn degenerate_rect_pins_to_its_origin() {
        assert_eq!(map_into((10, 20, 0, 0), 0.7, 0.7), (10, 20));
    }

    /// win32k maps absolute back as `px = ax * vw / 65536` (floor). Edge
    /// pixels and a streamed origin must survive that.
    #[test]
    fn virtualdesk_normalization_round_trips() {
        // Every column and row, not just the edges: rounding once lost ~40 columns near the
        // origin of a 4480-wide desktop.
        for (w, h) in [(4480, 1080), (1920, 1080), (7, 3)] {
            let v = (0, 0, w, h);
            for px in 0..w {
                let (ax, _) = px_to_abs(v, (px, 0));
                assert!((0..=65535).contains(&ax));
                assert_eq!(
                    (i64::from(ax) * i64::from(w) / 65536) as i32,
                    px,
                    "{w}: {px}"
                );
            }
            let (_, ay) = px_to_abs(v, (0, h - 1));
            assert_eq!((i64::from(ay) * i64::from(h) / 65536) as i32, h - 1);
        }
        // The absolute HID collection lands on every pixel of the primary monitor, under
        // win32k's floor and under mouhid's rescale to 0..65535 first.
        for w in [7, 1366, 1920, 3440, 3840, 7680, 15360] {
            for px in 0..w {
                let (l, _) = px_to_hid_abs((w, 1), (px, 0)).unwrap();
                let (l, w64) = (i64::from(l), i64::from(w));
                assert_eq!(l * w64 / 32768, i64::from(px), "{w}: {px}");
                assert_eq!(l * 65535 / 32767 * w64 / 65536, i64::from(px), "{w}: {px}");
            }
        }
        assert_eq!(px_to_hid_abs((1920, 1080), (1920, 0)), None);
        assert_eq!(px_to_hid_abs((1920, 1080), (-1, 0)), None);
        // Negative-origin desktops still normalize from 0.
        let v = (-2560, 0, 4480, 1440);
        assert_eq!(px_to_abs(v, (-2560, 0)), (0, 0));
    }
}
