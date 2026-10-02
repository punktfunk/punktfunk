//! Android video decode (android-only): pull HEVC access units from the connector into NDK
//! `AMediaCodec` — hardware decode, zero per-frame JNI.
//!
//! The decoded frames reach glass through one of two present backends (see [`asc_presenter`] and
//! [`presenter`]). The default is the **ASurfaceControl** backend: the codec renders into an
//! `AImageReader` and each frame is composited onto an `ASurfaceControl` layer via a transaction
//! carrying a desired present time, scheduling against the panel's real present clock. The
//! **SurfaceView** presenter — `releaseOutputBufferAtTime` straight to the SurfaceView's window — is
//! the fallback for API < 29, ChromeOS, an ASC init failure, or the `present_backend=surfaceview`
//! sysprop.
//!
//! One-in/one-out: the host opens every stream with an IDR carrying VPS/SPS/PPS **in-band**, so the
//! decoder needs no out-of-band codec-specific data — we configure with mime + the negotiated
//! WxH (from [`NativeClient::mode`]) and feed each access unit as it arrives. The decode thread owns
//! the codec + surface for its whole life; [`crate::session`] signals it to stop via the shared flag.

mod asc_presenter;
mod async_loop;
mod display;
mod latency;
mod presenter;
mod setup;
mod surface_control;
mod vsync;

use async_loop::run_async;
pub(crate) use setup::{codec_label, codec_mime};
// Shared with the PyroWave lane, which exists only where the codec is built (see `crate::pyro`).
#[cfg(target_pointer_width = "64")]
pub(crate) use latency::now_realtime_ns;
#[cfg(target_pointer_width = "64")]
pub(crate) use setup::boost_thread_priority;

use crate::input_stall::NoOutput;
use ndk::native_window::NativeWindow;
use punktfunk_core::client::NativeClient;
use punktfunk_core::reanchor::ReanchorGate;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Cap on AUs parked in the async loop awaiting a free codec input slot. Matches the connector's
/// own frame-channel depth; on sustained overflow the oldest is dropped and a keyframe requested
/// (same recovery as a reassembler drop). In steady state this stays near-empty.
const FRAME_PARK_CAP: usize = 16;

/// Cap on the pts→received-timestamp map below: MediaCodec holds only a handful of frames in
/// flight, so anything beyond this is stale (codec flushed / HUD toggled) and gets evicted.
const IN_FLIGHT_CAP: usize = 64;

/// Cap on rendered frames parked in [`DisplayTracker`] awaiting their `OnFrameRendered` render
/// timestamp: the callback trails its release by at most a vsync or two, so anything this deep
/// means the platform stopped delivering render callbacks (allowed under load, per the docs) and
/// gets evicted.
const RENDERED_CAP: usize = 64;

/// How long a session may deliver NO access unit at all before we ask for a keyframe and say so.
/// [`NoOutput`] needs AUs to have arrived, so it cannot see a session that receives nothing:
/// connected, audio alive, a black surface. Under infinite GOP the ask fetches the IDR this
/// client never saw; when the host sends nothing, the log line beside it says so.
pub(crate) const NO_VIDEO_PATIENCE: std::time::Duration = std::time::Duration::from_millis(1500);

/// Re-ask cadence once [`NO_VIDEO_PATIENCE`] has elapsed with still nothing received. Slow, because
/// this state is either self-healing on the first ask or not ours to heal — and each pass logs.
///
/// ⚠ Taken from core, NOT a local number. `FLUSH_COOLDOWN` (the jump-to-live rate limit) is 2000 ms,
/// and the host classifies a keyframe-recovery cadence by matching a cooldown's period ±10 % to
/// decide WHICH client failure it is looking at. The two are opposites — "I have received nothing"
/// versus "I am drowning in frames I cannot drain" — so while this was also 2000 ms the host
/// confidently reported the wrong one, and a black-screen field case was diagnosed as a slow decoder
/// for days (2026-08-20). Keeping the value in core is what stops the two drifting back together.
const NO_VIDEO_RETRY: std::time::Duration = punktfunk_core::client::NO_VIDEO_RETRY;

/// The keyframe backstops the decode loop runs once a pass: a decoder that owes output
/// ([`NoOutput`]), a session that never received an AU ([`NO_VIDEO_PATIENCE`]), and the gate's
/// overdue-freeze re-ask. Every ask shares one 100 ms throttle so a multi-frame gap can't flood
/// the control stream.
pub(super) struct Backstops {
    last_kf_req: Option<Instant>,
    no_output: NoOutput,
    started: Instant,
    last_no_video_req: Option<Instant>,
}

impl Backstops {
    pub(super) fn new() -> Backstops {
        let now = Instant::now();
        Backstops {
            last_kf_req: None,
            no_output: NoOutput::new(now),
            started: now,
            last_no_video_req: None,
        }
    }

    /// One pass. `handled` = AUs fed to the codec or withheld from it; `had_output` = the decoder
    /// produced this pass; `au_parked` = an AU is waiting for an input slot; `losses` = drops this
    /// pass that are a loss in their own right (a parked-AU overflow), which arm the gate.
    /// `true` when the no-output window or the gate asked for a keyframe.
    pub(super) fn poll(
        &mut self,
        client: &NativeClient,
        gate: &mut ReanchorGate,
        handled: u64,
        had_output: bool,
        au_parked: bool,
        losses: u64,
    ) -> bool {
        let now = Instant::now();
        if losses > 0 {
            gate.arm(now);
        }
        // Arm the freeze too, so the concealment a re-anchoring decoder emits stays off the glass.
        let starved = self.no_output.poll(handled, had_output, now);
        if starved {
            gate.arm(now);
        }
        // Nothing has EVER arrived: the no-output window cannot see it.
        if handled == 0
            && !au_parked
            && now.duration_since(self.started) >= NO_VIDEO_PATIENCE
            && self
                .last_no_video_req
                .is_none_or(|t| now.duration_since(t) >= NO_VIDEO_RETRY)
        {
            log::warn!(
                "decode: no video received {} ms into the session — requesting a keyframe",
                now.duration_since(self.started).as_millis()
            );
            self.last_no_video_req = Some(now);
            let _ = client.request_keyframe();
            self.last_kf_req = Some(now); // share the throttle with the loss-recovery path below
        }
        let backstop = gate.poll(client.frames_dropped(), now) || starved;
        if backstop || losses > 0 {
            self.ask(client);
        }
        backstop
    }

    /// Ask for a keyframe through the shared 100 ms throttle.
    pub(super) fn ask(&mut self, client: &NativeClient) {
        let now = Instant::now();
        if self
            .last_kf_req
            .is_none_or(|t| now.duration_since(t) >= Duration::from_millis(100))
        {
            self.last_kf_req = Some(now);
            let _ = client.request_keyframe();
        }
    }
}

/// Per-session decode configuration, resolved by the JNI layer (`nativeStartVideo`) and passed to
/// the decode loop. Bundled so the loop entry points don't sprout a wide argument list.
pub(crate) struct DecodeOptions {
    /// The decoder Kotlin ranked from `MediaCodecList` (`VideoDecoders.pickDecoder`). `None`/empty ⇒
    /// let the platform resolve the default decoder for the MIME.
    pub decoder_name: Option<String>,
    /// Whether Kotlin found the chosen decoder advertises `FEATURE_LowLatency` (queryable only via
    /// the Java `CodecCapabilities` API) — surfaced on the HUD next to the decoder name.
    pub ll_feature: bool,
    /// The user's "Low-latency mode" master toggle. On (default) ⇒ the aggressive vendor keys,
    /// pipeline thread boosts, ADPF max-performance and the forced TV mode switch. Off ⇒ the same
    /// loop and presenter with plain keys and no boosts — the per-device escape hatch.
    pub low_latency_mode: bool,
    /// TV form factor (Kotlin's `UiModeManager`): actively drive the HDMI output into the stream's
    /// refresh mode, vs. the softer seamless hint on a phone/tablet.
    pub is_tv: bool,
    /// ChromeOS (ARC): present through the SurfaceView, never ASurfaceControl — see
    /// [`asc_presenter::asc_backend_selected`].
    pub chromeos: bool,
    /// The user's presentation intent (`present_priority` setting): 0 = lowest latency
    /// (newest-wins), 1 = smoothness (a small FIFO). Resolved by
    /// [`presenter::PresentPriority::resolve`]; anything else = latency.
    pub present_priority: i32,
    /// The smoothness buffer depth (`smooth_buffer` setting): 0 = automatic (2), else 1..=3.
    /// Only meaningful with `present_priority` = smooth.
    pub smooth_buffer: i32,
    /// SEED for the panel's refresh period — the latch grid the presenter subdivides onto when
    /// the app's choreographer stream is down-rated below the panel (see `vsync.rs`). Kotlin
    /// resolves it from the display mode TABLE (`MainActivity.streamPanelFps`), not
    /// `display.refreshRate`, which reports a per-uid override rather than the panel. 0 = unknown.
    ///
    /// ⚠ Only a seed: `preferredDisplayModeId` is a REQUEST the system may refuse, so the mode
    /// named here is not necessarily the one the panel ends up in. The measured timeline spacing
    /// corrects it in both directions ([`punktfunk_core::phase::PanelGrid`]).
    pub panel_hz: i32,
    /// The video `SurfaceView`'s LIVE on-screen pixel size (the aspect-fitted display footprint),
    /// packed by [`crate::session::pack_surface_size`] and re-reported by Kotlin on every
    /// `surfaceChanged`. The ASurfaceControl backend composites its layer in this coordinate space
    /// — NOT the window's buffer geometry, which is rotated/scaled. `0` = Kotlin couldn't read it
    /// yet, and the backend falls back to the window buffer size.
    pub surface_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// The visible part of the frame, packed by [`crate::session::pack_src_crop`]; `0` = all of it.
    pub src_crop: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Where the decoder publishes its picture size, packed by [`crate::session::pack_surface_size`].
    pub decoded_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// A dual-screen handheld's second picture window and the layers-shown mask. The
    /// ASurfaceControl backend alone reads it; the SurfaceView presenter and PyroWave have one
    /// surface and show the whole picture there.
    pub layers: std::sync::Arc<crate::session::PictureLayers>,
    /// Not the session's first video start: the stream is mid-flight, so the fresh decoder
    /// holds no reference picture the next P-frames lean on.
    pub restart: bool,
}

/// The decode entry point on the `pf-decode` thread: dispatches to the codec's loop. All of
/// them run until `shutdown` is set or the session closes.
///
/// PyroWave leaves before any of this: it is GPU compute with its own Vulkan present path
/// ([`crate::pyro`]), sharing no MediaCodec machinery — not the codec object, not the
/// surface handling, not the presenters. Everything below is the MediaCodec pipeline.
pub fn run(
    client: Arc<NativeClient>,
    window: NativeWindow,
    shutdown: Arc<AtomicBool>,
    stats: Arc<crate::stats::VideoStats>,
    opts: DecodeOptions,
) {
    if client.codec == punktfunk_core::quic::CODEC_PYROWAVE {
        crate::pyro::run(client, window, shutdown, stats, opts);
        return;
    }
    run_async(client, window, shutdown, stats, opts);
}
