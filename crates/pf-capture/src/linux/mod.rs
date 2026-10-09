//! Live capture on Linux: [`PortalCapturer`] (xdg ScreenCast portal or a virtual output's
//! node, through PipeWire) and the direct `ext-image-copy-capture-v1` [`WlCapturer`], plus the
//! failure signals and the frame mailbox they share.
//!
//! PipeWire frames leave through a queue that waits out each producer render
//! (`pipewire::queue`) plus a wakeup edge; the direct capturer's are
//! rendered on arrival and leave through a one-deep [`FrameSlot`]. Payload may
//! be packed RGB, NV12, YUV444, 10-bit PQ, or a dmabuf that never touches the
//! CPU. Size is the negotiated PipeWire format, not the portal hint.

use super::{CapturedFrame, Capturer, PixelFormat, ZeroCopyPolicy};
use anyhow::{anyhow, Result};

// Gamescope's PipeWire node has no `SPA_META_Cursor`; this fills `cursor_live` from XFixes.
mod xfixes_cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// One-deep overwriting mailbox: producer drops oldest. `sync_channel` is
/// drop-newest (`try_send` discards the fresh frame once full). A queued
/// [`CapturedFrame`] can own a dup'd dmabuf or CUDA buffer, so depth > 1
/// pins compositor buffers.
type FrameSlot = Arc<std::sync::Mutex<Option<CapturedFrame>>>;

/// `wait_arrival` for the direct capturer: returns once `slot` holds a frame,
/// `deadline` passes, or the producer is broken or gone. Never consumes: the
/// frame stays for `try_latest`, which also classifies a dead producer.
fn wait_for_frame(
    slot: &FrameSlot,
    wake: &Receiver<()>,
    broken: &AtomicBool,
    deadline: std::time::Instant,
) {
    if broken.load(Ordering::Relaxed) {
        return;
    }
    loop {
        if slot.lock().is_ok_and(|s| s.is_some()) {
            return;
        }
        let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return;
        };
        if wake.recv_timeout(left).is_err() {
            return;
        }
    }
}

/// Drains stale wakeup edges so the next [`wait_for_frame`] cannot return
/// early. `true` when the producer thread is gone (its sender dropped).
fn drain_edges(wake: &Receiver<()>) -> bool {
    loop {
        match wake.try_recv() {
            Ok(()) => continue,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

#[derive(Clone)]
struct CaptureSignals {
    /// Failure memory scoped to this producer identity and shared with encoder frames.
    health: pf_zerocopy::ZeroCopyHealth,
    /// Per-frame de-pad runs only while set; pooling a 5K capturer is cheap
    /// between streams.
    active: Arc<AtomicBool>,
    /// Format agreed. Timeout diagnosis: mismatch vs idle/unmapped compositor.
    negotiated: Arc<AtomicBool>,
    /// Stream is `Streaming`. Distinguishes a static desktop from a dead source.
    streaming: Arc<AtomicBool>,
    /// This stream drives the graph: the producer paints only in cycles the
    /// loop thread's pacer starts on its requests. Cleared with `streaming`.
    driving: Arc<AtomicBool>,
    /// GPU import is gone for this stream (worker death, or a tiled or planar
    /// frame the CPU de-pad cannot read). Never cleared.
    broken: Arc<AtomicBool>,
    /// The stream reached `Error` (e.g. "no more input formats"). Terminal: it never delivers.
    errored: Arc<AtomicBool>,
    /// Buffers the producer sent again while this capture still held them (PipeWire < 1.6,
    /// no `node.reliable`). Each is re-held under a new generation; the count rides the
    /// provenance line, and [`Capturer::take_reference_risk`] turns a step into one IDR.
    resent: Arc<std::sync::atomic::AtomicU64>,
    hdr_negotiated: Arc<AtomicBool>,
    /// Thread actually advertised the EGL→CUDA dmabuf-only offer. `plan.build_importer`
    /// is not enough: a failed importer means no dmabuf was offered, so a
    /// timeout must not latch the GPU offer off.
    gpu_dmabuf_offer: Arc<AtomicBool>,
    /// Overlay from every buffer's `SPA_META_Cursor`, including cursor-only
    /// buffers that never become frames. Gamescope XFixes publishes here too.
    cursor_live: Arc<std::sync::Mutex<Option<pf_frame::CursorOverlay>>>,
    /// The host forwards or blends [`Self::cursor_live`] itself, so the CPU copy must not
    /// bake the pointer into its pixels as well. Set by [`Capturer::set_cursor_forward`].
    host_places_cursor: Arc<AtomicBool>,
    /// Packed `(w << 32) | h`; `0` until `param_changed`. Gamescope cursor
    /// maps root-space into frame space (`-w/-h` vs `-W/-H` are independent).
    frame_size: Arc<std::sync::atomic::AtomicU64>,
    /// NVIDIA zero-copy importer, usually the isolated worker. The loop thread
    /// fills it after negotiation; the consumer imports held frames through it
    /// ([`PortalCapturer::import_held`]) and retires it on a LINEAR failure.
    importer: Arc<std::sync::Mutex<Option<pf_zerocopy::Importer>>>,
    /// `importer` is `Some`. Read per frame on the loop thread without the lock.
    has_importer: Arc<AtomicBool>,
    /// `importer` runs in this process, its GL context current on the loop thread only. Read per
    /// frame without the lock, which the consumer holds across a whole worker import.
    importer_in_process: Arc<AtomicBool>,
}

/// Producer identity plus the consumer policy whose failures must stay independent.
fn health_identity(base: u64, policy: &ZeroCopyPolicy) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    base.hash(&mut hash);
    policy.backend_is_vaapi.hash(&mut hash);
    policy.pyrowave_session.hash(&mut hash);
    policy.nvenc_raw_dmabuf.hash(&mut hash);
    policy.encoder_modifiers.hash(&mut hash);
    hash.finish()
}

impl CaptureSignals {
    fn new(health: pf_zerocopy::ZeroCopyHealth) -> Self {
        Self {
            health,
            active: Arc::new(AtomicBool::new(false)),
            negotiated: Arc::new(AtomicBool::new(false)),
            streaming: Arc::new(AtomicBool::new(false)),
            driving: Arc::new(AtomicBool::new(false)),
            broken: Arc::new(AtomicBool::new(false)),
            errored: Arc::new(AtomicBool::new(false)),
            resent: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            hdr_negotiated: Arc::new(AtomicBool::new(false)),
            gpu_dmabuf_offer: Arc::new(AtomicBool::new(false)),
            cursor_live: Arc::new(std::sync::Mutex::new(None)),
            host_places_cursor: Arc::new(AtomicBool::new(false)),
            frame_size: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            importer: Arc::new(std::sync::Mutex::new(None)),
            has_importer: Arc::new(AtomicBool::new(false)),
            importer_in_process: Arc::new(AtomicBool::new(false)),
        }
    }
}

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
        let h = match wl_capture::spawn(output_name.clone(), policy, want_hdr, slot, signals) {
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

// ScreenCast/RemoteDesktop handshake + GNOME colour-mode probe. Async,
// not per-frame. `gnome_hdr_monitor_active` is re-exported from `lib.rs`.
mod portal;
pub use portal::gnome_hdr_monitor_active;

// PipeWire consumer (`!Send`, owns its thread) and its front end, [`PortalCapturer`].
mod pipewire;
pub(crate) use pipewire::PortalCapturer;
// Client-allocated dmabufs for the direct capture path.
mod gbm_pool;
// Direct `ext-image-copy-capture-v1` capture, with no portal and no PipeWire.
mod wl_capture;
pub(crate) use wl_capture::output_is_hdr10;
// Negotiation POD builders and cursor-meta parser + CPU blits. Pure enough
// to unit-test without a compositor.
mod pw_cursor;
mod pw_pods;
// Explicit sync (`SPA_META_SyncTimeline`): syncobj waits and release signals.
mod sync_timeline;

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
