//! Live capture on Linux: [`PortalCapturer`] (the portal monitor or a virtual output's node,
//! through PipeWire) and the direct [`WlCapturer`]. This file holds the cells each shares with
//! its thread ([`StreamShared`], [`CursorSlot`], [`ImporterCell`]) and the direct capture's
//! one-deep [`FrameSlot`]. A frame's size is the negotiated format, not the portal hint.
//!
//! - `portal.rs`: the ScreenCast/RemoteDesktop handshake and the GNOME colour-mode probe.
//! - `pipewire/`: the PipeWire consumer on its own thread, and its front end.
//! - `direct/`: `ext-image-copy-capture-v1` into dmabufs this side allocates, no PipeWire.
//! - `delivery.rs`: one buffer to one published frame, the same way from both captures.
//! - `pw_pods.rs`: the `EnumFormat`, `Buffers` and `Meta` pods the stream offers.
//! - `pw_cursor.rs`: the `SPA_META_Cursor` parser and the CPU cursor blit.
//! - `xfixes_cursor.rs`: gamescope's pointer, from XFixes, into the [`CursorSlot`].
//! - `sync_timeline.rs`: explicit sync (`SPA_META_SyncTimeline`): syncobj waits and signals.

use super::{CapturedFrame, PixelFormat, ZeroCopyPolicy};

mod xfixes_cursor;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Arc, Mutex};

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
    shared: &StreamShared,
    deadline: std::time::Instant,
) {
    if shared.status() == StreamStatus::Broken {
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

/// One stream's state, in the order its readers rank it: `Broken` outranks everything,
/// `Errored` everything else, and only `Streaming` delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StreamStatus {
    /// No `Streaming` state yet.
    Negotiating,
    /// `driving`: the producer paints only in cycles the loop thread's pacer starts.
    Streaming { driving: bool },
    /// Left `Streaming` for a state other than `Error`; a renegotiation passes through here.
    Paused,
    /// The stream reached `Error` (e.g. "no more input formats"). It never delivers again.
    Errored,
    /// GPU import is gone for this capture (worker death, or a tiled or planar frame the
    /// CPU de-pad cannot read).
    Broken,
}

impl StreamStatus {
    /// The status once the producer reports `next`. `Errored` and `Broken` stay, and a
    /// stream that never streamed is still negotiating.
    fn then(self, next: StreamStatus) -> StreamStatus {
        match (self, next) {
            (StreamStatus::Errored | StreamStatus::Broken, _) => self,
            (StreamStatus::Negotiating, StreamStatus::Paused) => self,
            _ => next,
        }
    }

    fn is_streaming(self) -> bool {
        matches!(self, StreamStatus::Streaming { .. })
    }

    fn to_u8(self) -> u8 {
        match self {
            StreamStatus::Negotiating => 0,
            StreamStatus::Streaming { driving: false } => 1,
            StreamStatus::Streaming { driving: true } => 2,
            StreamStatus::Paused => 3,
            StreamStatus::Errored => 4,
            StreamStatus::Broken => 5,
        }
    }

    fn from_u8(b: u8) -> StreamStatus {
        match b {
            1 => StreamStatus::Streaming { driving: false },
            2 => StreamStatus::Streaming { driving: true },
            3 => StreamStatus::Paused,
            4 => StreamStatus::Errored,
            5 => StreamStatus::Broken,
            _ => StreamStatus::Negotiating,
        }
    }
}

/// What this capture offered and what the producer fixated, read together by the
/// first-frame verdict.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Negotiation {
    /// The thread advertised the EGL→CUDA dmabuf-only offer, before connect. A failed
    /// importer offers no dmabuf, so a timeout must not latch the GPU offer off.
    gpu_offer: bool,
    /// The last Format the producer fixated; `None` until one parses.
    fixated: Option<Fixated>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fixated {
    /// 10-bit under PQ.
    hdr: bool,
    /// Gamescope's cursor maps root space into it (`-w/-h` and `-W/-H` are independent).
    size: (u32, u32),
}

/// What a capturer and its stream's thread share: the status, the negotiation and the re-send
/// count one way, the active gate the other, and the failure memory both write.
#[derive(Clone)]
struct StreamShared {
    /// Failure memory scoped to this producer identity and shared with encoder frames.
    health: pf_zerocopy::ZeroCopyHealth,
    /// [`crate::Feed::set_active`]: the PipeWire thread's per-frame de-pad runs only while
    /// set, so pooling a 5K capturer is cheap between streams. The direct capturer has none.
    active: Arc<AtomicBool>,
    /// The producer's last reported [`StreamStatus`], written by its thread alone. An atomic:
    /// `.process` reads the status per buffer and must not wait on the consumer.
    reported: Arc<AtomicU8>,
    /// [`StreamStatus::Broken`], latched apart from `reported` because every published dmabuf
    /// carries it as `rebuild`: an encoder that rejects the frame sets it too. Never cleared.
    broken: Arc<AtomicBool>,
    negotiation: Arc<Mutex<Negotiation>>,
    /// Buffers the producer sent again while this capture still held them (PipeWire < 1.6,
    /// no `node.reliable`). Each is re-held under a new generation; the count rides the
    /// provenance line, and [`crate::SourceEvent::ReferenceRisk`] turns a step into one IDR.
    resent: Arc<std::sync::atomic::AtomicU64>,
    /// The pool's depth and withheld peak, written by the PipeWire thread alone.
    pool: Arc<PoolGauge>,
}

/// What [`crate::Feed::pool_use`] reads: the census and the most buffers withheld at once.
#[derive(Default)]
struct PoolGauge {
    depth: AtomicU32,
    withheld_peak: AtomicU32,
}

/// The pointer the host places: the PipeWire thread and the gamescope XFixes reader write it,
/// the capturer reads it.
#[derive(Clone, Default)]
struct CursorSlot {
    /// Overlay from every buffer's `SPA_META_Cursor`, including cursor-only
    /// buffers that never become frames. Gamescope XFixes publishes here too.
    live: Arc<Mutex<Option<pf_frame::CursorOverlay>>>,
    /// The host forwards or blends [`Self::live`] itself, so the CPU copy must not bake the
    /// pointer into its pixels as well. Set by [`crate::CursorSource::set_forward`].
    host_places: Arc<AtomicBool>,
}

/// NVIDIA zero-copy importer, usually the isolated worker. The loop thread fills it after
/// negotiation; the consumer imports held frames through it ([`PortalCapturer::import_held`])
/// and retires it on a LINEAR failure.
#[derive(Clone, Default)]
struct ImporterCell {
    slot: Arc<Mutex<Option<pf_zerocopy::Importer>>>,
    /// `slot` is `Some`. Read per frame on the loop thread without the lock.
    present: Arc<AtomicBool>,
    /// The importer runs in this process, its GL context current on the loop thread only. Read
    /// per frame without the lock, which the consumer holds across a whole worker import.
    in_process: Arc<AtomicBool>,
}

/// The NVENC raw lane may take this capture: the encoder offers it and the fused convert samples
/// at least one tiled BGRx layout. With none, a tiled producer could only be offered LINEAR.
fn raw_lane_ok(policy: &ZeroCopyPolicy) -> bool {
    policy.nvenc_raw_dmabuf
        && !pf_zerocopy::fused_convert_modifiers(pf_zerocopy::drm::XR24).is_empty()
}

/// `importable` narrowed to the modifiers the raw lane's fused convert samples, order kept.
/// LINEAR stays.
fn raw_lane_modifiers(importable: &[u64], fused: &[u64]) -> Vec<u64> {
    importable
        .iter()
        .copied()
        .filter(|m| *m == 0 || fused.contains(m))
        .collect()
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

impl StreamShared {
    fn new(health: pf_zerocopy::ZeroCopyHealth) -> Self {
        Self {
            health,
            active: Arc::new(AtomicBool::new(false)),
            reported: Arc::new(AtomicU8::new(StreamStatus::Negotiating.to_u8())),
            broken: Arc::new(AtomicBool::new(false)),
            negotiation: Arc::default(),
            resent: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            pool: Arc::default(),
        }
    }

    /// The status every verdict reads: the broken latch, else the producer's report.
    fn status(&self) -> StreamStatus {
        if self.broken.load(Ordering::Relaxed) {
            StreamStatus::Broken
        } else {
            self.reported()
        }
    }

    /// The producer's last report, which the broken latch does not touch. The pacer follows it.
    fn reported(&self) -> StreamStatus {
        StreamStatus::from_u8(self.reported.load(Ordering::Relaxed))
    }

    fn report(&self, next: StreamStatus) {
        let _ = self
            .reported
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
                Some(StreamStatus::from_u8(b).then(next).to_u8())
            });
    }

    fn mark_broken(&self) {
        self.broken.store(true, Ordering::Relaxed);
    }

    fn negotiation(&self) -> Negotiation {
        *self.negotiation_mut()
    }

    fn offer_gpu(&self, offered: bool) {
        self.negotiation_mut().gpu_offer = offered;
    }

    fn fixate(&self, fixated: Fixated) {
        self.negotiation_mut().fixated = Some(fixated);
    }

    fn negotiation_mut(&self) -> std::sync::MutexGuard<'_, Negotiation> {
        self.negotiation.lock().unwrap_or_else(|e| e.into_inner())
    }
}

mod portal;
pub use portal::gnome_hdr_monitor_active;

mod pipewire;
pub(crate) use pipewire::PortalCapturer;
mod delivery;
mod direct;
pub(crate) use direct::{hdr_capture_globals, output_is_hdr10, WlCapturer};
mod pw_cursor;
mod pw_pods;
mod sync_timeline;

#[cfg(test)]
mod tests {
    use super::{raw_lane_modifiers, StreamShared, StreamStatus};

    /// `Errored` and `Broken` are terminal, `Paused` needs a stream first, and the broken
    /// latch outranks whatever the producer reports.
    #[test]
    fn the_stream_status_moves_only_where_the_producer_can_take_it() {
        use StreamStatus::*;
        let idle = Streaming { driving: false };
        let driven = Streaming { driving: true };
        #[rustfmt::skip]
        let rows: [(&[StreamStatus], StreamStatus); 8] = [
            (&[],                       Negotiating),
            (&[Paused],                 Negotiating),
            (&[Paused, idle],           idle),
            (&[idle, driven],           driven),
            (&[driven, Paused],         Paused),
            (&[Paused, Errored, idle],  Errored),
            (&[idle, Errored, Paused],  Errored),
            (&[Broken, idle],           Broken),
        ];
        for (reports, want) in rows {
            let s = StreamShared::new(pf_zerocopy::zero_copy_health(u64::MAX - 2));
            for &r in reports {
                s.report(r);
            }
            assert_eq!(s.status(), want, "{reports:?}");
            s.mark_broken();
            assert_eq!(s.status(), Broken, "{reports:?} then broken");
            assert_eq!(s.reported(), want, "the latch leaves the report");
        }
        for s in [Negotiating, idle, driven, Paused, Errored, Broken] {
            assert_eq!(StreamStatus::from_u8(s.to_u8()), s);
        }
    }

    /// NVIDIA 595, 2560×1440 BGRx: EGL imports the compressed block-linear layout KWin's gbm
    /// picks; the fused convert's Vulkan device samples only the plain ones.
    #[test]
    fn raw_lane_offer_drops_what_the_convert_cannot_sample() {
        const PLAIN: u64 = 0x0300_0000_0060_6010;
        const COMPRESSED: u64 = 0x0300_0000_00e0_8014;
        let egl = [PLAIN, COMPRESSED, PLAIN + 1, 0];
        let fused = [PLAIN + 1, PLAIN];
        assert_eq!(raw_lane_modifiers(&egl, &fused), vec![PLAIN, PLAIN + 1, 0]);
        assert_eq!(
            raw_lane_modifiers(&egl, &[]),
            vec![0],
            "LINEAR always stays"
        );
        assert!(raw_lane_modifiers(&[], &fused).is_empty());
    }
}
