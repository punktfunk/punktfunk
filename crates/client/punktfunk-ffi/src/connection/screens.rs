//! A session's further screens: the host's layout, the caller's ask, and each further screen's
//! access units, pulled as screen 0's are.

#[cfg(feature = "quic")]
use crate::*;
#[cfg(feature = "quic")]
use punktfunk_core::quic::v2::msg::{Screen, SCREENS_MAX};

/// Most screens a session shows, screen 0 included.
#[cfg(feature = "quic")]
pub const PUNKTFUNK_SCREENS_MAX: usize = 4;

/// One screen of a session: a client monitor in an ask, a screen the host built in a layout.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PunktfunkScreen {
    /// 0 is the screen the connect's mode describes.
    pub index: u8,
    /// The video stream it rides; `0` in an ask.
    pub stream: u8,
    /// `1`: the client's primary monitor.
    pub primary: u8,
    /// Always `0`.
    pub reserved: u8,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// In an ask, this monitor's top-left from screen 0's, in pixels of screen 0's mode. In a
    /// layout, the screen's origin on the host desktop.
    pub x: i32,
    pub y: i32,
}

/// The size an asked screen is shown at when smaller than its mode: the host scales that screen's
/// picture to it before encoding. `0` on either axis names none. Read beside the screens, the one
/// at `i` for `screens[i]`; screen 0's view is the connect's or the mode ask's.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PunktfunkScreenView {
    pub width: u32,
    pub height: u32,
}

#[cfg(feature = "quic")]
const _: () = {
    assert!(std::mem::size_of::<PunktfunkScreen>() == 24);
    assert!(std::mem::size_of::<PunktfunkScreenView>() == 8);
    assert!(PUNKTFUNK_SCREENS_MAX == SCREENS_MAX);
};

#[cfg(feature = "quic")]
impl PunktfunkScreen {
    fn from_core(s: &Screen) -> Self {
        PunktfunkScreen {
            index: s.index,
            stream: s.stream,
            primary: u8::from(s.primary),
            reserved: 0,
            width: s.mode.width,
            height: s.mode.height,
            refresh_hz: s.mode.refresh_hz,
            x: s.x,
            y: s.y,
        }
    }

    fn to_core(self) -> Screen {
        Screen {
            index: self.index,
            stream: self.stream,
            mode: punktfunk_core::config::Mode {
                width: self.width,
                height: self.height,
                refresh_hz: self.refresh_hz,
            },
            x: self.x,
            y: self.y,
            primary: self.primary != 0,
            view: None,
        }
    }
}

/// The `n` records at `screens` as core screens, each with the view at its place in `views`;
/// none for null `screens`, no views for null `views`.
///
/// # Safety
/// `screens` and `views` are each null or readable for `n` records.
#[cfg(feature = "quic")]
pub(super) unsafe fn screens_at(
    screens: *const PunktfunkScreen,
    views: *const PunktfunkScreenView,
    n: usize,
) -> Vec<Screen> {
    if screens.is_null() || n == 0 {
        return Vec::new();
    }
    // SAFETY: non-null and readable for `n` records (caller contract); copied out here.
    let records = unsafe { std::slice::from_raw_parts(screens, n) };
    let views = if views.is_null() {
        &[][..]
    } else {
        // SAFETY: as `screens`.
        unsafe { std::slice::from_raw_parts(views, n) }
    };
    records
        .iter()
        .enumerate()
        .map(|(i, s)| Screen {
            view: views
                .get(i)
                .filter(|v| v.width > 0 && v.height > 0)
                .map(|v| (v.width, v.height)),
            ..s.to_core()
        })
        .collect()
}

/// The screens the host built, screen 0 first, and a generation that moves with each change
/// (`0` before the host said). Writes up to `cap` records into `out` and the whole count into
/// `count`. A screen the host could not build is absent: close its window. Poll the generation.
///
/// # Safety
/// `c` is a valid connection handle; `out` is null or writable for `cap` records; `count` and
/// `generation` are null or writable.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_screen_layout(
    c: *const PunktfunkConnection,
    out: *mut PunktfunkScreen,
    cap: usize,
    count: *mut usize,
    generation: *mut u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        let (layout_gen, layout) = c.inner.screen_layout();
        if !out.is_null() {
            for (i, s) in layout.iter().take(cap).enumerate() {
                // SAFETY: `out` is writable for `cap` records (caller contract) and `i < cap`.
                unsafe { put(out.add(i), PunktfunkScreen::from_core(s)) };
            }
        }
        // SAFETY: the caller passes each out-param null or writable for one value.
        unsafe {
            put(count, layout.len());
            put(generation, layout_gen);
        }
        PunktfunkStatus::Ok
    })
}

/// Show these `n` screens from now on, screen 0 first, at most [`PUNKTFUNK_SCREENS_MAX`]: the
/// host adds, drops and resizes the others and answers with a new layout. Screen 0's size still
/// moves by [`punktfunk_connection_request_mode`]. A host without screens ignores it.
///
/// # Safety
/// `c` is a valid connection handle; `screens` is null or readable for `n` records.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_request_screens(
    c: *const PunktfunkConnection,
    screens: *const PunktfunkScreen,
    n: usize,
) -> PunktfunkStatus {
    // SAFETY: the caller's contract, with no views.
    unsafe { punktfunk_connection_request_screens_view(c, screens, std::ptr::null(), n) }
}

/// [`punktfunk_connection_request_screens`] with each screen's view, `views[i]` for
/// `screens[i]`: a further screen shown smaller than its mode is scaled to its view before
/// encoding. Null `views` names none.
///
/// # Safety
/// `c` is a valid connection handle; `screens` and `views` are each null or readable for `n`
/// records.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_request_screens_view(
    c: *const PunktfunkConnection,
    screens: *const PunktfunkScreen,
    views: *const PunktfunkScreenView,
    n: usize,
) -> PunktfunkStatus {
    with_conn!(c => {
        if n > PUNKTFUNK_SCREENS_MAX {
            return PunktfunkStatus::InvalidArg;
        }
        if screens.is_null() && n > 0 {
            return PunktfunkStatus::NullPointer;
        }
        // SAFETY: `screens` and `views` are null or readable for `n` records (caller contract).
        let list = unsafe { screens_at(screens, views, n) };
        status_of(c.inner.request_screens(list))
    })
}

/// Screen `screen`'s decoder cannot go on, or is new: its access units wait for a keyframe,
/// which core asks the host for, and that one carries `FRAME_NEW_FORMAT`.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_request_screen_keyframe(
    c: *const PunktfunkConnection,
    screen: u8,
) -> PunktfunkStatus {
    with_conn!(c => {
        c.inner.request_screen_keyframe(screen);
        PunktfunkStatus::Ok
    })
}

/// Next whole access unit of screen `screen` (1 or more), as [`punktfunk_connection_next_au`]
/// hands screen 0's: `FRAME_NEW_FORMAT` on the first of each format, and `*out` borrowed until
/// this screen's next pull. `NoFrame` on timeout and before the host opened the screen;
/// `Closed` once the session ended or the host dropped it; `InvalidArg` for screen 0.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one thread pulls each screen.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_screen_au(
    c: *mut PunktfunkConnection,
    screen: u8,
    out: *mut PunktfunkFrame,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let Some(slot) = usize::from(screen)
            .checked_sub(1)
            .filter(|i| *i < PUNKTFUNK_SCREENS_MAX - 1)
        else {
            return PunktfunkStatus::InvalidArg;
        };
        let timeout = std::time::Duration::from_millis(u64::from(timeout_ms));
        match c.inner.next_screen_frame(screen, timeout) {
            Ok(frame) => {
                let mut slots = lock_recover(&c.last_screens);
                let f = slots[slot].insert(frame);
                // SAFETY: `out` is a caller-owned, writable `#[repr(C)]` slot.
                unsafe { put(out, super::planes::frame_view(f)) };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}
