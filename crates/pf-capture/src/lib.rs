//! Linux xdg-ScreenCast/PipeWire and Windows IDD direct-push capturers, plus
//! synthetic test sources and the [`Capturer`] trait.
//!
//! Speaks [`pf_frame`] and the display leaves. Encode-backend facts arrive
//! pre-resolved in [`ZeroCopyPolicy`]; Windows sealed-section delivery arrives
//! as [`SetEncodeSender`] / [`CursorChannelSender`] closures. Never `pf-encode`
//! or the host orchestrator.
//!
//! Evidence: `design/idd-push-security.md`, `packaging/gamescope`.

use anyhow::Result;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

/// Least zero-copy pool depth asked of a compositor. 2 is what every producer already
/// serves; the negotiated depth minus the hold reserve is the deferred-requeue budget.
pub const POOL_MIN: i32 = 2;
/// KWin ≥ 6.2 offers `Range(3, 2, 4)` as a driver stream, so its default 3 wins any
/// intersection that contains it, and a pool of 3 spares one hold while the encoder keeps
/// two frames in flight. A minimum of 4 is the only ask that moves it.
pub const KWIN_POOL_MIN: i32 = 4;
/// The deepest pool KWin serves. A minimum above it fails negotiation outright
/// (`error alloc buffers: Invalid argument`).
pub const KWIN_POOL_MAX: i32 = 4;

/// Whether to ask a KWin output for delivery on its own frame signal.
///
/// KWin schedules each screencast frame on a QTimer whose wait it rounds *up* to a whole
/// millisecond, so an 8.333 ms frame is scheduled at 9 and the cadence jitters against the
/// real refresh. A ceiling well above the stream rate keeps that gate below one refresh, so
/// every real frame passes; without one, a game far above the stream rate and cursor-only
/// records take a pool buffer each, and KWin drops a frame outright when none is free.
/// The ceiling is `KWIN_UNPACED_HEADROOM` times the rate, or none when the rate is unknown.
/// `PUNKTFUNK_KWIN_PACED=1` asks for the stream rate itself.
pub fn unpaced_capture() -> bool {
    !pf_host_config::row_bool("PUNKTFUNK_KWIN_PACED")
}

/// Offer PipeWire explicit sync (`SPA_META_SyncTimeline`) on the dmabuf lane.
///
/// A producer that takes it hands over a fence at each buffer's acquire point and waits on
/// the release point this side signals, instead of finishing the GPU itself. KWin on NVIDIA
/// `glFinish()`es its compositor thread per cast frame otherwise — ~9 ms under a game's
/// load, the whole 120 → 111 fps gap. `PUNKTFUNK_EXPLICIT_SYNC=0` keeps the implicit path.
pub fn explicit_sync() -> bool {
    pf_host_config::env_on("PUNKTFUNK_EXPLICIT_SYNC").unwrap_or(true)
}

/// Whether to capture a compositor's output directly with `ext-image-copy-capture-v1`
/// instead of going through the xdg ScreenCast portal.
///
/// The portal is a second clock in the path: xdg-desktop-portal-hyprland re-requests each
/// frame on a millisecond timer with a 6 ms floor, which halves the rate at 165 Hz and
/// adds ~3 ms to every frame's age. Capturing the protocol ourselves removes both.
/// `PUNKTFUNK_DIRECT_CAPTURE=0` keeps the portal.
#[cfg(target_os = "linux")]
pub fn direct_capture() -> bool {
    pf_host_config::row_bool("PUNKTFUNK_DIRECT_CAPTURE")
}

/// Whether a virtual output may be driven as a PipeWire lazy driver.
///
/// A producer that emits RequestProcess (Mutter ≥ 49 virtual monitors) paints only in a
/// graph cycle the consumer starts, so the encode loop's slot is the one tick: no
/// compositor timer to beat against, no throttle to lose frames in, no extra render.
/// Producers without it are never driven. `PUNKTFUNK_LAZY_CAPTURE=0` restores the
/// producer-driven stream.
#[cfg(target_os = "linux")]
pub fn lazy_capture() -> bool {
    pf_host_config::row_bool("PUNKTFUNK_LAZY_CAPTURE")
}

/// A FATAL capture fault: retrying `try_latest` cannot help — the caller must rebuild the
/// capture attachment or fail the session. Carried inside the `anyhow::Error` a capture call
/// returns (downcast to route on it), so it can never collapse into an ordinary `Ok(None)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureFault {
    /// A known-ACTIVE display (input/cursor moving) delivered no new source frame through the
    /// stale floor and the staged recovery ladder (immunity plan WP13) — its terminal verdict;
    /// `secs` is the source gap at that point.
    SourceStalled { secs: u32 },
}

impl std::fmt::Display for CaptureFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceStalled { secs } => write!(
                f,
                "no source frame for {secs}s on a known-active display, through a rebuild"
            ),
        }
    }
}

impl std::error::Error for CaptureFault {}

/// The capturer's live health for the operator surface (immunity plan WP18): the classifier's
/// last verdict, the ring's self-report, and the last recovery episode. Plain data, no I/O —
/// the management layer maps it into its wire shape. Names are the classifier's own, lowercased.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureHealth {
    /// `healthy` / `idle` / `suspect` / `stalled` / `recovering` / `rebuilding` / `secure_desktop`.
    pub class: &'static str,
    /// The stall class when `class == "stalled"`: `worker` / `encoder` / `presentation` /
    /// `driver`.
    pub stall_class: Option<&'static str>,
    /// Time since the last real source frame.
    pub source_gap: std::time::Duration,
    /// The evidence the verdict rests on: `input` / `canary`.
    pub evidence: Option<&'static str>,
    /// The newest access unit's OS present stamp against the moment the host took it.
    pub present_to_arrival: Option<std::time::Duration>,
    /// `present_to_arrival` is past the classifier's bound: frames arrive late rather than not
    /// at all. A reported degradation — no recovery rung fires on it.
    pub late_frames: bool,
    /// The driver encoder's own state word: `closed` / `open` / `encoding` / `wedged`.
    /// `None` until the first `SET_ENCODE`.
    pub encoder_state: Option<&'static str>,
    /// The backend the driver opened (`nvenc` / `amf` / `qsv` / `pyrowave`); `None` as above.
    pub backend_opened: Option<&'static str>,
    /// Encode threads the driver abandoned after a wedge. Two opens the driver cycle.
    pub detached: u32,
    /// Access units the driver published, and frames it dropped at the encode pool.
    pub published_total: u64,
    pub dropped_total: u64,
    /// Frames the drain worker handed the pool — DWM's compose count, the source clock.
    pub source_seq: u64,
    /// The recovery stage running now, if an episode is open.
    pub current_stage: Option<&'static str>,
    /// The last closed episode.
    pub last_episode: Option<CaptureEpisode>,
    /// Stalled verdicts refused for budget or cooldown since the last episode.
    pub episodes_suppressed: u32,
    /// Time left in the post-failure cooldown.
    pub cooldown_remaining: Option<std::time::Duration>,
}

/// One closed recovery episode, as [`CaptureHealth`] reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureEpisode {
    pub stall_class: &'static str,
    pub recovered: bool,
    pub took: std::time::Duration,
    /// `(stage, outcome, took)` in ladder order; `outcome` is `applied` / `failed` /
    /// `unsupported` / `timed_out`.
    pub stages: Vec<(&'static str, &'static str, std::time::Duration)>,
    pub consecutive_failures: u32,
    pub cooldown: std::time::Duration,
}
// The Linux capturer reaches `DmabufFrame` through `super::`; `CursorOverlay` it names directly as
// `pf_frame::CursorOverlay`, so only `DmabufFrame` needs to sit in this crate root's scope.
#[cfg(target_os = "linux")]
use pf_frame::DmabufFrame;

/// Context on a capture loss whose display is still up (the import side broke, not the
/// compositor): the host may re-attach a capturer to the same output instead of creating
/// another one — on KWin every create is a new virtual output, and a burst of them wedges it.
#[derive(Debug, Clone, Copy)]
pub struct DisplayStillAlive;

impl std::fmt::Display for DisplayStillAlive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the output itself is still up")
    }
}

/// Produces frames without blocking the compositor. The Linux portal publishes
/// into a one-deep overwriting slot (drop-oldest): a stalled consumer still
/// sees the freshest frame.
/// A grid the host asks a request-driven producer to paint on: one grid point and the
/// period, in [`mono_ns`] time. The producer paints at `anchor + k × period`, one paint per
/// wire interval, where its cadence would otherwise free-run off its last paint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaintGrid {
    pub anchor_ns: i64,
    pub period_ns: i64,
}

static MONO_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Nanoseconds since a process-wide instant: the one clock the host loop and the capture
/// threads share for a [`PaintGrid`]. Negative before the epoch's first read, never after.
pub fn mono_ns(t: std::time::Instant) -> i64 {
    let epoch = *MONO_EPOCH.get_or_init(std::time::Instant::now);
    match t.checked_duration_since(epoch) {
        Some(d) => d.as_nanos() as i64,
        None => -(epoch.duration_since(t).as_nanos() as i64),
    }
}

/// The instant [`mono_ns`] maps to `ns`.
pub fn mono_instant(ns: i64) -> std::time::Instant {
    let epoch = *MONO_EPOCH.get_or_init(std::time::Instant::now);
    if ns >= 0 {
        epoch + std::time::Duration::from_nanos(ns as u64)
    } else {
        epoch - std::time::Duration::from_nanos(ns.unsigned_abs())
    }
}

pub trait Capturer: Send {
    fn next_frame(&mut self) -> Result<CapturedFrame>;

    /// Hand the virtual output's keepalive back so a capture-only rebuild can re-attach to the
    /// live output. `None` when this capturer holds none, or already gave it up; the output is
    /// then released when the capturer drops, as before.
    fn take_keepalive(&mut self) -> Option<Box<dyn Send>> {
        None
    }

    /// [`next_frame`](Self::next_frame) with a caller-chosen first-frame budget.
    /// A PipeWire stream can sit in `Streaming` with no buffer; retry shortens
    /// the first wait instead of blocking the default. Backends without an
    /// internal wait budget ignore it (the default delegates).
    fn next_frame_within(&mut self, _budget: std::time::Duration) -> Result<CapturedFrame> {
        self.next_frame()
    }

    /// [`next_frame_within`](Self::next_frame_within) whose expiry is retry, not
    /// a capture verdict. Must not latch process-wide HDR/dmabuf-only downgrades:
    /// a cold start can outlive the short window and still accept every offer.
    /// Backends that latch nothing from a timeout just delegate.
    fn next_frame_within_provisional(
        &mut self,
        budget: std::time::Duration,
    ) -> Result<CapturedFrame> {
        self.next_frame_within(budget)
    }

    /// Non-blocking: the freshest frame since the last call, or `None` so the
    /// caller reuses its last frame and holds a steady output rate. Default
    /// produces a frame each call; the portal drains without blocking.
    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.next_frame().map(Some)
    }

    /// Whether [`wait_arrival`](Self::wait_arrival) is usable. `false` (default)
    /// keeps the encode loop on its fixed-cadence tick.
    fn supports_arrival_wait(&self) -> bool {
        false
    }

    /// Block until a frame is ready for [`try_latest`](Self::try_latest) or
    /// `deadline` passes. Must not consume the frame. Only called when
    /// [`supports_arrival_wait`](Self::supports_arrival_wait) is `true`;
    /// errors surface at the following `try_latest`.
    fn wait_arrival(&mut self, _deadline: std::time::Instant) {}

    /// Gate expensive per-frame work so the capturer can stay alive between
    /// streams. The portal skips the de-pad copy while inactive and flushes
    /// its frame mailbox on `false`. `&mut self`: the mailbox flush cannot share.
    fn set_active(&mut self, _active: bool) {}

    /// Whether this capturer can still produce frames. A pool must consult this
    /// before reuse: zero-copy poison, a dead PipeWire thread, or a source that
    /// never returns to `Streaming` makes every later call fail. Default `true`.
    fn is_alive(&self) -> bool {
        true
    }

    /// Live cursor out-of-band from frames (Windows IddCx hardware-cursor
    /// channel). Preferred over `CapturedFrame::cursor`: pointer-only moves
    /// produce no frame, so a frame-attached overlay goes stale. Default `None`.
    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        None
    }

    /// Cursor-render flip: `true` keeps the pointer out of the video (client
    /// draws it); `false` puts it back in. A declared IddCx hardware cursor is
    /// irrevocable — DWM cannot take the job back. On Linux either call means
    /// the host places [`cursor`](Self::cursor), so a CPU copy stops baking it.
    /// Default no-op.
    fn set_cursor_forward(&mut self, _on: bool) {}

    /// Attach a gamescope cursor source. gamescope paints no `SPA_META_Cursor`,
    /// so [`cursor`](Self::cursor) stays empty unless the portal reads nested
    /// Xwaylands (one per `--xwayland-count`) over X11. Called once, after build.
    #[cfg(target_os = "linux")]
    fn attach_gamescope_cursor(&mut self, _targets: GamescopeCursorTargets) {}

    /// Static HDR mastering metadata (SMPTE ST.2086 + CLL) when the capturer can
    /// read it (Windows `IDXGIOutput6::GetDesc1`), or a generic HDR10 block once
    /// an HDR stream is negotiated (Linux exposes no real mastering volume).
    /// Forwarded to the encoder (SEI) and the client (`0xCE`). May change if regraded.
    fn hdr_meta(&self) -> Option<pf_frame::HdrMeta> {
        None
    }

    /// How many frames the encode loop may keep in flight before it blocks.
    /// `1` (default) is capture → submit → poll-blocks. `>1` overlaps convert
    /// of N+1 with encode of N when each frame has a fresh output texture.
    fn pipeline_depth(&self) -> usize {
        1
    }

    // `capture_target_id` and `resize_output` are one operation split in half:
    // `Some` from the id promises resize works; resize without the id cannot
    // check the reconfigured display is still this capturer's. Both defaults decline.

    /// OS display-target id this capturer is bound to (Windows IDD-push). Resize
    /// uses it to verify the reconfigured display is still this one. In-place
    /// resize keeps the target; a re-arrival fallback mints a new one. `None`
    /// = no such identity.
    fn capture_target_id(&self) -> Option<u32> {
        None
    }

    /// Host-initiated output resize after the session handler has committed the
    /// new mode. Resize the capture surface now: no descriptor-poll debounce,
    /// no teardown. `true` handled; `false` rebuilds.
    fn resize_output(&mut self, _width: u32, _height: u32) -> bool {
        false
    }

    /// Make the OS present to this display again at the current mode. An
    /// exclusive-topology eviction leaves the target active but unpresented, so the
    /// descriptor never changes and the two-strike debounce never trips. `true`
    /// handled; `false` unrecoverable.
    fn restart_presentation_in_place(&mut self) -> bool {
        false
    }

    /// A staged-recovery episode closed on new source frames since the last
    /// call: the measured local outage, from the last source frame before the
    /// stall to the frame that proved recovery. The stream loop forces an IDR
    /// and announces the gap. `None` = nothing recovered.
    fn take_recovered_outage(&mut self) -> Option<std::time::Duration> {
        None
    }

    /// Since the last call, the frame the encoder last read may be torn (a
    /// producer re-sent a buffer this side still held). The stream loop
    /// answers with one IDR. Default: never.
    fn take_reference_risk(&mut self) -> bool {
        false
    }

    /// Live capture health for the operator surface (WP18). `None` = this
    /// capturer does not classify (Linux portal, synthetic sources).
    fn health(&self) -> Option<CaptureHealth> {
        None
    }

    /// The session encoder's own clocks (`Encoder::telemetry`), handed over
    /// once per loop tick before `try_latest` so the supervisor classifies the
    /// encode leg on them. Default: ignored.
    fn observe_encoder(&mut self, _t: Option<pf_frame::health::EncoderTelemetry>) {}

    /// The grid a request-driven producer should paint on, or `None` to free-run. Only a
    /// producer that paints on this capturer's requests can follow it; the rest ignore it.
    fn set_paint_grid(&mut self, _grid: Option<PaintGrid>) {}

    /// A recovery rung whose actuator the stream loop owns because the
    /// encoder does (`EncoderReset`) or the display manager does
    /// (`DriverCycle`). The loop runs it and answers with [`Self::stage_done`].
    /// Default: never.
    fn take_pending_stage(&mut self) -> Option<pf_frame::recovery::Stage> {
        None
    }

    /// The loop-owned actuator for `stage` finished with `outcome`.
    fn stage_done(
        &mut self,
        _stage: pf_frame::recovery::Stage,
        _outcome: pf_frame::recovery::StageOutcome,
    ) {
    }

    /// The monitor and WUDFHost an in-driver encoder opens against
    /// ([`open_driver_encoder`]). `None` = not an IDD-push source.
    #[cfg(target_os = "windows")]
    fn driver_endpoint(&self) -> Option<DriverEndpoint> {
        None
    }
}

/// Deterministic moving BGRx test pattern: a sweeping bar plus an animated
/// gradient so every pixel changes.
pub struct SyntheticCapturer {
    width: u32,
    height: u32,
    fps: u32,
    frame_idx: u64,
    buf: Vec<u8>,
}

impl SyntheticCapturer {
    const BPP: usize = 4; // BGRx

    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        assert!(width > 0 && height > 0 && fps > 0);
        let buf = vec![0u8; width as usize * height as usize * Self::BPP];
        SyntheticCapturer {
            width,
            height,
            fps,
            frame_idx: 0,
            buf,
        }
    }
}

impl Capturer for SyntheticCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let w = self.width as usize;
        let h = self.height as usize;
        let bpp = Self::BPP;
        let t = self.frame_idx;
        // Vertical bar sweeps left→right once every ~2 s (`fps * 2`).
        let bar_x = ((t * w as u64) / (self.fps as u64 * 2)) % w as u64;
        let phase = (t % 256) as usize;
        for y in 0..h {
            let row = y * w * bpp;
            for x in 0..w {
                let i = row + x * bpp;
                let on_bar = (x as u64).abs_diff(bar_x) < 8;
                // BGRx: [B, G, R, x]
                self.buf[i] = if on_bar {
                    255
                } else {
                    ((x + phase) & 0xff) as u8
                };
                self.buf[i + 1] = if on_bar {
                    255
                } else {
                    ((y + phase) & 0xff) as u8
                };
                self.buf[i + 2] = if on_bar { 255 } else { ((x + y) & 0xff) as u8 };
                self.buf[i + 3] = 0;
            }
        }
        let pts_ns = self.frame_idx * 1_000_000_000 / self.fps as u64;
        self.frame_idx += 1;
        Ok(CapturedFrame {
            provenance: Default::default(),
            width: self.width,
            height: self.height,
            pts_ns,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(self.buf.clone()),
            cursor: None,
        })
    }
}

/// Cheap moving BGRx test pattern: whole-buffer `fill`s, real-time at 5K.
pub struct FastSyntheticCapturer {
    width: u32,
    height: u32,
    frame_idx: u64,
    buf: Vec<u8>,
    /// `PUNKTFUNK_SYNTH_NOISE`: high-entropy noise NVENC cannot compress, so
    /// the encoder hits its CBR target. Default flat/band compresses to ~nothing.
    noise: bool,
    rng: u64,
}

impl FastSyntheticCapturer {
    pub fn new(width: u32, height: u32) -> Self {
        assert!(width > 0 && height > 0);
        FastSyntheticCapturer {
            width,
            height,
            frame_idx: 0,
            buf: vec![0u8; width as usize * height as usize * 4],
            noise: std::env::var_os("PUNKTFUNK_SYNTH_NOISE").is_some(),
            rng: 0x9e3779b97f4a7c15,
        }
    }
}

impl Capturer for FastSyntheticCapturer {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        if self.noise {
            // Reseed from the frame index so consecutive frames share no
            // structure — large P-frames, not just the keyframe.
            let mut s = self
                .rng
                .wrapping_add(self.frame_idx.wrapping_mul(0x2545F491_4F6CDD1D))
                | 1;
            for c in self.buf.chunks_exact_mut(8) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                c.copy_from_slice(&s.to_le_bytes());
            }
            self.rng = s;
        } else {
            let (w, h) = (self.width as usize, self.height as usize);
            let row = w * 4;
            let shade = (self.frame_idx % 256) as u8;
            self.buf.fill(shade);
            let band_h = (h / 20).max(1);
            let band_y = (self.frame_idx as usize * 6) % h;
            for y in band_y..(band_y + band_h).min(h) {
                self.buf[y * row..(y + 1) * row].fill(0xff);
            }
        }
        self.frame_idx += 1;
        Ok(CapturedFrame {
            provenance: Default::default(),
            width: self.width,
            height: self.height,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(self.buf.clone()),
            cursor: None,
        })
    }
}

/// Encode-backend facts the Linux zero-copy negotiation needs, resolved once
/// by the host facade and passed in so capture never calls back into encode.
#[cfg(target_os = "linux")]
#[derive(Clone, Default)]
pub struct ZeroCopyPolicy {
    /// VAAPI (AMD/Intel): hand raw dmabufs through instead of the EGL→CUDA
    /// import (`encode::linux_zero_copy_is_vaapi`).
    pub backend_is_vaapi: bool,
    /// GPU-resident frames (everything but software). Phrases the CPU-fallback
    /// warning (`encode::resolved_backend_is_gpu`).
    pub backend_is_gpu: bool,
    /// This session encodes PyroWave: the wavelet encoder's Vulkan device
    /// imports raw dmabufs on any vendor, so take raw-dmabuf passthrough.
    /// Per-session, unlike `backend_is_vaapi`.
    pub pyrowave_session: bool,
    /// Encoder can ingest producer-native NV12 (Linux raw Vulkan Video on
    /// H265/AV1 — `pf_encode::linux_native_nv12_ok`). Every other arm takes
    /// packed RGB; H264/GameStream/PyroWave must never see NV12.
    pub native_nv12_session: bool,
    /// Encoder can ingest packed 10-bit PQ CUDA (`pf_encode::linux_hdr_cuda_ok`,
    /// direct-SDK NVENC only). No other arm reads those 2:10:10:10 words as
    /// anything but garbage, so do not produce them unless this holds.
    pub hdr_cuda_ok: bool,
    /// The NVENC encoder takes held dmabufs and lets its zero-copy worker convert them
    /// straight into its input slots (`pf_encode::linux_nvenc_raw_dmabuf_ok`). The capture
    /// then imports nothing; a producer that cannot be held keeps the import path.
    pub nvenc_raw_dmabuf: bool,
    /// The gamescope producer fixates a tiled modifier (`pf_vdisplay::gamescope_tiled_capture`).
    /// Off, its offer stays LINEAR-only.
    pub gamescope_tiled: bool,
    /// Per-fourcc modifiers the session's direct encoder import proved. Empty keeps LINEAR.
    pub encoder_modifiers: Vec<(u32, Vec<u64>)>,
}

/// Discovers gamescope's nested Xwayland cursor targets — `(DISPLAY, XAUTHORITY)`,
/// one per `--xwayland-count` — for [`Capturer::attach_gamescope_cursor`].
///
/// A closure, re-run on a slow cadence: gamescope creates a second Xwayland for
/// the game but advertises only the first in any child's environ, so a one-shot
/// snapshot taken before launch never sees the game display.
///
/// Built by the host facade (`pf_vdisplay::gamescope_xwayland_cursor_targets`)
/// so the capture→host edge stays one-way.
#[cfg(target_os = "linux")]
pub type GamescopeCursorTargets =
    std::sync::Arc<dyn Fn() -> Vec<(String, Option<String>)> + Send + Sync>;

#[cfg(target_os = "linux")]
pub fn capturer_supports_444(_encoder_ingests_rgb_444: bool) -> bool {
    true
}

/// Whether a native-plane capturer (compositor virtual output) can deliver HDR
/// (10-bit PQ/BT.2020) on this platform alone — the platform half of the
/// handshake's 10-bit gate, without knowing which compositor will be driven.
///
/// Linux is `false`: Mutter `RecordVirtual` and KWin/wlroots advertise 8-bit
/// BGRx/BGRA. gamescope can be 10-bit with the carried `pipewire-hdr` patch;
/// the host resolves that in `capture::capturer_supports_hdr_for`. The GNOME
/// portal monitor mirror (`open_portal_monitor` + `want_hdr`) is a separate gate.
#[cfg(target_os = "linux")]
pub fn capturer_supports_hdr() -> bool {
    false
}
/// Windows: IDD-push enables advanced colour and delivers P010/Rgb10a2.
#[cfg(target_os = "windows")]
pub fn capturer_supports_hdr() -> bool {
    true
}
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capturer_supports_hdr() -> bool {
    false
}

/// Which HDR capture source a `want_hdr` negotiation failure belongs to.
/// The latch is per source so a portal-monitor failure cannot disable the
/// virtual-output path, and vice versa, until host restart.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HdrSource {
    /// GNOME 50+ portal monitor mirror (`open_portal_monitor` with `want_hdr`).
    PortalMonitor,
    /// Compositor virtual output (`open_virtual_output` with `want_hdr`) —
    /// gamescope's PipeWire node with the carried `pipewire-hdr` patch.
    VirtualOutput,
}

/// Per-source latch: `want_hdr` failed to negotiate the 10-bit PQ offer.
/// Later sessions fall back to SDR instead of re-running the 10 s timeout.
/// `PortalMonitor` sticks until host restart. `VirtualOutput` lasts until a
/// gamescope display is torn down ([`clear_virtual_output_hdr_latch`]).
#[cfg(target_os = "linux")]
static HDR_CAPTURE_FAILED: [std::sync::atomic::AtomicBool; 2] = [
    std::sync::atomic::AtomicBool::new(false),
    std::sync::atomic::AtomicBool::new(false),
];

#[cfg(target_os = "linux")]
impl HdrSource {
    fn slot(self) -> usize {
        match self {
            HdrSource::PortalMonitor => 0,
            HdrSource::VirtualOutput => 1,
        }
    }
}

#[cfg(target_os = "linux")]
pub fn hdr_capture_failed(source: HdrSource) -> bool {
    HDR_CAPTURE_FAILED[source.slot()].load(std::sync::atomic::Ordering::Relaxed)
}

/// Latches SDR for `source`. Public so pf-vdisplay's teardown test can arm it.
#[cfg(target_os = "linux")]
pub fn note_hdr_capture_failed(source: HdrSource) {
    if !HDR_CAPTURE_FAILED[source.slot()].swap(true, std::sync::atomic::Ordering::Relaxed) {
        match source {
            HdrSource::PortalMonitor => tracing::warn!(
                "HDR capture negotiation failed on the monitor mirror — this host will offer SDR \
                 for that source for the rest of the process lifetime (restart the host after \
                 fixing the monitor's HDR mode to retry)"
            ),
            HdrSource::VirtualOutput => tracing::warn!(
                "HDR capture negotiation failed on the virtual output — this host will offer SDR \
                 for gamescope until that display is torn down (is the spawned gamescope the \
                 punktfunk build? see packaging/gamescope)"
            ),
        }
    }
}

/// Re-arms gamescope HDR: each spawn is a new compositor. The registry calls this when a
/// gamescope display is torn down. The portal latch has no such event and stays.
#[cfg(target_os = "linux")]
pub fn clear_virtual_output_hdr_latch() {
    HDR_CAPTURE_FAILED[HdrSource::VirtualOutput.slot()]
        .store(false, std::sync::atomic::Ordering::Relaxed);
}
#[cfg(target_os = "windows")]
pub fn capturer_supports_444(encoder_ingests_rgb_444: bool) -> bool {
    // IDD-push is full-chroma RGB (BGRA SDR, Rgb10a2 HDR). Only a backend that
    // CSCs RGB to 4:4:4 itself can use that (direct-NVENC). Both depths are
    // full-chroma, so Welcome chroma is real regardless of HDR.
    encoder_ingests_rgb_444
}
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capturer_supports_444(_encoder_ingests_rgb_444: bool) -> bool {
    false
}

/// v5 hardware-cursor channel (`IOCTL_SET_CURSOR_CHANNEL`). Host-built so this
/// crate never reaches the orchestrator; on IOCTL success the driver owns the
/// handle duplicated into WUDFHost. `Some` opts in: the capturer creates
/// the cursor section only when the host hands a sender; a plain session
/// keeps DWM's pointer.
#[cfg(target_os = "windows")]
pub type CursorChannelSender = std::sync::Arc<
    dyn Fn(&pf_driver_proto::control::SetCursorChannelRequest) -> Result<()> + Send + Sync,
>;

/// Mid-stream cursor-render flip (`IOCTL_SET_CURSOR_FORWARD`). `true` declares
/// the IddCx hardware cursor; `false` stands it down (host also forces the
/// same-mode re-commit that actualises the OS software cursor). UAC/Winlogon
/// render only through software cursor.
#[cfg(target_os = "windows")]
pub type CursorForwardSender = std::sync::Arc<dyn Fn(bool) -> Result<()> + Send + Sync>;

/// v7 in-driver encode open (`IOCTL_SET_ENCODE`) — same facade contract as
/// [`CursorChannelSender`]: the driver adopts the request's handle values iff the
/// IOCTL succeeds. Once per encoder generation.
#[cfg(target_os = "windows")]
pub type SetEncodeSender = std::sync::Arc<
    dyn Fn(
            &pf_driver_proto::encode::SetEncodeRequest,
        ) -> Result<pf_driver_proto::encode::SetEncodeReply>
        + Send
        + Sync,
>;

/// v7 one-shot encoder control (`IOCTL_ENCODE_CTL`): the `Encoder` calls the
/// stream loop makes, forwarded by the driver proxy.
#[cfg(target_os = "windows")]
pub type EncodeCtlSender =
    std::sync::Arc<dyn Fn(&pf_driver_proto::encode::EncodeCtlRequest) -> Result<()> + Send + Sync>;

/// Where an in-driver encoder is opened: the monitor's driver target and the WUDFHost the AU
/// section is duplicated into ([`Capturer::driver_endpoint`]).
#[cfg(target_os = "windows")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DriverEndpoint {
    pub target_id: u32,
    pub wudf_pid: u32,
}

// One-time PipeWire library init, shared by video (portal) and audio capture.
#[cfg(target_os = "linux")]
pub mod pwinit;

// Which clock the wire's `pts_ns` comes from. Linux-only consumer; arithmetic
// is platform-independent so tests run everywhere.
#[cfg(any(target_os = "linux", test))]
mod pts_provenance;

#[cfg(target_os = "windows")]
#[path = "windows/dxgi.rs"]
pub mod dxgi;
#[cfg(target_os = "windows")]
#[path = "windows/idd_push.rs"]
mod idd_push;
// WUDFHost duplication target — open with the shared rights mask, then prove the image path.
// Reused by the host gamepad-channel bootstrap (`inject::windows::gamepad_raii`); re-export so
// that reach stays a leaf.
#[cfg(target_os = "windows")]
pub use idd_push::{open_wudfhost, verify_is_wudfhost};
// The AU section's reader half. Pure over a mapped view, so its tests run on every target.
#[path = "windows/au_reader.rs"]
mod au_reader;
// The recovery classifier's cursor-damage witness. Pure over `(now, kicked, position)` for the
// same reason: the rule that decides whether a silent desktop resets gets tests on every target.
#[path = "windows/cursor_witness.rs"]
mod cursor_witness;
#[cfg(target_os = "windows")]
pub use idd_push::driver_encode::{open_driver_encoder, DriverEncodeOpenError, DriverEncodeParams};
#[cfg(target_os = "linux")]
#[path = "linux/mod.rs"]
mod linux;
/// ScreenCast handshake bounds and cursor-mode negotiation, shared with pf-vdisplay.
/// They run on `pf_portal`'s never-dropped runtime.
#[cfg(target_os = "linux")]
#[path = "linux/portal_rt.rs"]
pub mod portal_rt;
// GNOME BT.2100 colour-mode probe — host gate for offering HDR on the portal
// monitor path (`open_portal_monitor` `want_hdr`).
#[cfg(target_os = "linux")]
pub use linux::gnome_hdr_monitor_active;
#[cfg(target_os = "windows")]
#[path = "windows/synthetic_nv12.rs"]
pub mod synthetic_nv12;

/// Linux xdg-ScreenCast portal capturer for a client-sized monitor. `anchored`
/// inherits a RemoteDesktop grant headlessly. Pass `want_hdr` only when the
/// mirrored monitor is in HDR mode, or the 10 s negotiation latches SDR.
/// Pass `want_metadata_cursor` only when encode composites `CapturedFrame::cursor`;
/// otherwise the portal embeds the pointer so it is never silently lost.
#[cfg(target_os = "linux")]
pub fn open_portal_monitor(
    anchored: bool,
    want_hdr: bool,
    want_metadata_cursor: bool,
    policy: ZeroCopyPolicy,
) -> Result<Box<dyn Capturer>> {
    linux::PortalCapturer::open(
        anchored,
        want_hdr && !hdr_capture_failed(HdrSource::PortalMonitor),
        want_metadata_cursor,
        policy,
    )
    .map(|c| Box::new(c) as Box<dyn Capturer>)
}

/// The compositor behind a virtual output's PipeWire node. Node ids and remote fds do not
/// name it, so the host does.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Producer {
    /// KWin: rewrites `SPA_META_Cursor` every buffer (`id == 0` hides the pointer), serves
    /// pools of [`KWIN_POOL_MIN`] to [`KWIN_POOL_MAX`], and records unpaced while
    /// [`unpaced_capture`] holds.
    Kwin,
    /// gamescope: no cursor metadata, a gated tiled modifier offer, and the only HDR producer.
    Gamescope,
    /// Mutter and every other producer.
    #[default]
    Other,
}

/// What [`open_virtual_output`] negotiates. Named fields: adjacent bools transpose silently
/// and negotiate the wrong pod family (black screen).
#[cfg(target_os = "linux")]
#[derive(Clone, Default)]
pub struct VirtualOutputOpts {
    /// `false` forces CPU mmap even when `PUNKTFUNK_ZEROCOPY` is set.
    pub allow_zerocopy: bool,
    /// Tiled dmabufs convert to planar YUV444.
    pub want_444: bool,
    /// Offer 10-bit PQ/BT.2020. Holds on a [`Producer::Gamescope`] node only: every other
    /// virtual output is SDR, and a desktop that refuses the offer would latch gamescope's SDR.
    pub want_hdr: bool,
    /// 10-bit SDR: keep packed RGB so direct NVENC widens 8→10.
    pub ten_bit_sdr: bool,
    /// Skip buffers until the negotiated size matches the preferred mode (KWin's
    /// sacrificial birth mode).
    pub expect_exact_dims: bool,
    pub producer: Producer,
    pub policy: ZeroCopyPolicy,
}

/// Linux capturer for an existing virtual output's PipeWire node. `keepalive` owns the output.
#[cfg(target_os = "linux")]
pub fn open_virtual_output(
    remote_fd: Option<std::os::fd::OwnedFd>,
    node_id: u32,
    preferred_mode: Option<(u32, u32, u32)>,
    keepalive: Box<dyn Send>,
    opts: VirtualOutputOpts,
) -> Result<Box<dyn Capturer>> {
    let want_hdr = opts.want_hdr
        && opts.producer == Producer::Gamescope
        && !hdr_capture_failed(HdrSource::VirtualOutput);
    linux::PortalCapturer::from_virtual_output(
        remote_fd,
        node_id,
        preferred_mode,
        keepalive,
        VirtualOutputOpts { want_hdr, ..opts },
    )
    .map(|c| Box::new(c) as Box<dyn Capturer>)
}

/// Direct `ext-image-copy-capture-v1` capturer for a compositor output the host has
/// already created, named by its `wl_output.name`. The capturer owns `keepalive`.
///
/// Fails for every reason the caller should fall back to the portal: the compositor
/// lacks the protocol, the output is gone, or nothing it offers can be imported by this
/// session's encoder. The failure hands `keepalive` back for that fallback.
#[cfg(target_os = "linux")]
pub fn open_direct_output(
    output_name: String,
    keepalive: Box<dyn Send>,
    policy: ZeroCopyPolicy,
) -> std::result::Result<Box<dyn Capturer>, (anyhow::Error, Box<dyn Send>)> {
    linux::WlCapturer::open(output_name, keepalive, policy)
        .map(|c| Box::new(c) as Box<dyn Capturer>)
}

/// Windows IDD direct-push capturer on a pf-vdisplay target. `sender` delivers
/// the sealed frame channel. On failure `keepalive` is handed back so the
/// caller can retire the display.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
pub fn open_idd_push(
    target: pf_frame::dxgi::WinCaptureTarget,
    preferred: Option<(u32, u32, u32)>,
    want_hdr: bool,
    ten_bit_sdr: bool,
    want_444: bool,
    pyrowave: bool,
    keepalive: Box<dyn Send>,
    cursor_sender: Option<CursorChannelSender>,
    cursor_forward: Option<CursorForwardSender>,
    forwards_to_client: bool,
) -> std::result::Result<Box<dyn Capturer>, (anyhow::Error, Box<dyn Send>)> {
    idd_push::IddPushCapturer::open(
        target,
        preferred,
        want_hdr,
        ten_bit_sdr,
        want_444,
        pyrowave,
        keepalive,
        cursor_sender,
        cursor_forward,
        forwards_to_client,
    )
    .map(|c| Box::new(c) as Box<dyn Capturer>)
}
