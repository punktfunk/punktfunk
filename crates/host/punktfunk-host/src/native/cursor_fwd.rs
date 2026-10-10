//! Host cursor-forward channel (`design/remote-desktop-sweep.md`).
//!
//! Armed when `CLIENT_CAP_CURSOR` met `HOST_CAP_CURSOR`. The encoder then
//! stops blending (`SessionPlan::cursor_blend = false`) and this loop forwards
//! the pointer out-of-band: `CursorShape` (bitmap + hotspot) on serial change
//! via the control-task bridge (reliable, depth-1 latest-wins); `CursorState` (hotspot position /
//! visibility, 14 B) as a `0xD0` datagram every encode tick (lossy, latest-wins;
//! no refresh timer).
//!
//! Pin: `shape_from_overlay` tests. Flag contract is on [`CursorForwarder::tick`].

use punktfunk_core::quic::{
    encode_cursor_state_datagram, CursorShape, CursorState, CURSOR_RELATIVE_HINT,
    CURSOR_SHAPE_MAX_SIDE, CURSOR_VISIBLE,
};

/// Owned by the encode loop — the thread that binds frames.
pub(super) struct CursorForwarder {
    sent_serial: Option<u64>,
    /// The reframe the sent shape was scaled for; a new one re-sends it.
    sent_reframe: Option<punktfunk_core::video_fit::Reframe>,
    /// Hotspot in encoded-frame px. Survives hide so a hidden tick still names
    /// the last visible point.
    last_pos: (i32, i32),
}

impl CursorForwarder {
    pub(super) fn new() -> CursorForwarder {
        CursorForwarder {
            sent_serial: None,
            sent_reframe: None,
            last_pos: (0, 0),
        }
    }

    /// Send `0xD0` every encode tick (lossy, latest-wins; no refresh timer).
    /// Visible → `CURSOR_VISIBLE`; hidden-but-known (app grabbed the pointer)
    /// → `CURSOR_RELATIVE_HINT`; `None` (no overlay ever) → 0. Do not hint
    /// off a cold start — only off an observed hide. The overlay is in capture pixels;
    /// `reframe` moves it, and scales its bitmap, into the picture the client decodes.
    pub(super) fn tick(
        &mut self,
        cursor: Option<&pf_frame::CursorOverlay>,
        reframe: &punktfunk_core::video_fit::Reframe,
        conn: &super::link::SessionLink,
        shape_tx: &tokio::sync::watch::Sender<Option<CursorShape>>,
    ) {
        let flags = match cursor {
            Some(ov) if ov.visible => {
                let reframed = self.sent_reframe != Some(*reframe);
                if self.sent_serial != Some(ov.serial) || reframed {
                    if let Some(shape) = shape_from_overlay(ov, reframe_scale(reframe)) {
                        // Depth-1 slot: an undrained older bitmap is replaced, never queued.
                        // Send fail ⇒ receiver dropped; session is tearing down.
                        let _ = shape_tx.send(Some(shape));
                        self.sent_serial = Some(ov.serial);
                        self.sent_reframe = Some(*reframe);
                    }
                }
                self.last_pos = reframe.to_frame(ov.x + ov.hot_x as i32, ov.y + ov.hot_y as i32);
                CURSOR_VISIBLE
            }
            Some(_) => CURSOR_RELATIVE_HINT,
            None => 0,
        };
        let state = CursorState {
            serial: self.sent_serial.unwrap_or(0) as u32,
            flags,
            x: self.last_pos.0,
            y: self.last_pos.1,
        };
        conn.send_datagram(encode_cursor_state_datagram(&state));
    }
}

/// How much smaller the client's picture is than the capture: a joiner's frame or a
/// mirror downscale. A reframe never grows, so this is at most 1.
fn reframe_scale(r: &punktfunk_core::video_fit::Reframe) -> f64 {
    let (cw, ch) = (r.crop[2], r.crop[3]);
    if cw == 0 || ch == 0 {
        return 1.0;
    }
    (f64::from(r.out.0) / f64::from(cw))
        .min(f64::from(r.out.1) / f64::from(ch))
        .min(1.0)
}

/// Nearest-neighbour resample to the client picture's `scale`, and down to
/// [`CURSOR_SHAPE_MAX_SIDE`]. Control frames are u16-length; the cap is a backstop for
/// oversized accessibility cursors, not a quality path.
fn shape_from_overlay(ov: &pf_frame::CursorOverlay, scale: f64) -> Option<CursorShape> {
    let px = (ov.w as usize).checked_mul(ov.h as usize)?.checked_mul(4)?;
    if ov.w == 0 || ov.h == 0 || ov.rgba.len() < px {
        return None;
    }
    let cap = f64::from(CURSOR_SHAPE_MAX_SIDE) / f64::from(ov.w.max(ov.h));
    let s = scale.min(cap).min(1.0);
    let side = |n: u32| ((f64::from(n) * s).round() as u32).max(1);
    let (w, h) = (side(ov.w), side(ov.h));
    let rgba = if (w, h) == (ov.w, ov.h) {
        ov.rgba.as_ref().clone()
    } else {
        let at = |i: u32, n: u32| (((f64::from(i) + 0.5) / s) as u32).min(n - 1);
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let o = ((at(y, ov.h) * ov.w + at(x, ov.w)) * 4) as usize;
                out.extend_from_slice(&ov.rgba[o..o + 4]);
            }
        }
        out
    };
    let hot = |v: u32, n: u32| ((f64::from(v) * s) as u32).min(n - 1) as u16;
    Some(CursorShape {
        serial: ov.serial as u32,
        w: w as u16,
        h: h as u16,
        hot_x: hot(ov.hot_x, w),
        hot_y: hot(ov.hot_y, h),
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::v2::msg::{decode, V2Message};
    use std::sync::Arc;

    fn overlay(w: u32, h: u32, hot: (u32, u32)) -> pf_frame::CursorOverlay {
        pf_frame::CursorOverlay {
            x: 10,
            y: 20,
            w,
            h,
            rgba: Arc::new((0..w * h * 4).map(|i| i as u8).collect()),
            serial: 3,
            hot_x: hot.0,
            hot_y: hot.1,
            visible: true,
        }
    }

    #[test]
    fn small_shape_passes_through() {
        let s = shape_from_overlay(&overlay(32, 32, (4, 5)), 1.0).unwrap();
        assert_eq!((s.w, s.h, s.hot_x, s.hot_y, s.serial), (32, 32, 4, 5, 3));
        assert_eq!(s.rgba.len(), 32 * 32 * 4);
        assert!(s.encode_v2().len() <= u16::MAX as usize);
    }

    #[test]
    fn oversize_shape_downscales_with_hotspot() {
        // 256 > 120 → f = ceil(256/120) = 3; hotspot scales with the same f.
        let s = shape_from_overlay(&overlay(256, 256, (255, 0)), 1.0).unwrap();
        assert!(s.w <= CURSOR_SHAPE_MAX_SIDE && s.h <= CURSOR_SHAPE_MAX_SIDE);
        assert_eq!(s.rgba.len(), s.w as usize * s.h as usize * 4);
        assert!(s.hot_x < s.w && s.hot_y < s.h);
        assert!(s.encode_v2().len() <= u16::MAX as usize);
        let body = s.fields().into_body();
        assert_eq!(decode::<CursorShape>(CursorShape::TYPE, &body).unwrap(), s);
    }

    #[test]
    fn a_reframed_client_gets_a_pointer_at_its_picture_scale() {
        // A 1920-wide client framing a 3840 host: half size, hotspot with it.
        let r = punktfunk_core::video_fit::Reframe {
            source: (3840, 2160),
            crop: [0, 0, 3840, 2160],
            out: (1920, 1080),
        };
        let s = shape_from_overlay(&overlay(32, 32, (10, 6)), reframe_scale(&r)).unwrap();
        assert_eq!((s.w, s.h, s.hot_x, s.hot_y), (16, 16, 5, 3));
        assert_eq!(s.rgba.len(), 16 * 16 * 4);
        let full = punktfunk_core::video_fit::Reframe::full((3840, 2160));
        assert_eq!(reframe_scale(&full), 1.0);
    }

    #[test]
    fn short_buffer_rejected() {
        let mut ov = overlay(8, 8, (0, 0));
        ov.rgba = Arc::new(vec![0; 8]);
        assert!(shape_from_overlay(&ov, 1.0).is_none());
    }
}
