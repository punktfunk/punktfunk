//! The pointer apart from the frames: its image from AppKit's system cursor and the display's
//! bounds, read on a thread of their own, and its place from a Core Graphics event on every
//! call. Both map into the frame the way the stream draws the display. While the client draws
//! it, a second thread reads the place at the stream rate and rings on each move, so a move
//! over a still desktop, which brings no frame, still reaches the client.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use objc2_app_kit::NSCursor;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    kCGColorSpaceSRGB, CGBitmapContextCreate, CGColorSpace, CGContext, CGDisplayBounds, CGEvent,
    CGImageAlphaInfo,
};
use pf_frame::CursorOverlay;

use super::plan::{self, Rect};

/// How often the image and the bounds are read again. Each is a window-server round trip that
/// takes past 4 ms at times on macOS 27.0, so neither runs on the encode loop.
const SHAPE_EVERY: Duration = Duration::from_millis(33);

/// [`crate::CursorSource`] for a display whose frames leave the pointer out.
pub(super) struct Pointer {
    latest: Arc<Mutex<Latest>>,
    /// The client draws the pointer: the place reader runs.
    forwarding: Arc<AtomicBool>,
    /// Dropping them ends the image and place readers.
    _stop: [mpsc::Sender<()>; 2],
}

#[derive(Clone)]
struct Latest {
    shape: Shape,
    serial: u64,
    /// The display in global points, and where it lands in the frame.
    bounds: Rect,
    content: Rect,
}

#[derive(Clone, PartialEq)]
struct Shape {
    /// Straight-alpha RGBA at frame scale.
    rgba: Arc<Vec<u8>>,
    w: u32,
    h: u32,
    hot_x: u32,
    hot_y: u32,
    /// Fully transparent: the system hides the pointer, while typing for one.
    blank: bool,
}

impl Pointer {
    /// `None` when the system reports no cursor image; the frames must carry the pointer then.
    /// While forwarding, the place is read every `period` and `moved` rings when it changed.
    pub(super) fn open(
        display_id: u32,
        frame: (u32, u32),
        period: Duration,
        moved: Arc<dyn Fn() + Send + Sync>,
    ) -> Option<Pointer> {
        let (bounds, content) = place(display_id, frame);
        let latest = Arc::new(Mutex::new(Latest {
            shape: system_cursor(content[2] / bounds[2])?,
            serial: 1,
            bounds,
            content,
        }));
        // The first event read sets up the window-server connection: milliseconds, once.
        let _ = CGEvent::new(None);
        let forwarding = Arc::new(AtomicBool::new(false));
        let (stop_moves, moves_stopped) = mpsc::channel();
        let on = forwarding.clone();
        let mover = move || {
            let mut last = location();
            loop {
                let wait = if on.load(Ordering::Relaxed) {
                    period
                } else {
                    SHAPE_EVERY
                };
                if moves_stopped.recv_timeout(wait) != Err(RecvTimeoutError::Timeout) {
                    return;
                }
                let at = location();
                if at != last && on.load(Ordering::Relaxed) {
                    moved();
                }
                last = at;
            }
        };
        std::thread::Builder::new()
            .name("punktfunk-sck-pointer".into())
            .spawn(mover)
            .ok()?;
        let (stop, stopped) = mpsc::channel();
        let shared = latest.clone();
        let reader = move || {
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(SHAPE_EVERY) {
                let (bounds, content) = place(display_id, frame);
                let shape = system_cursor(content[2] / bounds[2]);
                let mut latest = shared.lock().unwrap_or_else(|e| e.into_inner());
                (latest.bounds, latest.content) = (bounds, content);
                if let Some(shape) = shape.filter(|s| *s != latest.shape) {
                    latest.shape = shape;
                    latest.serial += 1;
                }
            }
        };
        std::thread::Builder::new()
            .name("punktfunk-sck-cursor".into())
            .spawn(reader)
            .ok()?;
        Some(Pointer {
            latest,
            forwarding,
            _stop: [stop, stop_moves],
        })
    }
}

/// Whether the system reports a cursor image now: the frames can leave the pointer out.
pub(crate) fn readable() -> bool {
    system_cursor(1.0).is_some()
}

/// The pointer in global points.
fn location() -> (f64, f64) {
    let at = CGEvent::location(CGEvent::new(None).as_deref());
    (at.x, at.y)
}

impl crate::CursorSource for Pointer {
    fn overlay(&mut self) -> Option<CursorOverlay> {
        let at = location();
        let latest = self
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Latest {
            shape: s,
            serial,
            bounds,
            content,
        } = latest;
        let on = plan::to_frame(at, bounds, content);
        let (x, y) = on.unwrap_or_default();
        Some(CursorOverlay {
            x: x.round() as i32 - s.hot_x as i32,
            y: y.round() as i32 - s.hot_y as i32,
            w: s.w,
            h: s.h,
            rgba: s.rgba,
            serial,
            hot_x: s.hot_x,
            hot_y: s.hot_y,
            visible: !s.blank && on.is_some(),
        })
    }

    /// Starts or stops the place reader. Whether the frames carry the pointer is the stream's
    /// configuration, which the capturer switches.
    fn set_forward(&mut self, client_draws: bool) {
        self.forwarding.store(client_draws, Ordering::Relaxed);
    }
}

/// The display's bounds in global points and where they land in the frame. A mode change moves
/// both.
fn place(display_id: u32, frame: (u32, u32)) -> (Rect, Rect) {
    let b = CGDisplayBounds(display_id);
    let bounds = [
        b.origin.x,
        b.origin.y,
        b.size.width.max(1.0),
        b.size.height.max(1.0),
    ];
    (bounds, plan::fit((bounds[2], bounds[3]), frame))
}

/// The system cursor's image at `scale` frame pixels per point. `None` when the system reports
/// no cursor.
fn system_cursor(scale: f64) -> Option<Shape> {
    objc2::rc::autoreleasepool(|_| {
        // The one public read of another process's pointer image. macOS 27 deprecates it and
        // says it will turn nil; `Pointer::open` then leaves the pointer in the frames.
        #[allow(deprecated)]
        let cursor = NSCursor::currentSystemCursor()?;
        raster(&cursor, scale)
    })
}

/// `cursor` at `scale` frame pixels per point, straight alpha, cut to
/// [`pf_frame::CURSOR_OVERLAY_MAX`].
fn raster(cursor: &NSCursor, scale: f64) -> Option<Shape> {
    let image = cursor.image();
    let (size, hot) = (image.size(), cursor.hotSpot());
    // SAFETY: a null rect and no context or hints ask for the best representation.
    let cg =
        unsafe { image.CGImageForProposedRect_context_hints(std::ptr::null_mut(), None, None) }?;
    let px = |v: f64| (v * scale).round().max(1.0) as u32;
    let (w, h) = (px(size.width), px(size.height));
    let mut rgba = vec![0u8; w as usize * h as usize * 4];
    // SAFETY: a static string.
    let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))?;
    // SAFETY: `rgba` holds `h` rows of `w * 4` bytes and outlives the context, which is dropped
    // before `rgba` is read.
    let ctx = unsafe {
        CGBitmapContextCreate(
            rgba.as_mut_ptr().cast(),
            w as usize,
            h as usize,
            8,
            w as usize * 4,
            Some(&space),
            CGImageAlphaInfo::PremultipliedLast.0,
        )
    }?;
    let rect = CGRect::new(CGPoint::ZERO, CGSize::new(f64::from(w), f64::from(h)));
    CGContext::draw_image(Some(&ctx), rect, Some(&cg));
    drop(ctx);
    plan::unpremultiply(&mut rgba);
    let blank = rgba.chunks_exact(4).all(|px| px[3] == 0);
    let (rgba, w, h) = pf_frame::crop_cursor_rgba(rgba, w, h);
    Some(Shape {
        rgba: Arc::new(rgba),
        w,
        h,
        hot_x: (hot.x * scale).round() as u32,
        hot_y: (hot.y * scale).round() as u32,
        blank,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 32-point cursor with half-alpha red in its top-left corner and opaque blue in its
    /// bottom-right, its hotspot at (6, 2).
    fn marked_cursor() -> objc2::rc::Retained<NSCursor> {
        use objc2::AllocAnyThread;
        use objc2_app_kit::NSImage;
        let mut px = vec![0u8; 32 * 32 * 4];
        px[..4].copy_from_slice(&[128, 0, 0, 128]);
        px[32 * 32 * 4 - 4..].copy_from_slice(&[0, 0, 255, 255]);
        // SAFETY: a static string.
        let space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }));
        // SAFETY: `px` holds 32 rows of 128 bytes and outlives the context.
        let ctx = unsafe {
            CGBitmapContextCreate(
                px.as_mut_ptr().cast(),
                32,
                32,
                8,
                128,
                space.as_deref(),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        };
        let cg = objc2_core_graphics::CGBitmapContextCreateImage(ctx.as_deref()).unwrap();
        let image = NSImage::initWithCGImage_size(NSImage::alloc(), &cg, CGSize::new(32.0, 32.0));
        NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &image, CGPoint::new(6.0, 2.0))
    }

    /// Rows run top-down, alpha comes out straight, the hotspot scales, and half the scale is
    /// half the size.
    #[test]
    fn a_cursor_rasters_at_frame_scale() {
        objc2::rc::autoreleasepool(|_| {
            let cursor = marked_cursor();
            let full = raster(&cursor, 1.0).expect("rasters");
            assert_eq!((full.w, full.h, full.hot_x, full.hot_y), (32, 32, 6, 2));
            assert!(!full.blank);
            assert_eq!(full.rgba[..4], [255, 0, 0, 128], "top-left, straight alpha");
            assert_eq!(full.rgba[full.rgba.len() - 4..], [0, 0, 255, 255]);
            assert_eq!(full.rgba[16 * 128 + 64..][..4], [0, 0, 0, 0]);
            let half = raster(&cursor, 0.5).expect("rasters");
            assert_eq!((half.w, half.h, half.hot_x, half.hot_y), (16, 16, 3, 1));
        });
    }
}
