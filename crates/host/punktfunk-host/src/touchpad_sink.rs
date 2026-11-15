//! One session's touchpad: [`TouchpadTracker`] gates frames by `seq`, and each applied frame
//! lands on a [`VirtualTouchpad`] in pad units ([`TouchpadFrame::to_pad`]). The device appears
//! on the first frame, an empty one included, and dies with the session.
//!
//! Clients repeat their frame while anything is down, so [`TOUCHPAD_TIMEOUT_MS`] without one
//! lifts everything. A repeat keeps its `seq`: the gate drops it, the clock still counts it.

use crate::inject::touchpad::VirtualTouchpad;
use punktfunk_core::quic::{PadState, TouchpadFrame, TouchpadTracker, TOUCHPAD_TIMEOUT_MS};
use std::time::{Duration, Instant};

/// What the sink drives: [`VirtualTouchpad`] in a session, a recorder in tests.
pub(crate) trait PadDevice: Sized {
    fn create() -> anyhow::Result<Self>;
    fn apply(&mut self, pad: &PadState);
}

impl PadDevice for VirtualTouchpad {
    fn create() -> anyhow::Result<Self> {
        VirtualTouchpad::create()
    }

    fn apply(&mut self, pad: &PadState) {
        VirtualTouchpad::apply(self, pad);
    }
}

pub(crate) struct TouchpadSink<D = VirtualTouchpad> {
    tracker: TouchpadTracker,
    dev: Option<D>,
    /// Create failed once: never retry at frame rate. The tracker still follows the frames.
    create_failed: bool,
    last_rx: Instant,
}

impl<D: PadDevice> TouchpadSink<D> {
    pub(crate) fn new() -> Self {
        TouchpadSink {
            tracker: TouchpadTracker::default(),
            dev: None,
            create_failed: false,
            last_rx: Instant::now(),
        }
    }

    pub(crate) fn apply(&mut self, frame: &TouchpadFrame) {
        self.apply_at(frame, Instant::now());
    }

    fn apply_at(&mut self, frame: &TouchpadFrame, now: Instant) {
        self.last_rx = now;
        if self.dev.is_none() && !self.create_failed {
            match D::create() {
                Ok(d) => self.dev = Some(d),
                Err(e) => {
                    self.create_failed = true;
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "virtual touchpad not created; touchpad input is dropped this session"
                    );
                }
            }
        }
        if self.tracker.apply(frame) {
            self.present(frame);
        }
    }

    /// Lift everything, at session end and when the client went silent.
    pub(crate) fn force_release(&mut self) {
        if let Some(lifted) = self.tracker.force_release() {
            self.present(&lifted);
        }
    }

    /// The input loop wakes at least every 100 ms while this is active, so the timeout runs.
    pub(crate) fn active(&self) -> bool {
        self.tracker.is_active()
    }

    pub(crate) fn check_timeout(&mut self) {
        self.check_timeout_at(Instant::now());
    }

    fn check_timeout_at(&mut self, now: Instant) {
        let deadline = Duration::from_millis(u64::from(TOUCHPAD_TIMEOUT_MS));
        if self.active() && now.saturating_duration_since(self.last_rx) >= deadline {
            tracing::debug!("touchpad frames went silent; lifting every contact");
            self.force_release();
        }
    }

    fn present(&mut self, frame: &TouchpadFrame) {
        if let Some(dev) = self.dev.as_mut() {
            dev.apply(&frame.to_pad());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::TouchpadContact;
    use std::cell::RefCell;

    thread_local! {
        static CREATES: RefCell<u32> = const { RefCell::new(0) };
        static FAIL: RefCell<bool> = const { RefCell::new(false) };
    }

    #[derive(Default)]
    struct Recorder(Vec<PadState>);

    impl PadDevice for Recorder {
        fn create() -> anyhow::Result<Self> {
            CREATES.with(|c| *c.borrow_mut() += 1);
            if FAIL.with(|f| *f.borrow()) {
                anyhow::bail!("recorder refuses");
            }
            Ok(Recorder::default())
        }

        fn apply(&mut self, pad: &PadState) {
            self.0.push(*pad);
        }
    }

    fn sink(fail: bool) -> TouchpadSink<Recorder> {
        CREATES.with(|c| *c.borrow_mut() = 0);
        FAIL.with(|f| *f.borrow_mut() = fail);
        TouchpadSink::new()
    }

    fn creates() -> u32 {
        CREATES.with(|c| *c.borrow())
    }

    fn frame(seq: u16, contacts: &[TouchpadContact]) -> TouchpadFrame {
        TouchpadFrame::new(seq, false, 700, 1500, contacts).unwrap()
    }

    fn finger(x: u16) -> TouchpadContact {
        TouchpadContact {
            id: 0,
            tip: true,
            confident: true,
            x,
            y: 0,
        }
    }

    fn applied(s: &TouchpadSink<Recorder>) -> &[PadState] {
        &s.dev.as_ref().unwrap().0
    }

    #[test]
    fn an_empty_frame_builds_the_device_and_stale_ones_never_reach_it() {
        let mut s = sink(false);
        s.apply(&frame(0, &[]));
        assert_eq!(creates(), 1);
        assert!(applied(&s)[0].contacts().is_empty());
        s.apply(&frame(1, &[finger(0)]));
        s.apply(&frame(1, &[finger(0)]));
        s.apply(&frame(0, &[]));
        assert_eq!(creates(), 1);
        assert_eq!(applied(&s).len(), 2, "the repeat and the stale frame drop");
        assert_eq!(applied(&s)[1].contacts()[0].x, 12500, "in pad units");
        assert!(s.active());
    }

    #[test]
    fn a_failed_create_is_not_retried() {
        let mut s = sink(true);
        s.apply(&frame(0, &[finger(0)]));
        s.apply(&frame(1, &[finger(5)]));
        s.check_timeout_at(Instant::now() + Duration::from_secs(1));
        assert_eq!(creates(), 1);
        assert!(s.dev.is_none());
        assert!(!s.active(), "the tracker still follows");
    }

    #[test]
    fn silence_lifts_every_contact_and_repeats_hold_it_down() {
        let mut s = sink(false);
        let t0 = Instant::now();
        s.apply_at(&frame(0, &[finger(0)]), t0);
        s.apply_at(&frame(0, &[finger(0)]), t0 + Duration::from_millis(150));
        s.check_timeout_at(t0 + Duration::from_millis(300));
        assert!(s.active(), "a repeat keeps the clock running");
        s.check_timeout_at(t0 + Duration::from_millis(350));
        assert!(!s.active());
        let last = applied(&s).last().unwrap();
        assert!(last.contacts().is_empty() && !last.button);
        let n = applied(&s).len();
        s.force_release();
        assert_eq!(applied(&s).len(), n, "nothing left to lift");
    }
}
