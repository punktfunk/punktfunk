//! The wake shape, switched by evidence: a receiver that loses the head of a burst while
//! its adapter wakes gets a small first group and a short gap on every frame. It costs
//! about half a millisecond a frame, so it runs only while the loss says so.

use std::time::{Duration, Instant};

/// Consecutive head-signature windows ([`super::verdict::head_signature`]) that turn the
/// shape on.
pub(super) const WAKE_WINDOWS: u8 = 2;
/// Calm this long, the head no worse than the rest, turns it off again.
pub(super) const WAKE_OFF_SECS: u64 = 60;

/// How the client asks the host to shape its frames (`Feedback::shape`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Shape {
    Auto = 0,
    Wake = 1,
}

#[derive(Debug, Default)]
pub(crate) struct ShapeState {
    on: bool,
    head_windows: u8,
    quiet_since: Option<Instant>,
}

impl ShapeState {
    /// One closed window at `now`: whether it carried the head signature, and whether its
    /// head lost no more than the rest. `Some` when the shape changes.
    pub(crate) fn note(&mut self, head_sig: bool, calm: bool, now: Instant) -> Option<Shape> {
        self.head_windows = if head_sig {
            self.head_windows.saturating_add(1)
        } else {
            0
        };
        if !self.on {
            if self.head_windows < WAKE_WINDOWS {
                return None;
            }
            self.on = true;
            self.quiet_since = None;
            return Some(Shape::Wake);
        }
        if !calm {
            self.quiet_since = None;
            return None;
        }
        let since = *self.quiet_since.get_or_insert(now);
        if now.duration_since(since) < Duration::from_secs(WAKE_OFF_SECS) {
            return None;
        }
        *self = ShapeState::default();
        Some(Shape::Auto)
    }

    pub(crate) fn on(&self) -> bool {
        self.on
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two windows of head loss turn the shape on; one does not. A minute without head
    /// loss turns it off.
    #[test]
    fn head_loss_twice_wakes_the_shape_and_a_calm_minute_ends_it() {
        let t0 = Instant::now();
        let mut s = ShapeState::default();
        assert_eq!(s.note(true, false, t0), None);
        assert_eq!(s.note(false, true, t0), None, "the run restarts");
        assert_eq!(s.note(true, false, t0), None);
        assert_eq!(s.note(true, false, t0), Some(Shape::Wake));
        assert!(s.on());
        let w = |s| t0 + Duration::from_secs(s);
        assert_eq!(s.note(false, true, w(1)), None);
        assert_eq!(s.note(false, false, w(30)), None, "the head still loses");
        assert_eq!(s.note(false, true, w(31)), None);
        assert_eq!(s.note(false, true, w(90)), None);
        assert_eq!(s.note(false, true, w(91)), Some(Shape::Auto));
        assert!(!s.on());
    }
}
