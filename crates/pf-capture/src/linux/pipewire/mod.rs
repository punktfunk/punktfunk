//! The PipeWire consumer, confined to its own thread (the PW types are `!Send`).
//!
//! [`plan`] resolves the zero-copy negotiation, [`offers`] builds the modifier offers,
//! [`thread`] runs the stream, [`consume`] turns one buffer into a frame, [`queue`] keeps the
//! frames until their renders finish, [`hold`] keeps a published buffer from the producer
//! until encode lets go, and [`pacer`] drives a lazy producer.

mod consume;
mod hold;
mod offers;
mod pacer;
mod plan;
mod queue;
mod thread;

pub(super) use consume::{realtime_minus_monotonic_ns, FenceWaitStats};
pub(super) use plan::{
    gpu_import, negotiation_plan, ImportOutcome, ImportPolicy, ImportState, NegotiationInputs,
};
pub(super) use queue::{wait_ready, FrameQueue, Taken};
pub(super) use thread::pipewire_thread;

use super::pw_cursor::CursorState;
use super::sync_timeline::SyncDevice;
use super::{CapturedFrame, PixelFormat};
use hold::{DeferredRequeue, PoolCensus};
use pacer::Pacer;
use pipewire as pw;
use plan::PassthroughFallbacks;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use std::sync::mpsc::SyncSender;

fn map_format(f: VideoFormat) -> Option<PixelFormat> {
    Some(match f {
        VideoFormat::BGRx => PixelFormat::Bgrx,
        VideoFormat::RGBx => PixelFormat::Rgbx,
        VideoFormat::BGRA => PixelFormat::Bgra,
        VideoFormat::RGBA => PixelFormat::Rgba,
        VideoFormat::RGB => PixelFormat::Rgb,
        VideoFormat::BGR => PixelFormat::Bgr,
        VideoFormat::NV12 => PixelFormat::Nv12,
        // The 10-bit offers negotiate these: PQ (`want_hdr`) or gamescope's BT.709 SDR
        // (`sdr10_native`). The fixated transfer function says which.
        VideoFormat::xRGB_210LE => PixelFormat::X2Rgb10,
        VideoFormat::xBGR_210LE => PixelFormat::X2Bgr10,
        VideoFormat::P010_10LE => PixelFormat::P010,
        _ => return None,
    })
}

struct UserData {
    info: VideoInfoRaw,
    /// `None` until `param_changed`, or if the SPA format is unsupported.
    format: Option<PixelFormat>,
    /// DRM modifier for dmabuf import; 0 = LINEAR.
    modifier: u64,
    /// Arrivals awaiting their renders; write only through [`UserData::publish`].
    queue: FrameQueue,
    wake: SyncSender<()>,
    signals: super::CaptureSignals,
    /// Raw dmabuf to the encoder instead of a CUDA import (VAAPI).
    vaapi_passthrough: bool,
    /// Tiled 10-bit was offered because the encoder's raw convert reads it; hold it, never import.
    hdr_tiled_raw: bool,
    /// CUDA import choices; the consumer imports held frames with the same policy.
    import_policy: ImportPolicy,
    /// Arrival-path import memory. The consumer keeps its own.
    import_state: ImportState,
    /// Rate-limit counter for the latest-frame-only diagnostic (see `.process`).
    dbg_log_n: u64,
    /// Which clock feeds wire `pts_ns`. Delivery stamps sit downstream of compositor jitter.
    pts: crate::pts_provenance::PtsProvenance,
    pts_reported: std::time::Instant,
    /// `CLOCK_REALTIME − CLOCK_MONOTONIC`, ns. Re-sampled each 30 s window; clocks drift by µs.
    rt_minus_mono_ns: i64,
    /// `PUNKTFUNK_CAPTURE_HDR_PTS=0` puts the wire back on the delivery stamp unconditionally.
    hdr_pts_enabled: bool,
    /// Negotiated pool depth from `add_buffer`/`remove_buffer`. Budget for a deeper encode pipeline.
    pool: PoolCensus,
    /// Raw-passthrough frames that fell through to CPU, by reason. Fresh `UserData` per pipeline.
    passthrough_fallbacks: PassthroughFallbacks,
    cursor: CursorState,
    /// Sacrificial birth-mode size (kwin.rs `create`). `.process` skips until it matches, then clears.
    expect_dims: Option<(u32, u32)>,
    /// Buffers skipped by `expect_dims` (rate-limits its log).
    gate_skips: u64,
    /// When the gate first held a buffer. After [`GATE_DEADLINE`] it disarms: degraded dims beat a retry loop.
    gate_since: Option<std::time::Instant>,
    /// Encode reads the dmabuf after `.process` returns; do not rejoin the pool until [`BufferHold`] drops.
    defer: std::sync::Arc<DeferredRequeue>,
    /// Lazy-driver pacing; `None` when the producer keeps the tick.
    pacer: Option<std::rc::Rc<Pacer>>,
    /// Arrivals dropped because no hold was to be had.
    held_drops: u64,
    damage: thread::DamageGate,
    /// Arrivals that repainted nothing and went straight back.
    undamaged: u64,
    /// Explicit-sync device; `None` when the lane cannot offer it. Whether a buffer carries
    /// sync points is the producer's call at negotiation.
    sync: Option<std::sync::Arc<SyncDevice>>,
}

impl UserData {
    /// Queue `frame` behind `fence`, the render its pixels wait on, then a wakeup edge.
    ///
    /// Must not block: this runs inside `.process` and would stall the compositor. A full
    /// wakeup channel already has a pending edge; the queue is the truth.
    fn publish(&self, frame: CapturedFrame, fence: Option<queue::RenderFence>) {
        if let Ok(mut q) = self.queue.lock() {
            q.push(frame, fence, std::time::Instant::now());
        }
        let _ = self.wake.try_send(());
    }
}
