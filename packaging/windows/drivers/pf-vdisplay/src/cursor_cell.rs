//! The cursor as the encode pool blends it: one [`CursorCell`] per monitor, written by the
//! cursor worker on every hardware-cursor publish and read by the pool at every frame.
//!
//! `blend` is the render model: on while the client draws no pointer and a hardware cursor is
//! declared on the adapter, which excludes the pointer from every frame for the WUDFHost's
//! life. The image is frame-relative as IddCx reports it: "screen co-ordinates" of this
//! monitor, negative past its top-left, so the pool never needs to know where it sits.
//!
//! A hardware cursor moves without DWM composing, so while the pool blends, a publish that
//! changes what the client would see is DAMAGE: the cell marks itself dirty and wakes the
//! pool's encode thread, which re-encodes its stash with the pointer where it is now
//! (`encode/drive.rs`). No DDI here.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};

use pf_driver_proto::cursor::{CursorShm, ShapeRgba};

use crate::encode::pool::Pool;

pub use pf_encode_session::targets::CursorImage;

/// What a viewer sees of this pointer: where it is, which shape, whether at all.
fn seen(image: &CursorImage) -> (i32, i32, u32, bool) {
    (image.x, image.y, image.serial, image.visible)
}

/// See the module docs. The worker and the pool hold clones; neither pins the monitor.
#[derive(Default)]
pub struct CursorCell {
    pub image: Mutex<Option<CursorImage>>,
    blend: AtomicBool,
    /// A worker runs under a declared hardware cursor, so the blend can turn on at any moment.
    /// The pool keeps a clean plate of every composed frame while this holds.
    armed: AtomicBool,
    /// The host's SDR-white scale for an FP16 frame, as `f32` bits; 0 until the first publish.
    pub sdr_white_scale: AtomicU32,
    /// A blended pointer changed since the encode thread last consumed it.
    dirty: AtomicBool,
    /// The pool whose encode thread a dirty pointer wakes; `Weak` so a pool's life stays the
    /// monitor's business.
    waker: Mutex<Option<Weak<Pool>>>,
}

impl CursorCell {
    /// Whether the pool blends the pointer into every frame.
    pub fn blends(&self) -> bool {
        self.blend.load(Ordering::Relaxed)
    }

    /// A worker runs under a declared hardware cursor: the pool keeps a clean plate.
    pub fn armed(&self) -> bool {
        self.armed.load(Ordering::Relaxed)
    }

    pub fn set_armed(&self, armed: bool) {
        self.armed.store(armed, Ordering::Relaxed);
    }

    /// Flip the render model. Either edge is damage: on, the stash was encoded without a
    /// pointer and the client just stopped drawing its own; off, the stash still carries the
    /// last blend under the pointer the client now draws. A re-declare that keeps the model
    /// encodes nothing.
    pub fn set_blend(&self, on: bool) {
        if self.blend.swap(on, Ordering::Release) != on {
            self.mark_dirty();
        }
    }

    /// The pointer changed since the encode thread last consumed it — a peek that leaves the
    /// mark, so the drive loop can rate-limit before it takes it.
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// The pool this cell wakes; set once per pool build.
    pub fn set_waker(&self, pool: Weak<Pool>) {
        *crate::registry::lock(&self.waker) = Some(pool);
    }

    /// Consume the dirty mark: `true` once per changed pointer or render-model flip, so the
    /// caller encodes the latest state exactly once however many publishes coalesced into it.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::AcqRel)
    }

    /// Mark the pointer changed and wake the pool's encode thread. Also how a pointer-only
    /// frame that could not be sent asks to be tried again.
    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
        let pool = crate::registry::lock(&self.waker)
            .as_ref()
            .and_then(Weak::upgrade);
        if let Some(pool) = pool {
            pool.wake();
        }
    }

    /// The pointer to blend now: `None` unless blending is on and a visible shape was
    /// published. The scale is the FP16 SDR-white factor (1.0 when the host stamped none).
    pub fn to_blend(&self) -> Option<(CursorImage, f32)> {
        if !self.blends() {
            return None;
        }
        let image = crate::registry::lock(&self.image).clone()?;
        if !image.visible {
            return None;
        }
        let bits = self.sdr_white_scale.load(Ordering::Relaxed);
        let scale = if bits == 0 { 1.0 } else { f32::from_bits(bits) };
        Some((image, scale))
    }

    /// Fold one worker tick in: a new `shape` replaces the pixels, every tick moves the
    /// position and visibility. `image` is the worker's copy, kept across shape-less ticks;
    /// nothing is published until the first shape. A tick that changes what the viewer sees
    /// marks the cell dirty while the pool blends.
    pub fn publish(
        &self,
        image: &mut Option<CursorImage>,
        hdr: &CursorShm,
        shape: Option<ShapeRgba>,
        visible: bool,
    ) {
        self.sdr_white_scale
            .store(hdr.sdr_white_scale, Ordering::Relaxed);
        if let Some(s) = shape {
            *image = Some(CursorImage {
                x: 0,
                y: 0,
                w: s.w,
                h: s.h,
                hot_x: s.hot_x,
                hot_y: s.hot_y,
                rgba: Arc::new(s.rgba),
                serial: hdr.shape_id,
                visible,
            });
        }
        let Some(img) = image.as_mut() else {
            return;
        };
        img.x = hdr.x;
        img.y = hdr.y;
        img.visible = visible;
        let changed = {
            let mut slot = crate::registry::lock(&self.image);
            let changed = slot.as_ref().is_none_or(|old| seen(old) != seen(img));
            *slot = Some(img.clone());
            changed
        };
        if changed && self.blends() {
            self.mark_dirty();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shm(x: i32, y: i32, shape_id: u32) -> CursorShm {
        let mut h: CursorShm = bytemuck::Zeroable::zeroed();
        h.x = x;
        h.y = y;
        h.shape_id = shape_id;
        h
    }

    fn arrow() -> ShapeRgba {
        ShapeRgba {
            w: 2,
            h: 2,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![255; 16],
        }
    }

    #[test]
    fn a_move_while_blending_is_dirty_once_until_taken() {
        let cell = CursorCell::default();
        let mut image = None;
        cell.set_blend(true);
        assert!(cell.take_dirty(), "the flip itself is damage");
        cell.publish(&mut image, &shm(10, 10, 1), Some(arrow()), true);
        cell.publish(&mut image, &shm(11, 10, 1), None, true);
        assert!(cell.take_dirty());
        assert!(!cell.take_dirty(), "two publishes coalesce into one take");
        cell.publish(&mut image, &shm(11, 10, 1), None, true);
        assert!(
            !cell.take_dirty(),
            "a publish that changes nothing is not damage"
        );
    }

    #[test]
    fn a_move_while_the_client_draws_is_not_damage() {
        let cell = CursorCell::default();
        let mut image = None;
        cell.publish(&mut image, &shm(10, 10, 1), Some(arrow()), true);
        cell.publish(&mut image, &shm(50, 50, 1), None, true);
        assert!(!cell.take_dirty());
        cell.set_blend(false);
        assert!(!cell.take_dirty());
    }

    #[test]
    fn handing_the_pointer_back_is_one_frame_that_erases_the_blend() {
        let cell = CursorCell::default();
        let mut image = None;
        cell.set_blend(true);
        cell.publish(&mut image, &shm(10, 10, 1), Some(arrow()), true);
        assert!(cell.take_dirty());
        cell.set_blend(false);
        assert!(
            cell.take_dirty(),
            "the stash still carries the blended pointer"
        );
        assert!(
            cell.to_blend().is_none(),
            "the erase frame draws no pointer"
        );
        cell.set_blend(false);
        cell.publish(&mut image, &shm(40, 40, 1), None, true);
        assert!(
            !cell.take_dirty(),
            "while the client draws, nothing more is damage"
        );
    }

    #[test]
    fn a_still_pointer_while_blending_sends_no_frame() {
        let cell = CursorCell::default();
        let mut image = None;
        cell.set_blend(true);
        cell.publish(&mut image, &shm(10, 10, 1), Some(arrow()), true);
        assert!(cell.take_dirty(), "the first pointer is one frame");
        // Idle ticks at the same spot: a still desktop keeps composing nothing.
        for _ in 0..10 {
            cell.publish(&mut image, &shm(10, 10, 1), None, true);
        }
        assert!(!cell.take_dirty(), "a stationary pointer marks no damage");
    }
}
