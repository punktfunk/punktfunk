//! [`WlCapturer`]: the encode loop's end of the direct capture thread in [`super`].

use super::spawn;
use crate::linux::{
    drain_edges, health_identity, wait_for_frame, FrameSlot, Nudges, StreamShared, StreamStatus,
};
use crate::{CapturedFrame, Capturer, Feed, FirstFrame, Paced, ZeroCopyPolicy};
use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;

/// Direct `ext-image-copy-capture-v1` capturer: the compositor fills buffers we
/// allocate, and we re-arm the instant one is ready.
///
/// Shares the portal's one-deep mailbox and wakeup edge, so the encode loop's
/// arrival wait is unchanged. What it does not share is any pacing: there is no
/// portal round trip and no PipeWire graph, so the rate is the compositor's
/// repaint rate (`design/linux-consumer-driven-capture.md` §8).
pub struct WlCapturer {
    slot: FrameSlot,
    wake: Receiver<()>,
    /// Set by [`Feed::arrival_poke`] and [`Feed::au_wake`]; the arrival wait takes one and
    /// returns with no frame.
    nudges: Arc<Nudges>,
    /// The thread's wake sender, held weakly so the thread's end still disconnects `wake`.
    poke_wake: Weak<SyncSender<()>>,
    shared: StreamShared,
    output_name: String,
    quit: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
    /// Holds the compositor output; dropped after the thread is joined, unless a
    /// capture-only rebuild took it back (`take_keepalive`).
    keepalive: Option<Box<dyn Send>>,
    /// The output's mastering volume while the pool is 10-bit PQ. Fixed for the capture's
    /// life: a re-lit output ends the thread and the rebuild reads it again.
    hdr_meta: Option<pf_frame::HdrMeta>,
}

impl WlCapturer {
    /// Open the named compositor output with identity-scoped failure health.
    /// Missing protocol/output or no consumer-importable dmabuf keeps the portal path,
    /// so a failure hands `keepalive` back for it. `want_hdr` captures the output's
    /// packed 10-bit buffer and fails unless the output is lit in BT.2020 PQ.
    pub fn open(
        output_name: String,
        keepalive: Box<dyn Send>,
        policy: ZeroCopyPolicy,
        want_hdr: bool,
        knobs: crate::knobs::Knobs,
    ) -> std::result::Result<WlCapturer, (anyhow::Error, Box<dyn Send>)> {
        let slot: FrameSlot = Arc::new(std::sync::Mutex::new(None));
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        output_name.hash(&mut hash);
        let identity = health_identity(hash.finish() | (1 << 63), &policy);
        let shared = StreamShared::new(pf_zerocopy::zero_copy_health(identity));
        let h = match spawn(
            output_name.clone(),
            policy,
            want_hdr,
            knobs.header_pts,
            slot,
            shared,
        ) {
            Ok(h) => h,
            Err(e) => return Err((e, keepalive)),
        };
        Ok(WlCapturer {
            slot: h.slot,
            wake: h.wake,
            nudges: Arc::default(),
            poke_wake: h.poke_wake,
            shared: h.shared,
            output_name,
            quit: h.quit,
            join: Some(h.join),
            keepalive: Some(keepalive),
            hdr_meta: h.hdr_meta,
        })
    }

    fn take_frame(&self) -> Option<CapturedFrame> {
        self.slot.lock().ok().and_then(|mut s| s.take())
    }
}

impl WlCapturer {
    fn frame_within(&mut self, budget: Duration) -> Result<CapturedFrame> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if let Some(f) = self.take_frame() {
                return Ok(f);
            }
            if self.shared.status() == StreamStatus::Broken {
                return Err(anyhow!(
                    "direct wayland capture failed on output {}",
                    self.output_name
                ));
            }
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                return Err(anyhow!(
                    "no frame within {:.1}s from the direct wayland capture on output {} — the \
                     compositor accepted the session but painted nothing",
                    budget.as_secs_f32(),
                    self.output_name
                ));
            };
            // The thread owns the sender: disconnected means it ended, a panic included,
            // which leaves the status `Streaming`. A failure is `Broken`, which the loop reports.
            let ended = match self.wake.recv_timeout(left.min(Duration::from_millis(500))) {
                Ok(()) => false,
                Err(RecvTimeoutError::Disconnected) => self.shared.status() != StreamStatus::Broken,
                Err(RecvTimeoutError::Timeout) => !self.shared.status().is_streaming(),
            };
            if ended {
                return self
                    .take_frame()
                    .ok_or_else(|| anyhow!("direct wayland capture thread ended"));
            }
        }
    }
}

impl Capturer for WlCapturer {
    /// No offer to convict: a provisional budget is only a shorter wait.
    fn next_frame(&mut self, budget: FirstFrame) -> Result<CapturedFrame> {
        match budget {
            FirstFrame::Default => self.frame_within(Duration::from_secs(10)),
            FirstFrame::Within(b) | FirstFrame::Provisional(b) => self.frame_within(b),
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        if self.shared.status() == StreamStatus::Broken {
            return Err(anyhow!(
                "direct wayland capture lost on output {} — rebuilding capture",
                self.output_name
            ));
        }
        // A thread that ended without a failure (the compositor stopped the
        // session) fails here too, after its leftover frame, so the loop rebuilds.
        let producer_gone = drain_edges(&self.wake);
        let latest = self.take_frame();
        if producer_gone && latest.is_none() {
            return Err(anyhow!("direct wayland capture thread ended"));
        }
        Ok(latest)
    }

    fn is_alive(&self) -> bool {
        self.shared.status() != StreamStatus::Broken
            && self.join.as_ref().is_some_and(|j| !j.is_finished())
    }

    fn colour(&self) -> crate::ColourFacts {
        crate::ColourFacts {
            hdr_meta: self.hdr_meta,
            sdr_fp16: false,
        }
    }

    fn feed(&mut self) -> Option<&mut dyn Feed> {
        Some(self)
    }
}

impl WlCapturer {
    /// Sets the nudge (`encoded`: an access unit, else a poke), then the thread's wake edge.
    fn nudge(&self, encoded: bool) -> Arc<dyn Fn() + Send + Sync> {
        let (nudges, wake) = (self.nudges.clone(), self.poke_wake.clone());
        Arc::new(move || {
            nudges.flag(encoded).store(true, Ordering::Relaxed);
            if let Some(wake) = wake.upgrade() {
                let _ = wake.try_send(());
            }
        })
    }
}

impl Feed for WlCapturer {
    fn wait_arrival(&mut self, deadline: std::time::Instant) {
        self.wait_paced(std::time::Instant::now(), deadline);
    }

    fn wait_paced(
        &mut self,
        not_before: std::time::Instant,
        deadline: std::time::Instant,
    ) -> Paced {
        let (slot, wake, shared, nudges) = (&self.slot, &self.wake, &self.shared, &self.nudges);
        wait_for_frame(slot, wake, shared, nudges, not_before, deadline)
    }

    fn arrival_poke(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        Some(self.nudge(false))
    }

    fn au_wake(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        Some(self.nudge(true))
    }

    fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
        self.keepalive.take()
    }
}

impl Drop for WlCapturer {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod wl_capturer_tests {
    use super::{Capturer, StreamShared, StreamStatus, WlCapturer};
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::sync_channel;
    use std::sync::Arc;

    /// A thread that ended cleanly drops its wake sender. `try_latest` must say so,
    /// or the encode loop repeats the last frame and never rebuilds capture.
    #[test]
    fn try_latest_reports_an_ended_thread() {
        let (edge, wake) = sync_channel::<()>(1);
        let mut cap = WlCapturer {
            slot: Arc::default(),
            wake,
            nudges: Arc::default(),
            poke_wake: std::sync::Weak::new(),
            shared: StreamShared::new(pf_zerocopy::zero_copy_health(u64::MAX)),
            output_name: "TEST-1".into(),
            quit: Arc::new(AtomicBool::new(false)),
            join: None,
            keepalive: None,
            hdr_meta: None,
        };
        edge.try_send(()).expect("empty channel");
        assert!(
            matches!(cap.try_latest(), Ok(None)),
            "a live thread with no frame is idle"
        );
        drop(edge);
        assert!(cap.try_latest().is_err());
    }

    /// A state the capture thread leaves its capturer in.
    #[derive(Clone, Copy, Debug)]
    enum At {
        /// `open` returned.
        Streaming,
        /// The session stopped and the thread returned without a failure.
        Stopped,
        /// The thread failed once streaming.
        Broken,
    }

    /// Leave `s` as `open` and the thread's end leave it at `at`.
    fn settle(s: &StreamShared, at: At) {
        s.report(StreamStatus::Streaming { driving: false });
        if matches!(at, At::Broken) {
            s.mark_broken();
        }
        if !matches!(at, At::Streaming) {
            s.report(StreamStatus::Paused);
        }
    }

    /// A capturer at `at` with an empty slot. Fields drop in order: the parked thread ends
    /// before the capturer's `Drop` joins it.
    struct Rig {
        _parked: Option<std::sync::mpsc::Sender<()>>,
        edge: Option<Arc<std::sync::mpsc::SyncSender<()>>>,
        cap: WlCapturer,
    }

    fn rig(at: At) -> Rig {
        let (edge, wake) = sync_channel::<()>(1);
        let edge = Arc::new(edge);
        let (parked, park) = std::sync::mpsc::channel::<()>();
        let join = std::thread::spawn(move || {
            let _ = park.recv();
        });
        let shared = StreamShared::new(pf_zerocopy::zero_copy_health(u64::MAX - 1));
        settle(&shared, at);
        let cap = WlCapturer {
            slot: Arc::default(),
            wake,
            nudges: Arc::default(),
            poke_wake: Arc::downgrade(&edge),
            shared,
            output_name: "TEST-1".into(),
            quit: Arc::new(AtomicBool::new(false)),
            join: Some(join),
            keepalive: None,
            hdr_meta: None,
        };
        Rig {
            _parked: Some(parked),
            edge: Some(edge),
            cap,
        }
    }

    impl Rig {
        /// The thread's end: its wakeup sender goes with it.
        fn gone(mut self) -> Rig {
            self.edge = None;
            self
        }

        fn put_frame(&self) {
            *self.cap.slot.lock().unwrap() = Some(crate::CapturedFrame {
                provenance: Default::default(),
                width: 1,
                height: 1,
                pts_ns: 1,
                format: crate::PixelFormat::Bgrx,
                payload: crate::FramePayload::Cpu(vec![0; 4]),
                cursor: None,
            });
        }
    }

    #[derive(Debug, PartialEq)]
    enum End {
        Frame,
        Failed,
        NoFrame,
        Ended,
        Idle,
        Lost,
    }

    const BUDGET: std::time::Duration = std::time::Duration::from_millis(300);

    /// The first-frame wait's end, and whether it waited out the budget.
    fn first(cap: &mut WlCapturer) -> (End, bool) {
        let t0 = std::time::Instant::now();
        let end = match cap.frame_within(BUDGET) {
            Ok(_) => End::Frame,
            Err(e) if format!("{e}").contains("capture failed") => End::Failed,
            Err(e) if format!("{e}").contains("no frame within") => End::NoFrame,
            Err(e) if format!("{e}").contains("thread ended") => End::Ended,
            Err(e) => panic!("{e:#}"),
        };
        (end, t0.elapsed() >= BUDGET)
    }

    fn latest(cap: &mut WlCapturer) -> End {
        match cap.try_latest() {
            Ok(Some(_)) => End::Frame,
            Ok(None) => End::Idle,
            Err(e) if format!("{e}").contains("capture lost") => End::Lost,
            Err(e) if format!("{e}").contains("thread ended") => End::Ended,
            Err(e) => panic!("{e:#}"),
        }
    }

    fn waits(cap: &mut WlCapturer) -> bool {
        use crate::Feed;
        let t0 = std::time::Instant::now();
        cap.wait_arrival(t0 + std::time::Duration::from_millis(50));
        t0.elapsed() >= std::time::Duration::from_millis(50)
    }

    /// The direct capturer's three ends. A failure outranks the wait but not a frame; a
    /// stream that stopped ends the wait at its next slice; a thread gone without a failure
    /// (a panic leaves `Streaming`) ends it at once.
    #[test]
    fn the_first_frame_wait_ends_three_ways() {
        use At::*;
        use End::*;
        #[rustfmt::skip]
        let rows = [
            (Streaming, (NoFrame, true), (Ended, false)),
            (Stopped,   (Ended, true),   (Ended, false)),
            (Broken,    (Failed, false), (Failed, false)),
        ];
        for (at, live, gone) in rows {
            assert_eq!(first(&mut rig(at).cap), live, "{at:?}");
            assert_eq!(first(&mut rig(at).gone().cap), gone, "{at:?}");
        }
        for at in [Streaming, Broken] {
            let mut r = rig(at);
            r.put_frame();
            assert_eq!(first(&mut r.cap), (Frame, false), "{at:?} with a frame");
        }
    }

    /// `try_latest`, with the thread gone, `is_alive`, `wait_arrival`: only a failure
    /// counts, and a stopped stream is not dead until its thread is.
    #[test]
    fn the_tick_readers_answer_by_the_failure_alone() {
        use At::*;
        use End::*;
        #[rustfmt::skip]
        let rows = [
            (Streaming, Idle, Ended, true,  true),
            (Stopped,   Idle, Ended, true,  true),
            (Broken,    Lost, Lost,  false, false),
        ];
        for (at, now, gone, alive, wait) in rows {
            let mut r = rig(at);
            assert_eq!(latest(&mut r.cap), now, "{at:?}");
            assert_eq!(r.cap.is_alive(), alive, "{at:?}");
            assert_eq!(waits(&mut r.cap), wait, "{at:?}");
            assert_eq!(latest(&mut r.gone().cap), gone, "{at:?}");
        }
    }

    /// A poke or an AU wake from another thread ends the wait with its own answer and no
    /// frame, the gate before `not_before` included, and is taken; a frame alone waits the
    /// gate out.
    #[test]
    fn a_nudge_ends_the_gate_and_a_frame_waits_for_it() {
        use crate::{Feed, Paced};
        use std::time::{Duration, Instant};
        let mut r = rig(At::Streaming);
        let nudges = [
            (r.cap.arrival_poke().expect("a poke"), Paced::Poked),
            (r.cap.au_wake().expect("an AU wake"), Paced::Encoded),
        ];
        for (nudge, answer) in nudges {
            let t0 = Instant::now();
            let (gate, deadline) = (t0 + Duration::from_secs(2), t0 + Duration::from_secs(3));
            let woke = std::thread::scope(|s| {
                s.spawn(|| {
                    std::thread::sleep(Duration::from_millis(20));
                    nudge();
                });
                r.cap.wait_paced(gate, deadline)
            });
            assert_eq!(woke, answer);
            assert!(t0.elapsed() < Duration::from_secs(1), "woke on {answer:?}");
        }
        r.put_frame();
        let t0 = Instant::now();
        let gate = t0 + Duration::from_millis(60);
        let woke = r.cap.wait_paced(gate, t0 + Duration::from_secs(2));
        assert_eq!(woke, Paced::Due, "the nudges were taken");
        assert!(Instant::now() >= gate, "a frame ended the gate");
    }
}
