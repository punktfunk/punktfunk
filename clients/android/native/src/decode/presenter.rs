//! The timeline presenter — Android's port of the Apple client's stage-4 deadline discipline
//! (`clients/apple/.../Stage2Pipeline.swift`, the `.deadline` pacing):
//!
//!  * a **newest-wins slot** (or a small smoothing FIFO, by user intent) between decode and
//!    release, so a burst coalesces in the app — as an explicit, counted drop — instead of
//!    queueing behind the display;
//!  * a **glass budget of one**: at most one undisplayed release in flight to SurfaceFlinger,
//!    reopened on the clock-predicted latch (with a 100 ms stale force-open as the liveness
//!    backstop, mirroring Apple's `PresentGate.staleAfter`), and bounded underneath by what
//!    `OnFrameRendered` actually confirmed reached glass ([`UNDISPLAYED_CAP`]) — because the
//!    prediction is only as good as the panel grid behind it, and 0.23.0 shipped a grid that
//!    could be wrong in one direction forever;
//!  * a **timed release**: `AMediaCodec_releaseOutputBufferAtTime` targeting the platform's own
//!    frame timeline (API 33+, via [`super::vsync`]), so the latch phase is deterministic instead
//!    of inheriting network + decode jitter. On the 31/32 fallback the release is ASAP —
//!    identical to the legacy path — and only the budget prediction uses the measured period.
//!  * under the SMOOTHNESS intent only, a **due time** per frame from
//!    [`punktfunk_core::phase::CadenceClock`]: the release targets the first timeline at or after
//!    the instant the SOURCE's own timestamp says the frame is due, rather than the first one
//!    after it happened to decode, so the host's cadence reaches glass instead of the network's
//!    (`design/presenter-cadence-rework.md` D1). The latency intent holds no clock at all and
//!    stays arrival-driven by construction.
//!
//! The legacy behaviour (release the newest ready buffer immediately, unbudgeted) remains
//! selectable at runtime: `adb shell setprop debug.punktfunk.presenter arrival` — the on-device
//! A/B needs no rebuild. The user-facing escape hatch stays the "Low-latency mode" master toggle
//! (off = the synchronous pre-overhaul loop, no presenter at all).

use ndk::media::media_codec::MediaCodec;
use punktfunk_core::phase::{CadenceClock, CadenceHealth, CadenceTuning};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use super::display::DisplayTracker;
use super::latency::{now_realtime_ns, p50_max_ms};
use super::vsync::VsyncShared;
use crate::sys::sysprop;

/// Submit-margin ahead of a timeline's EXPECTED PRESENT — SurfaceFlinger's own latch lead: the
/// released buffer must be in the BufferQueue by SF's wakeup for that vsync (a few ms before
/// present). A present closer than this is treated as missed and the next one is targeted. This
/// is deliberately NOT the timeline's `deadline` (which budgets for GPU rendering a video
/// buffer doesn't do — see `VsyncShared::next_target`); a too-tight gamble here presents one
/// vsync later, the exact cost the deadline gate paid on every frame.
///
/// 2.5 ms: SF's latch runs ~1-2 ms before present on modern devices (its `sfOffset`), and the
/// release itself is a binder call well under a ms. 4 ms measured latch p50 8-10; each ms cut
/// here is a ms off every frame's display stage. A device that misses at the live margin shows it
/// as a measured latch beyond one panel period (see the adaptation in
/// [`Presenter::flush_log`]) — that, not a drop counter, is the signal to widen.
const LATCH_MARGIN_NS: i64 = 2_500_000;

/// `debug.punktfunk.latch_margin_us` (0..=8000 µs): PIN the submit margin for a sweep —
/// setprop + stream restart, no rebuild. Unset/invalid = `None` = the adaptive default.
fn latch_margin_ns() -> Option<i64> {
    let us = sysprop(c"debug.punktfunk.latch_margin_us")?
        .parse::<i64>()
        .ok()?;
    (0..=8_000).contains(&us).then_some(us * 1_000)
}

/// The budget's liveness backstop: a release whose predicted latch never seems to arrive
/// (clock glitch, mode switch) force-reopens the budget this long after the release, counted in
/// `forced` — reads 0 on healthy systems (Apple's `PresentGate.staleAfter`, same value).
const STALE_REOPEN_NS: i64 = 100_000_000;

/// Releases still unconfirmed by `OnFrameRendered` at which the presenter stops handing
/// SurfaceFlinger more work.
///
/// The reopen above is a PREDICTION off the learned panel grid. A grid finer than the panel
/// (0.23.0 could pin one permanently — see [`punktfunk_core::phase::PanelGrid`]) reopens the
/// budget before the display has consumed anything, and the presenter then releases faster than
/// the panel scans: the BufferQueue fills, MediaCodec runs out of output buffers, the decoder
/// stalls, and the no-output backstop starts begging for keyframes. The render callback is the
/// ground truth about what actually reached glass, so it bounds the prediction.
///
/// Six, not one: the platform is explicitly allowed to deliver these callbacks BATCHED, and this
/// module's own `RENDERED_CAP` note records them trailing a release by a vsync or two — so a
/// healthy device sits at 1-3 outstanding and a tight cap would throttle it for nothing (a held
/// frame in the newest-wins slot is a DROPPED frame the moment a fresher one decodes). This is
/// not a pacing knob; it is the "something is structurally wrong" rail, and a presenter genuinely
/// out-running its display climbs past any fixed cap within a second. If a device's BufferQueue
/// is shallower than this the rail simply never engages and the no-output backstop handles it,
/// exactly as before — best-effort, never worse than not having it.
const UNDISPLAYED_CAP: i32 = 6;

/// Fallback latch-prediction period while the vsync clock is unmeasured/absent: one 120 Hz frame.
const FALLBACK_PERIOD_NS: i64 = 8_333_333;

/// The user's presentation intent — the Apple client's `PresentPriority`, same resolution rules:
/// anything but an explicit "smooth" is latency; a smooth buffer outside 1..=3 becomes 2.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum PresentPriority {
    /// Newest-wins, release the instant the budget opens. The default.
    Latency,
    /// A small holding store (1..=3 frames) drained on each frame's DUE time from the source's own
    /// timestamps: transport jitter absorbed in the cushion the cadence loop sizes from its own
    /// measured residual, at a latency the metrics show rather than hide.
    Smooth { buffer: usize },
}

impl PresentPriority {
    /// From the JNI ints (`presentPriority` 0 = latency / 1 = smooth; `smoothBuffer` 0 = auto).
    pub(crate) fn resolve(priority: i32, buffer: i32) -> PresentPriority {
        if priority != 1 {
            return PresentPriority::Latency;
        }
        let b = if (1..=3).contains(&buffer) {
            buffer as usize
        } else {
            2
        };
        PresentPriority::Smooth { buffer: b }
    }
}

/// One decoded output buffer held for presentation.
struct HeldFrame {
    index: usize,
    pts_us: u64,
    /// The output callback's `CLOCK_REALTIME` stamp — the pace metric's start (decoded→release).
    decoded_ns: i128,
    /// When the source says this frame is due, monotonic — [`CadenceClock::due_ns`] folded at
    /// submit. `None` under the latency intent, which has no clock.
    due_ns: Option<i64>,
}

/// The one-in-flight glass budget.
struct InFlight {
    /// Monotonic instant the budget reopens: the release target's expected present (clock), or
    /// `release + period` on the fallback path.
    reopen_at_ns: i64,
    released_at_ns: i64,
}

/// Latch samples + display confirms recorded by the `OnFrameRendered` callback thread, drained by
/// the presenter's 1 Hz `pf-present` line. Always on (independent of the HUD) — this is what makes
/// a HUD-off wireless A/B readable from logcat.
pub(super) struct PresentMeter {
    inner: Mutex<PresentMeterInner>,
    /// Frames released to SurfaceFlinger that `OnFrameRendered` has not yet confirmed reached
    /// glass. The presenter's structural rail (see [`UNDISPLAYED_CAP`]) and the pf-present line's
    /// queue-depth readout. Lock-free because the release side runs on the decode loop and the
    /// confirm side on the codec's callback thread, once per frame each.
    undisplayed: AtomicI32,
    /// This device delivers render callbacks at all (API ≥ 33 and the platform accepted the
    /// registration). Until one arrives, `undisplayed` is meaningless and the rail stays down.
    confirms: AtomicBool,
}

struct PresentMeterInner {
    latch_us: Vec<u64>,
    displays: u64,
    /// The `decode` stage's feed split, received→queued µs (P3 science: hand-off + input-slot
    /// wait). Empty when no receipt stamp matched (HUD off and ABR not measuring decode).
    feed_us: Vec<u64>,
    /// The codec-pure half, queued→decoded µs, measured from the AU's LAST piece — always on,
    /// so a HUD-off logcat A/B still reads the decoder's own time.
    codec_us: Vec<u64>,
    /// Capture→decoded end-to-end µs (skew-corrected, clamped) — always on for the same reason:
    /// the wireless A/B's headline without having to reach the on-screen HUD.
    e2e_us: Vec<u64>,
}

impl PresentMeter {
    pub(super) fn new() -> PresentMeter {
        PresentMeter {
            inner: Mutex::new(PresentMeterInner {
                latch_us: Vec::with_capacity(256),
                displays: 0,
                feed_us: Vec::with_capacity(256),
                codec_us: Vec::with_capacity(256),
                e2e_us: Vec::with_capacity(256),
            }),
            undisplayed: AtomicI32::new(0),
            confirms: AtomicBool::new(false),
        }
    }

    /// One displayed frame's release→displayed latch, µs. Callback thread; poison-proof.
    ///
    /// Also the glass budget's CONFIRM: this frame left the BufferQueue, so one outstanding
    /// release is settled. Clamped at zero — the legacy `arrival` path renders without going
    /// through [`Presenter::pump`], so confirms can outnumber counted releases.
    pub(super) fn note_latch(&self, latch_us: Option<u64>) {
        self.confirms.store(true, Ordering::Relaxed);
        let _ = self
            .undisplayed
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some((v - 1).max(0))
            });
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        g.displays += 1;
        if let Some(l) = latch_us {
            if g.latch_us.len() < 4096 {
                g.latch_us.push(l);
            }
        }
    }

    /// One frame handed to SurfaceFlinger, awaiting its confirm. Decode thread.
    fn note_released(&self) {
        self.undisplayed.fetch_add(1, Ordering::Relaxed);
    }

    /// Releases still unconfirmed, and whether confirms happen on this device at all.
    fn outstanding(&self) -> (i32, bool) {
        (
            self.undisplayed.load(Ordering::Relaxed),
            self.confirms.load(Ordering::Relaxed),
        )
    }

    /// Write off the outstanding releases: the platform stopped confirming (it is allowed to
    /// drop callbacks under load) or SurfaceFlinger discarded the buffers without presenting
    /// them. Never stall the stream on a ledger we cannot audit.
    fn forgive_outstanding(&self) {
        self.undisplayed.store(0, Ordering::Relaxed);
    }

    /// One decoded frame's always-on measurements: the `decode`-stage split (feed =
    /// received→queued when a receipt stamp matched; codec = queued→decoded when the queued
    /// stamp did) and the capture→decoded end-to-end, µs. Decode thread; poison-proof.
    pub(super) fn note_decode(
        &self,
        feed_us: Option<u64>,
        codec_us: Option<u64>,
        e2e_us: Option<u64>,
    ) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(f) = feed_us {
            if g.feed_us.len() < 4096 {
                g.feed_us.push(f);
            }
        }
        if let Some(c) = codec_us {
            if g.codec_us.len() < 4096 {
                g.codec_us.push(c);
            }
        }
        if let Some(e) = e2e_us {
            if g.e2e_us.len() < 4096 {
                g.e2e_us.push(e);
            }
        }
    }

    #[allow(clippy::type_complexity)] // one caller unpacks it in place; a struct would be noise
    fn drain(&self) -> (Vec<u64>, u64, Vec<u64>, Vec<u64>, Vec<u64>) {
        let mut g = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let displays = g.displays;
        g.displays = 0;
        (
            std::mem::take(&mut g.latch_us),
            displays,
            std::mem::take(&mut g.feed_us),
            std::mem::take(&mut g.codec_us),
            std::mem::take(&mut g.e2e_us),
        )
    }
}

/// The 1 Hz line's cadence-loop tail under the smoothness intent: `late‰` of all frames folded
/// (a due time already past when the frame became presentable — the direct signal the cushion
/// is too small), the loop residual's jitter, the cushion, and re-anchors. Empty under latency
/// (no loop). Counters are cumulative since the last re-anchor, so `late` reads as a rate.
pub(super) fn cadence_suffix(health: Option<CadenceHealth>) -> String {
    health
        .map(|h| {
            format!(
                " late={}‰ jitterMs={:.2} cushionMs={:.2} reanchors={}",
                h.late.saturating_mul(1000) / h.frames.max(1),
                h.jitter_ns as f64 / 1e6,
                h.cushion_ns as f64 / 1e6,
                h.reanchors,
            )
        })
        .unwrap_or_default()
}

pub(super) struct Presenter {
    /// 0 = newest-wins; 1..=3 = the holding store's capacity, a bound against a burst rather than
    /// a pacing depth (design §4.3).
    fifo_capacity: usize,
    frames: VecDeque<HeldFrame>,
    /// The source-cadence loop, and the ONLY thing that makes this presenter anything but
    /// arrival-driven. `None` under the latency intent — not "a clock we choose to ignore" but no
    /// clock at all, so that path cannot drift into cadence targeting by accident (`design/
    /// presenter-cadence-rework.md` §5; the test below is what holds it there).
    cadence: Option<CadenceClock>,
    /// The source's nominal frame interval — [`CadenceClock`]'s cushion ceiling, which is a design
    /// invariant rather than a tunable (a cushion past one whole frame buys smoothness the source
    /// cannot supply). From the negotiated stream rate, which cannot change without tearing this
    /// loop down.
    frame_interval_ns: i64,
    inflight: Option<InFlight>,
    /// A vsync arrived since the last release — the retry beat for a parked frame, and what the
    /// empty-store readout below is counted against.
    vsync_tick: bool,
    // -- 1 Hz pf-present window, always on --
    released: u64,
    paced_drops: u64,
    no_budget: u64,
    forced: u64,
    /// Vsync ticks that found the store empty. Under cadence targeting an empty store is the
    /// ordinary state between a frame's decode and its due time, so this reads as supply depth
    /// rather than as the underflow alarm it was under the retired per-slot drain — the alarm is
    /// the loop's own late count.
    dry: u64,
    /// Pump passes that held a frame back because too many earlier releases were still
    /// unconfirmed ([`UNDISPLAYED_CAP`]) — reads 0 on a healthy device, and a climbing value is
    /// the signature of a presenter out-running its display.
    queue_waits: u64,
    /// When the unconfirmed-release rail first engaged, so it can be forgiven if the confirms
    /// simply stopped coming. `None` while the rail is down.
    backed_up_since: Option<i64>,
    pace_us: Vec<u64>,
    last_flush: Instant,
    /// The live submit margin. Starts at 0 (P2e on-glass: SurfaceFlinger latched every
    /// release with NO lead on the NP3 — the fixed 2.5 ms was pure display latency) and
    /// widens by measurement: `paced` misses in a 1 Hz window push it +500 µs toward
    /// [`LATCH_MARGIN_NS`], the pre-sweep known-safe ceiling. A sysprop override PINS it.
    margin_ns: i64,
    /// `debug.punktfunk.latch_margin_us` was set — the margin is pinned, never adapted.
    margin_pinned: bool,
}

impl Presenter {
    /// `source_hz` is the NEGOTIATED STREAM rate, not the panel's — it sizes the cadence loop's
    /// cushion ceiling, and the quantity that must not be exceeded is one source frame. 0 (an
    /// unnegotiated rate) falls back to [`FALLBACK_PERIOD_NS`], the tighter of the two plausible
    /// answers: a ceiling set too low costs smoothness, one set too high costs latency the user
    /// never asked for.
    pub(super) fn new(priority: PresentPriority, source_hz: u32) -> Presenter {
        let pinned = latch_margin_ns();
        let (margin_ns, margin_pinned) = match pinned {
            Some(ns) => (ns, true),
            None => (0, false),
        };
        log::info!(
            "presenter: latch margin {}us{}",
            margin_ns / 1_000,
            if margin_pinned {
                " (debug.punktfunk.latch_margin_us pin)"
            } else {
                " (adaptive — widens on latch misses)"
            }
        );
        let frame_interval_ns = match source_hz {
            0 => FALLBACK_PERIOD_NS,
            hz => 1_000_000_000 / i64::from(hz),
        };
        // `snapping()`, because every release here goes through the frame-timeline grid: the
        // snap-up carries roughly half a refresh of implicit slack, so the cushion is small.
        let cadence = matches!(priority, PresentPriority::Smooth { .. }).then(|| {
            log::info!(
                "presenter: cadence clock on the source's timeline, frame interval {:.2}ms",
                frame_interval_ns as f64 / 1e6
            );
            CadenceClock::new(CadenceTuning::snapping())
        });
        Presenter {
            fifo_capacity: match priority {
                PresentPriority::Latency => 0,
                PresentPriority::Smooth { buffer } => buffer,
            },
            frames: VecDeque::new(),
            cadence,
            frame_interval_ns,
            inflight: None,
            vsync_tick: false,
            released: 0,
            paced_drops: 0,
            no_budget: 0,
            forced: 0,
            dry: 0,
            queue_waits: 0,
            backed_up_since: None,
            pace_us: Vec::with_capacity(256),
            last_flush: Instant::now(),
            margin_ns,
            margin_pinned,
        }
    }

    /// A vsync pulse from the clock thread's event — the retry tick for a frame parked on a closed
    /// budget, and the beat the empty-store readout is counted against. No longer a drain pace:
    /// under cadence targeting the due time is what releases a frame.
    pub(super) fn on_vsync(&mut self) {
        self.vsync_tick = true;
    }

    /// Accept one decoded, gate-approved output buffer. Newest-wins evicts everything older
    /// (released unrendered — the explicit, counted drop); the FIFO evicts its oldest past
    /// capacity. Returns how many frames were dropped by the policy (the HUD's `skipped`).
    ///
    /// `decoded_mono_ns` is the monotonic twin of `decoded_ns`, stamped at the same instant on the
    /// codec's callback thread: the cadence loop and `releaseOutputBufferAtTime` both live in
    /// `CLOCK_MONOTONIC`, while the latency stats live in `CLOCK_REALTIME`, and the loop is fed and
    /// read in ONE domain (`phase.rs`: a constant offset between domains is absorbed by the offset
    /// estimator, so no conversion belongs anywhere in this path).
    pub(super) fn submit(
        &mut self,
        codec: &MediaCodec,
        index: usize,
        pts_us: u64,
        decoded_ns: i128,
        decoded_mono_ns: i64,
    ) -> u64 {
        let due_ns = self.due_at(pts_us, decoded_mono_ns);
        let mut dropped = 0u64;
        if self.fifo_capacity == 0 {
            while let Some(stale) = self.frames.pop_front() {
                release_unrendered(codec, stale.index);
                dropped += 1;
            }
        }
        self.frames.push_back(HeldFrame {
            index,
            pts_us,
            decoded_ns,
            due_ns,
        });
        if self.fifo_capacity > 0 && self.frames.len() > self.fifo_capacity {
            if let Some(stale) = self.frames.pop_front() {
                release_unrendered(codec, stale.index);
                dropped += 1;
            }
        }
        self.paced_drops += dropped;
        dropped
    }

    /// When the source says this frame is due, or `None` under the latency intent.
    ///
    /// `pts_us` IS the host's own stamp — it round-trips through the codec's presentation time, so
    /// the source timeline needs no plumbing of its own. The µs the codec API quantises it to
    /// costs ±0.5 µs of white noise on an 8.3 ms period, and that passes through to the due time
    /// like any other variation in the source's cadence: the loop smooths the offset, never the
    /// timestamps.
    fn due_at(&mut self, pts_us: u64, decoded_mono_ns: i64) -> Option<i64> {
        let interval_ns = self.frame_interval_ns;
        self.cadence
            .as_mut()
            .map(|c| c.due_ns(pts_us.saturating_mul(1_000), decoded_mono_ns, interval_ns))
    }

    /// The earliest present a release may target: SurfaceFlinger's latch lead ahead of now, since
    /// a present it cannot latch in time is not a target, and never before the frame's own due
    /// time. That second half is the whole of the cadence change — `next_target(max(now, due))`
    /// where it used to be `next_target(now)` (design §4.2).
    fn not_before_ns(&self, now_mono_ns: i64, due_ns: Option<i64>) -> i64 {
        let submit_floor_ns = now_mono_ns + self.margin_ns;
        due_ns.map_or(submit_floor_ns, |due| due.max(submit_floor_ns))
    }

    /// Whether the frame at the head of the smoothing store may leave it yet.
    ///
    /// The store's job under cadence targeting is to HOLD WHAT IS NOT DUE YET (design §4.3): the
    /// due time paces, so capacity is a bound against a burst rather than the clock. A frame is
    /// releasable once the grid point it aims at is the next one this pump could still submit for
    /// — one `grid_period_ns` of reach past the submit margin. Sooner buys nothing, because the
    /// release is timed either way and holding keeps the store's own eviction policy live over the
    /// frame; later risks the loop's 5 ms housekeeping wake landing inside the submit lead, which
    /// costs the frame a whole refresh.
    fn head_is_releasable(&self, now_mono_ns: i64, grid_period_ns: i64) -> bool {
        let reach_ns = now_mono_ns + self.margin_ns + grid_period_ns;
        self.frames
            .front()
            .is_some_and(|f| f.due_ns.is_none_or(|due| due <= reach_ns))
    }

    /// Force the cadence loop to re-anchor on the next frame — the discontinuity hook the clock
    /// asks its callers for. The Android seam is the re-anchor gate arming: a loss freezes the
    /// gate, the decoder recovers behind it, and what reaches this store on the far side comes
    /// through a pipeline whose delay is no longer the one the loop measured. Source-timestamp
    /// regressions and half-second gaps the loop catches by itself. No-op under latency.
    pub(super) fn reset_cadence(&mut self) {
        if let Some(c) = self.cadence.as_mut() {
            c.reset();
        }
    }

    /// The cadence loop's health for the 1 Hz line, or `None` when there is no loop to read.
    fn cadence_health(&self) -> Option<CadenceHealth> {
        self.cadence.as_ref().map(CadenceClock::health)
    }

    /// The present decision point — run on every loop pass (frame arrivals, vsync ticks, and the
    /// 5 ms housekeeping wake all land here). Releases AT MOST one frame (the budget). Returns
    /// `true` when a frame was released to glass this call.
    #[allow(clippy::too_many_arguments)] // one call site; the seams are the point
    pub(super) fn pump(
        &mut self,
        codec: &MediaCodec,
        clock: Option<&VsyncShared>,
        tracker: &DisplayTracker,
        meter: &PresentMeter,
        now_mono_ns: i64,
    ) -> bool {
        // Budget bookkeeping first: reopen on the predicted latch, force-open on the backstop.
        if let Some(f) = &self.inflight {
            if now_mono_ns >= f.reopen_at_ns {
                self.inflight = None;
            } else if now_mono_ns - f.released_at_ns > STALE_REOPEN_NS {
                self.forced += 1;
                self.inflight = None;
            }
        }
        // The measured rail beneath that prediction (see `UNDISPLAYED_CAP`). Evaluated on every
        // pass — frame waiting or not — so its forgiveness timer measures real elapsed time
        // rather than how often a frame happened to be ready.
        let backlogged = self.unconfirmed_backlog(meter, now_mono_ns);
        // The grid a release snaps to, for the store's due-time reach below: the panel's own
        // period where it is known — the app's choreographer stream can be down-rated below it
        // (see `VsyncShared::next_target`) — and the measured callback period otherwise.
        let grid_period_ns = clock
            .map(|c| c.panel_period_ns().max(c.period_ns()))
            .filter(|&p| p > 0)
            .unwrap_or(FALLBACK_PERIOD_NS);
        // Pick the frame this pump may release.
        let frame = if self.fifo_capacity == 0 {
            self.frames.pop_back() // submit() kept it a single slot; back == the newest
        } else {
            // The smoothing store releases on the DUE time, so there is no drain tick to gate on
            // and no preroll beneath it. One-frame-per-slot was itself the defect: at 60 fps on a
            // 120 Hz panel it drains at twice supply, the store empties, preroll re-arms, and the
            // intervals become 1,3,1,3 where 2,2,2 is the correct answer — the smoothing mode
            // juddering by construction at exactly the rate mismatch it exists to smooth (design
            // §4.3, D3). Due times one source period apart snap to every second vblank instead.
            if !self.head_is_releasable(now_mono_ns, grid_period_ns) {
                if self.vsync_tick && self.frames.is_empty() {
                    self.dry += 1;
                }
                self.vsync_tick = false; // this tick's evaluation ran (and released nothing)
                return false;
            }
            self.frames.pop_front()
        };
        let Some(frame) = frame else { return false };
        if self.inflight.is_some() || backlogged {
            // Budget closed — park it back; a fresher submit replaces it (newest-wins), the next
            // vsync tick / loop pass retries the pairing.
            if backlogged {
                self.queue_waits += 1;
            }
            self.no_budget += 1;
            match self.fifo_capacity {
                0 => self.frames.push_back(frame),
                _ => self.frames.push_front(frame),
            }
            return false;
        }
        // Release: timeline-timed when the clock has one, ASAP otherwise. Under cadence targeting
        // the floor is the frame's due time rather than this instant — the source's grid rather
        // than the network's.
        let target =
            clock.and_then(|c| c.next_target(self.not_before_ns(now_mono_ns, frame.due_ns)));
        let released = match target {
            Some(t) => codec
                .release_output_buffer_at_time_by_index(frame.index, t.expected_present_ns)
                .map_err(|e| log::warn!("presenter: release_at_time({}): {e}", frame.index)),
            None => codec
                .release_output_buffer_by_index(frame.index, true)
                .map_err(|e| log::warn!("presenter: release({}): {e}", frame.index)),
        };
        self.vsync_tick = false;
        if released.is_err() {
            return false; // the buffer is gone either way; nothing to book-keep
        }
        let period = clock.map(|c| c.period_ns()).filter(|&p| p > 0);
        // Reopen at SurfaceFlinger's LATCH for the targeted vsync (expected present minus the
        // latch lead) — the instant SF consumes the queued buffer and the slot frees, so the
        // next release can target the NEXT refresh. Not the platform `deadline` (with the
        // aggressive present gate it can already be in the past — an instant reopen would let
        // two releases pile onto the same vsync) and not the present time itself (a period too
        // late — it would cap the sustainable rate at roughly half the panel).
        let reopen_at_ns = target
            .map(|t| t.expected_present_ns - self.margin_ns)
            .unwrap_or(now_mono_ns + period.unwrap_or(FALLBACK_PERIOD_NS));
        self.inflight = Some(InFlight {
            reopen_at_ns,
            released_at_ns: now_mono_ns,
        });
        self.released += 1;
        meter.note_released();
        let release_real_ns = now_realtime_ns();
        let pace_us = ((release_real_ns - frame.decoded_ns).max(0) / 1000) as u64;
        if self.pace_us.len() < 4096 {
            self.pace_us.push(pace_us);
        }
        tracker.note_rendered(frame.pts_us, frame.decoded_ns, release_real_ns);
        true
    }

    /// Whether SurfaceFlinger is sitting on too many unconfirmed releases to be handed another.
    ///
    /// The predicted reopen is only as good as the panel grid behind it; this is the measured
    /// rail underneath it (see [`UNDISPLAYED_CAP`]). It self-clears two ways — the confirms catch
    /// up, or [`STALE_REOPEN_NS`] passes with the backlog stuck, which means the ledger itself is
    /// unreliable (callbacks dropped under load, or SF discarded the buffers) and is written off
    /// rather than allowed to wedge the stream.
    fn unconfirmed_backlog(&mut self, meter: &PresentMeter, now_ns: i64) -> bool {
        let (outstanding, confirms_live) = meter.outstanding();
        if !confirms_live || outstanding < UNDISPLAYED_CAP {
            self.backed_up_since = None;
            return false;
        }
        match self.backed_up_since {
            Some(t) if now_ns - t > STALE_REOPEN_NS => {
                meter.forgive_outstanding();
                self.backed_up_since = None;
                self.forced += 1;
                false
            }
            _ => {
                self.backed_up_since.get_or_insert(now_ns);
                true
            }
        }
    }

    /// Release every held buffer unrendered — the teardown path, BEFORE `codec.stop()`.
    pub(super) fn release_all(&mut self, codec: &MediaCodec) {
        while let Some(f) = self.frames.pop_front() {
            release_unrendered(codec, f.index);
        }
        self.inflight = None;
    }

    /// The 1 Hz `pf-present` logcat mirror (target `pf.present`) — the Apple client's Console
    /// `pf-present` line, so a HUD-off on-device A/B is readable wirelessly:
    /// `released` (to glass) / `displays` (OnFrameRendered confirms) / `paced` (policy drops) /
    /// `noBudget` (waits on the closed budget) / `forced` (stale force-opens — 0 when healthy) /
    /// `qDry` (FIFO underflows) / `qWait` (pumps held back by unconfirmed releases — 0 when
    /// healthy) / `unconfirmed` (releases OnFrameRendered hasn't settled) /
    /// `pace` (decoded→release) / `latch` (release→displayed) /
    /// `feed`+`codec` (the decode stage split: received→queued hand-off/slot wait + the
    /// codec-pure queued→decoded time) / `e2e` (capture→decoded, skew-corrected — the wireless
    /// A/B headline) / `vsync` (the measured panel period).
    ///
    /// Under the smoothness intent it carries the cadence loop's health as well — `late‰` of all
    /// frames folded (a due time already past when the frame became presentable: the direct signal
    /// the cushion is too small, and WP8's acceptance criterion), `jitter` (the loop residual's
    /// mean absolute deviation, our first honest per-stream jitter number), `cushion` and
    /// `reanchors`. Absent under latency, where there is no loop.
    pub(super) fn flush_log(&mut self, meter: &PresentMeter, clock: Option<&VsyncShared>) {
        if self.last_flush.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        self.last_flush = Instant::now();
        let (latch, displays, feed, codec, e2e) = meter.drain();
        if self.released == 0 && displays == 0 {
            return; // idle stream — nothing worth a line
        }
        let (pace_p50, pace_max) = p50_max_ms(std::mem::take(&mut self.pace_us));
        let (feed_p50, feed_max) = p50_max_ms(feed);
        let (codec_p50, codec_max) = p50_max_ms(codec);
        let (e2e_p50, e2e_max) = p50_max_ms(e2e);
        let latch_samples = latch.len();
        let (latch_p50, latch_max) = p50_max_ms(latch);
        let period_ms = clock.map(|c| c.period_ns() as f64 / 1e6).unwrap_or(0.0);
        let panel_ns = clock.map(|c| c.panel_period_ns()).unwrap_or(0);
        let (outstanding, _) = meter.outstanding();
        // Cumulative over the session rather than this window (the loop's counters survive
        // `reset`): `late` is a RATE question, and one second of it is too few frames to read a
        // sub-percent criterion off.
        let cadence = cadence_suffix(self.cadence_health());
        log::info!(
            target: "pf.present",
            "released={} displays={} paced={} noBudget={} forced={} qDry={} \
             qWait={} unconfirmed={} \
             paceMs p50={:.2} max={:.2} latchMs p50={:.2} max={:.2} \
             feedMs p50={:.2} max={:.2} codecMs p50={:.2} max={:.2} \
             e2eMs p50={:.2} max={:.2} \
             vsyncMs={:.2} panelMs={:.2}{}",
            self.released,
            displays,
            self.paced_drops,
            self.no_budget,
            self.forced,
            self.dry,
            self.queue_waits,
            outstanding,
            pace_p50,
            pace_max,
            latch_p50,
            latch_max,
            feed_p50,
            feed_max,
            codec_p50,
            codec_max,
            e2e_p50,
            e2e_max,
            period_ms,
            panel_ns as f64 / 1e6,
            cadence,
        );
        self.released = 0;
        // Margin adaptation, off the MEASURED latch. A release targets the first grid point past
        // `now + margin`, so a frame that makes its vsync is on glass within one panel period of
        // that margin; beyond it, SurfaceFlinger wanted more lead and the frame waited out an
        // extra refresh. Widen toward the pre-sweep ceiling. One-way by design: a margin that
        // once proved necessary is never re-gambled mid-stream (the next stream restarts at 0).
        //
        // ⚠ NOT `paced_drops`, which 0.23.0 used: those are the newest-wins store's own policy
        // evictions — a second frame decoding while one is held — which happen whenever the
        // stream out-runs the panel and say nothing at all about SF's latch lead. Driving the
        // margin from them widened it to the ceiling on healthy devices, re-imposing the 2.5 ms
        // of pure display latency the P2e sweep had just measured away.
        let latch_p50_ns = (latch_p50 * 1e6) as i64;
        if !self.margin_pinned
            && self.margin_ns < LATCH_MARGIN_NS
            && panel_ns > 0
            && latch_samples >= 8
            && latch_p50_ns > panel_ns + self.margin_ns
        {
            self.margin_ns = (self.margin_ns + 500_000).min(LATCH_MARGIN_NS);
            log::warn!(
                "presenter: latch p50 {:.2}ms over the {:.2}ms panel period — margin widened to {}us",
                latch_p50,
                panel_ns as f64 / 1e6,
                self.margin_ns / 1_000
            );
        }
        if self.queue_waits > 0 {
            log::warn!(
                "presenter: {} pump(s) held back — {} release(s) still unconfirmed by \
                 OnFrameRendered (the display is not keeping up with the release rate)",
                self.queue_waits,
                outstanding
            );
        }
        self.paced_drops = 0;
        self.no_budget = 0;
        self.forced = 0;
        self.dry = 0;
        self.queue_waits = 0;
    }
}

fn release_unrendered(codec: &MediaCodec, index: usize) {
    if let Err(e) = codec.release_output_buffer_by_index(index, false) {
        log::warn!("presenter: release_output_buffer({index}, false): {e}");
    }
}

/// `debug.punktfunk.presenter` sysprop: `arrival` = the legacy release-immediately path,
/// anything else / unset = the timeline presenter. The rebuild-free on-device A/B lever.
pub(super) fn presenter_disabled_by_sysprop() -> bool {
    sysprop(c"debug.punktfunk.presenter").as_deref() == Some("arrival")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE_HZ: u32 = 120;
    const P: i64 = 8_333_333; // one 120 Hz source interval, ns
    /// A host stamping realtime, played out against a monotonic present clock — two different
    /// eras, which is the whole reason the loop estimates the offset rather than being told it.
    const PTS0_NS: i64 = 1_786_000_000_000_000_000;
    const MONO0: i64 = 987_000_000_000;
    const PTS0_US: u64 = (PTS0_NS / 1000) as u64;

    /// `k`'s pts as the CODEC echoes it — µs, truncated exactly as `feed_ready` queues it — and
    /// the monotonic instant it became presentable, with `jitter_ns` of transport noise on the
    /// arrival and none on the source stamp.
    fn frame(k: i64, jitter_ns: i64) -> (u64, i64) {
        (((PTS0_NS + k * P) / 1000) as u64, MONO0 + k * P + jitter_ns)
    }

    #[test]
    fn the_latency_intent_never_consults_the_cadence_clock() {
        let mut p = Presenter::new(PresentPriority::Latency, SOURCE_HZ);
        for k in 0..600 {
            let (pts_us, ready) = frame(k, (k % 11) * 400_000);
            assert_eq!(
                p.due_at(pts_us, ready),
                None,
                "latency must produce no due time at all"
            );
        }
        assert!(
            p.cadence_health().is_none(),
            "latency holds no loop to have health"
        );
        // …so the target floor is exactly what it was before the clock existed: this instant plus
        // SurfaceFlinger's latch lead, and nothing else.
        assert_eq!(p.not_before_ns(MONO0, None), MONO0 + p.margin_ns);
    }

    #[test]
    fn the_smooth_intent_puts_every_decoded_frame_through_the_loop() {
        let mut p = Presenter::new(PresentPriority::Smooth { buffer: 2 }, SOURCE_HZ);
        for k in 0..600 {
            let (pts_us, ready) = frame(k, (k % 11) * 400_000);
            assert!(p.due_at(pts_us, ready).is_some());
        }
        let h = p.cadence_health().expect("smooth holds a loop");
        assert_eq!(h.frames, 600);
        assert_eq!(
            h.reanchors, 1,
            "only the cold start anchors on a clean trace"
        );
    }

    #[test]
    fn a_due_time_ahead_of_now_moves_the_target_and_one_behind_it_does_not() {
        let p = Presenter::new(PresentPriority::Smooth { buffer: 2 }, SOURCE_HZ);
        let floor = MONO0 + p.margin_ns;
        // Due ahead: the release aims at the source's grid, which is the entire change.
        assert_eq!(p.not_before_ns(MONO0, Some(floor + P)), floor + P);
        // Due already past — a late frame, which the loop deliberately reports unclamped: present
        // at the next opportunity, never drag the grid back to the frame.
        assert_eq!(p.not_before_ns(MONO0, Some(floor - 5 * P)), floor);
    }

    #[test]
    fn the_store_holds_a_frame_that_is_not_due_yet_and_releases_it_within_reach_of_its_slot() {
        let mut p = Presenter::new(PresentPriority::Smooth { buffer: 2 }, SOURCE_HZ);
        let due = MONO0 + 4 * P;
        p.frames.push_back(HeldFrame {
            index: 0,
            pts_us: PTS0_US,
            decoded_ns: 0,
            due_ns: Some(due),
        });
        assert!(!p.head_is_releasable(MONO0, P), "four refreshes early");
        assert!(
            !p.head_is_releasable(due - P - p.margin_ns - 1, P),
            "one nanosecond outside the submit reach of its own slot"
        );
        assert!(
            p.head_is_releasable(due - P - p.margin_ns, P),
            "exactly one panel period of reach, the earliest that still buys nothing to wait"
        );
        assert!(
            p.head_is_releasable(due + 10 * P, P),
            "late frames go at once"
        );
    }

    #[test]
    fn a_frame_carrying_no_due_time_is_always_releasable() {
        // `HeldFrame` is shared with the latency intent, where the due time is always absent, so
        // the predicate has to drain on a frame it cannot answer for rather than wedge behind it.
        let mut p = Presenter::new(PresentPriority::Smooth { buffer: 2 }, SOURCE_HZ);
        p.frames.push_back(HeldFrame {
            index: 0,
            pts_us: PTS0_US,
            decoded_ns: 0,
            due_ns: None,
        });
        assert!(p.head_is_releasable(MONO0, P));
    }
}
