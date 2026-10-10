//! Client-side OS cursor: the host forwards SHAPE (reliable, cached by serial)
//! and per-frame STATE (lossy `0xD0` — position/visibility). We draw it locally
//! so pointer feel does not pay the video round-trip. Active only when
//! `HOST_CAP_CURSOR` was in Welcome (host stopped compositing) and the DESKTOP
//! mouse model is engaged — under capture the pointer is relative-locked and
//! games draw their own cursor in-frame.
//!
//! Host bitmaps are host-framebuffer pixels (they track host DPI). Scale by
//! the video aspect-fit factor times the client display's content scale
//! (`cursor_scale` in the run loop). SDL cursors are fixed-size from their
//! surface, so shapes are cached RAW and resampled per install — rebuild when
//! serial or scale changes.

use punktfunk_core::client::NativeClient;
use punktfunk_core::quic::{CursorState, HOST_CAP_CURSOR};
use sdl3::mouse::{Cursor, MouseUtil, SystemCursor};
use sdl3::pixels::PixelFormat;
use sdl3::surface::Surface;
use std::collections::HashMap;
use std::time::Duration;

/// Cap. Cursors cycle a handful of shapes; 64 stops a runaway host. Reset
/// re-installs via the serial-miss path on the reliable stream.
const SHAPE_CACHE_MAX: usize = 64;

/// Host-framebuffer bytes + hotspot, held raw so a fit-scale change can rebuild
/// the OS cursor. Caching a fixed-size `Cursor` would freeze it at build-time size.
struct RawShape {
    rgba: Vec<u8>,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
}

pub struct CursorChannel {
    /// Welcome carried `HOST_CAP_CURSOR`: host forwards instead of compositing.
    negotiated: bool,
    shapes: HashMap<u32, RawShape>,
    /// Serial and fit-scale the installed OS cursor was built at. `None` is the
    /// system default. A change in either forces a rebuild.
    installed: Option<(u32, f32)>,
    /// Installed `Cursor` must outlive `set()` (SDL).
    installed_cursor: Option<Cursor>,
    /// Latest `0xD0` (latest-wins across a drained batch).
    state: Option<CursorState>,
}

impl CursorChannel {
    pub fn new(connector: &NativeClient) -> CursorChannel {
        let negotiated = connector.host_caps() & HOST_CAP_CURSOR != 0;
        if negotiated {
            tracing::info!("cursor channel negotiated — host cursor renders locally");
        }
        CursorChannel {
            negotiated,
            shapes: HashMap::new(),
            installed: None,
            installed_cursor: None,
            state: None,
        }
    }

    pub fn negotiated(&self) -> bool {
        self.negotiated
    }

    /// The run loop reads `relative_hint` for the host-driven mode flip and
    /// `x`/`y` as the reappear position when leaving relative.
    pub fn state(&self) -> Option<CursorState> {
        self.state
    }

    /// Own shape/visibility only while `desktop_active`; under capture SDL relative
    /// mode owns the cursor, and released it must look like the system default.
    /// `fit_scale` is host-framebuffer pixels → cursor-surface pixels.
    pub fn pump(
        &mut self,
        connector: &NativeClient,
        mouse: &MouseUtil,
        desktop_active: bool,
        fit_scale: f32,
    ) {
        if !self.negotiated {
            return;
        }
        while let Ok(shape) = connector.next_cursor_shape(Duration::ZERO) {
            if self.shapes.len() >= SHAPE_CACHE_MAX {
                // Runaway host: reset; live shapes re-install via the serial-miss path.
                self.shapes.clear();
                self.installed = None;
            }
            let (w, h) = (shape.w as u32, shape.h as u32);
            if w == 0 || h == 0 || shape.rgba.len() < (w * h * 4) as usize {
                tracing::warn!(w, h, "cursor shape malformed — ignored");
                continue;
            }
            // Re-sent serial replaces the entry; force re-install if it is current.
            if matches!(self.installed, Some((s, _)) if s == shape.serial) {
                self.installed = None;
            }
            self.shapes.insert(
                shape.serial,
                RawShape {
                    rgba: shape.rgba,
                    w,
                    h,
                    hot_x: shape.hot_x as u32,
                    hot_y: shape.hot_y as u32,
                },
            );
        }
        while let Ok(st) = connector.next_cursor_state(Duration::ZERO) {
            self.state = Some(st); // latest wins
        }

        if !desktop_active {
            // Capture or released: restore the system default so the pointer over
            // the window is not the host's shape.
            if self.installed.take().is_some() {
                if let Ok(c) = Cursor::from_system(SystemCursor::Arrow) {
                    c.set();
                    self.installed_cursor = Some(c); // keep it alive past set()
                }
            }
            return;
        }
        let Some(st) = self.state else { return };
        if st.visible() && self.installed != Some((st.serial, fit_scale)) {
            if let Some(shape) = self.shapes.get(&st.serial) {
                match build_scaled_cursor(shape, fit_scale) {
                    Ok(cursor) => {
                        cursor.set();
                        self.installed = Some((st.serial, fit_scale));
                        self.installed_cursor = Some(cursor); // outlive set()
                    }
                    Err(e) => tracing::warn!(error = %e, w = shape.w, h = shape.h,
                        "cursor shape rejected by SDL — keeping the previous cursor"),
                }
            }
            // Serial miss: keep the previous cursor for one RTT rather than flashing default.
        }
        // Query, do not shadow: apply_capture's own show/hide must not desync us.
        if mouse.is_cursor_showing() != st.visible() {
            mouse.show_cursor(st.visible());
        }
    }
}

/// Resample by `fit_scale` into an SDL color cursor. Hotspot scales with the
/// bitmap. `fit_scale <= 0` (or a degenerate result) is clamped to a ≥1×1 surface.
fn build_scaled_cursor(shape: &RawShape, fit_scale: f32) -> Result<Cursor, String> {
    let scale = if fit_scale.is_finite() && fit_scale > 0.0 {
        fit_scale
    } else {
        1.0
    };
    let dw = ((shape.w as f32 * scale).round() as u32).max(1);
    let dh = ((shape.h as f32 * scale).round() as u32).max(1);
    let hot_x = ((shape.hot_x as f32 * scale).round() as u32).min(dw - 1) as i32;
    let hot_y = ((shape.hot_y as f32 * scale).round() as u32).min(dh - 1) as i32;

    if dw == shape.w && dh == shape.h {
        let mut data = shape.rgba.clone();
        let surf = Surface::from_data(
            &mut data,
            shape.w,
            shape.h,
            shape.w * 4,
            PixelFormat::RGBA32,
        )
        .map_err(|e| e.to_string())?;
        return Cursor::from_surface(&surf, hot_x, hot_y).map_err(|e| e.to_string());
    }

    let mut scaled = resample_rgba(&shape.rgba, shape.w, shape.h, dw, dh);
    let surf = Surface::from_data(&mut scaled, dw, dh, dw * 4, PixelFormat::RGBA32)
        .map_err(|e| e.to_string())?;
    Cursor::from_surface(&surf, hot_x, hot_y).map_err(|e| e.to_string())
}

/// Area-average `(sw×sh) → (dw×dh)` on straight-alpha RGBA. Average in
/// premultiplied colour so transparent-pixel colour cannot bleed into the
/// fringe, then un-premultiply back.
fn resample_rgba(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut out = vec![0u8; (dw * dh * 4) as usize];
    let fx = sw as f32 / dw as f32;
    let fy = sh as f32 / dh as f32;
    for dy in 0..dh {
        let sy0 = (dy as f32 * fy).floor() as u32;
        let sy1 = (((dy + 1) as f32 * fy).ceil() as u32).clamp(sy0 + 1, sh);
        for dx in 0..dw {
            let sx0 = (dx as f32 * fx).floor() as u32;
            let sx1 = (((dx + 1) as f32 * fx).ceil() as u32).clamp(sx0 + 1, sw);
            let (mut r, mut g, mut b, mut a_sum, mut n) = (0f32, 0f32, 0f32, 0f32, 0f32);
            for sy in sy0..sy1 {
                for sx in sx0..sx1 {
                    let i = ((sy * sw + sx) * 4) as usize;
                    let a = src[i + 3] as f32 / 255.0;
                    r += src[i] as f32 * a;
                    g += src[i + 1] as f32 * a;
                    b += src[i + 2] as f32 * a;
                    a_sum += a;
                    n += 1.0;
                }
            }
            let di = ((dy * dw + dx) * 4) as usize;
            if a_sum > 0.0 {
                out[di] = (r / a_sum).round().clamp(0.0, 255.0) as u8;
                out[di + 1] = (g / a_sum).round().clamp(0.0, 255.0) as u8;
                out[di + 2] = (b / a_sum).round().clamp(0.0, 255.0) as u8;
                out[di + 3] = (a_sum / n * 255.0).round().clamp(0.0, 255.0) as u8;
            }
            // else fully transparent — already zero-filled.
        }
    }
    out
}
