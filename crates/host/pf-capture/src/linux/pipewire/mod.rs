//! The PipeWire consumer, confined to its own thread (the PW types are `!Send`).
//!
//! - [`capturer`]: [`PortalCapturer`], the front end the encode loop holds.
//! - [`plan`]: the zero-copy negotiation, resolved once per pipeline.
//! - [`config`]: what the stream decides before its thread exists.
//! - [`offers`]: the producer's probed traits, the pacing and the modifier offers.
//! - [`thread`]: the loop thread: connect, offer, negotiate, run `.process` until quit.
//! - [`consume`]: one buffer to one frame: the wire stamp, the fence, the delivery lanes.
//! - [`queue`]: the frames in arrival order, each kept until its render finishes.
//! - [`ledger`]: each pool buffer's life; a published buffer is held until encode lets go.
//! - [`pacer`]: the paint requests that drive a lazy producer.
//! - [`share`]: one stream feeding every session on a shared gamescope screen.

mod capturer;
mod config;
mod consume;
mod ledger;
mod offers;
mod pacer;
mod plan;
mod queue;
mod share;
mod thread;

pub(crate) use capturer::PortalCapturer;
pub(super) use consume::realtime_minus_monotonic_ns;
pub(in crate::linux) use queue::RenderFence;
pub(super) use queue::RENDER_GUARD;
use queue::{FrameQueue, Taken};

use super::pw_cursor::CursorState;
use super::sync_timeline::SyncDevice;
use super::{CapturedFrame, PixelFormat};
use ledger::{PoolCensus, PoolLedger};
use pacer::Pacer;
use pipewire as pw;
use plan::{ImportPolicy, ImportState, PassthroughFallbacks};
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use std::sync::mpsc::SyncSender;

/// Named bools: four adjacent same-typed args transpose silently and
/// negotiate the wrong pod family (black screen).
#[derive(Clone)]
struct CaptureOpts {
    /// `false` forces CPU mmap even when `PUNKTFUNK_ZEROCOPY` is set — the
    /// session plan does that when 4:4:4 has no zero-copy convert (`SessionPlan::output_format`).
    allow_zerocopy: bool,
    /// Tiled dmabufs convert via `ImportKind::Tiled444`, not NV12/RGB.
    want_444: bool,
    /// Offer only 10-bit PQ/BT.2020 as LINEAR dmabufs. SHM cannot: Mutter's
    /// SHM path paints 8-bit ARGB32, and the tiled EGL blit is 8-bit.
    want_hdr: bool,
    /// 10-bit SDR: keep packed RGB (skip the NV12 convert) so direct-NVENC widens 8→10. A
    /// planar 8-bit surface fails a 10-bit NVENC session; packed RGB is the only 8-bit input
    /// it accepts there.
    ten_bit_sdr: bool,
    /// The producer offers 10-bit SDR (gamescope from `+pfhdr26`): its P010 and packed 10-bit
    /// pods go out under BT.709 ahead of the 8-bit ones, which stay as the fallback.
    sdr10_native: bool,
    /// Range of the NV12/P010 pods, PQ included, and of the importer's YUV converts; `false` is
    /// studio.
    full_range: bool,
    /// Skip buffers until negotiated size matches `preferred` — KWin virtual
    /// outputs birth a sacrificial mode then renegotiate (`kwin.rs` `create`).
    /// `false` elsewhere: Mutter sizes from negotiation; gamescope fixates.
    expect_exact_dims: bool,
    /// `true` (KWin): `id == 0` means pointer hidden — producer rewrites
    /// `SPA_META_Cursor` every buffer. `false` (Mutter): buffers recycle
    /// the region. See [`CursorState::id0_hides`].
    cursor_id0_hides: bool,
    /// Gamescope omits cursor metadata. Its proved tiled formats lead; LINEAR remains fallback.
    producer_is_gamescope: bool,
    /// Least dmabuf pool depth to ask for: [`crate::POOL_MIN`], or
    /// [`crate::KWIN_POOL_MIN`] so KWin's default of 3 cannot win.
    pool_min: i32,
    /// Deepest pool the producer serves ([`crate::KWIN_POOL_MAX`]); `None` serves any depth.
    /// A deeper ask for the raw lane stops here.
    pool_max: Option<i32>,
    /// Offer `maxFramerate = 0/1` so KWin records on its own frame signal
    /// rather than a millisecond-rounded timer. KWin only; see
    /// [`Knobs::kwin_paced`](crate::knobs::Knobs::kwin_paced).
    unpaced: bool,
    /// Drive the producer as a PipeWire lazy driver when it emits RequestProcess
    /// (Mutter ≥ 49 virtual monitors): it then paints when it asks, one paint per
    /// wire interval at most. See [`Knobs::lazy_capture`](crate::knobs::Knobs::lazy_capture).
    lazy: bool,
    /// The wire rate ([`crate::VirtualOutputOpts::stream_hz`]); `0` = unknown.
    stream_hz: u32,
    /// Resolved at open; the thread reads the rest of its switches here.
    knobs: crate::knobs::Knobs,
}

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
    /// Every capturer this stream feeds; `queue` and `wake` are its opener's.
    viewers: std::sync::Arc<share::Viewers>,
    shared: super::StreamShared,
    cursor_slot: super::CursorSlot,
    importer: super::ImporterCell,
    /// Raw dmabuf to the encoder instead of a CUDA import (VAAPI).
    vaapi_passthrough: bool,
    /// The session's encoder is the VA lane on the host's render node, not PyroWave.
    va_import: bool,
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
    /// Every pool buffer's state; a published one rejoins the pool only once encode lets go.
    ledger: std::sync::Arc<PoolLedger>,
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
    /// Queue `frame` behind `fence`, the render its pixels wait on, then a wakeup edge: for
    /// the opener alone, or for every viewer once the stream is shared.
    ///
    /// Must not block: this runs inside `.process` and would stall the compositor. A full
    /// wakeup channel already has a pending edge; the queue is the truth.
    fn publish(&self, frame: CapturedFrame, fence: Option<queue::RenderFence>) {
        if self.viewers.fanned() {
            return self
                .viewers
                .publish((&self.queue, &self.wake), frame, fence);
        }
        if let Ok(mut q) = self.queue.lock() {
            q.push(frame, fence, std::time::Instant::now());
        }
        let _ = self.wake.try_send(());
    }
}
