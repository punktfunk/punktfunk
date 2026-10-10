//! [`WlCapturer`]: the encode loop's end of the direct capture thread in [`super`].

use super::spawn;
use crate::linux::{drain_edges, health_identity, wait_for_frame, CaptureSignals, FrameSlot};
use crate::{CapturedFrame, Capturer, ZeroCopyPolicy};
use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
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
    signals: CaptureSignals,
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
    ) -> std::result::Result<WlCapturer, (anyhow::Error, Box<dyn Send>)> {
        let slot: FrameSlot = Arc::new(std::sync::Mutex::new(None));
        use std::hash::{Hash, Hasher};
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        output_name.hash(&mut hash);
        let identity = health_identity(hash.finish() | (1 << 63), &policy);
        let signals = CaptureSignals::new(pf_zerocopy::zero_copy_health(identity));
        signals.active.store(true, Ordering::Relaxed);
        let h = match spawn(output_name.clone(), policy, want_hdr, slot, signals) {
            Ok(h) => h,
            Err(e) => return Err((e, keepalive)),
        };
        Ok(WlCapturer {
            slot: h.slot,
            wake: h.wake,
            signals: h.signals,
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

impl Capturer for WlCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        self.next_frame_within(Duration::from_secs(10))
    }

    fn next_frame_within(&mut self, budget: Duration) -> Result<CapturedFrame> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            if let Some(f) = self.take_frame() {
                return Ok(f);
            }
            if self.signals.broken.load(Ordering::Relaxed) {
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
            if self
                .wake
                .recv_timeout(left.min(Duration::from_millis(500)))
                .is_err()
                && !self.signals.streaming.load(Ordering::Relaxed)
            {
                return Err(anyhow!("direct wayland capture thread ended"));
            }
        }
    }

    fn supports_arrival_wait(&self) -> bool {
        true
    }

    fn wait_arrival(&mut self, deadline: std::time::Instant) {
        wait_for_frame(&self.slot, &self.wake, &self.signals.broken, deadline);
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        if self.signals.broken.load(Ordering::Relaxed) {
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

    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        self.signals.cursor_live.lock().ok().and_then(|c| c.clone())
    }

    fn is_alive(&self) -> bool {
        !self.signals.broken.load(Ordering::Relaxed)
            && self.join.as_ref().is_some_and(|j| !j.is_finished())
    }

    fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
        self.keepalive.take()
    }

    fn hdr_meta(&self) -> Option<pf_frame::HdrMeta> {
        self.hdr_meta
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
    use super::{CaptureSignals, Capturer, WlCapturer};
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
            signals: CaptureSignals::new(pf_zerocopy::zero_copy_health(u64::MAX)),
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
}
