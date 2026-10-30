//! The SurfaceView sink: decoded buffers go straight to the SurfaceView's window through
//! `releaseOutputBuffer[AtTime]`, and `OnFrameRendered` (`display.rs`) confirms what reached
//! glass.
//!
//! The timeline presenter, the Android port of the Apple client's stage-4 deadline discipline
//! (`Stage2Pipeline.swift`, `.deadline` pacing), holds frames in a [`Store`] and releases them
//! under a [`TimelineBudget`], each timed to the platform's own frame timeline (API 33+, via
//! [`VsyncShared::next_target`]) so the latch phase is deterministic instead of inheriting
//! network and decode jitter. On API 31/32 the release is ASAP and only the budget uses the
//! measured period.
//!
//! `adb shell setprop debug.punktfunk.presenter arrival` selects the arrival path instead: the
//! newest ready buffer releases at once, unbudgeted — the on-device A/B needs no rebuild. The
//! user-facing escape hatch stays the "Low-latency mode" toggle.

use ndk::data_space::DataSpace;
use ndk::media::media_codec::MediaCodec;
use ndk::native_window::NativeWindow;
use punktfunk_core::quic::ColorInfo;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::super::async_loop::{Ctx, OutputReady};
use super::super::display::{apply_dataspace, color_dataspace, stream_dataspace, DisplayTracker};
use super::super::latency::{now_realtime_ns, p50_max_ms};
use super::super::surface_control::{Layer, PresentComplete};
use super::super::vsync::VsyncShared;
use super::{Downrate, Routed, Sink};
use crate::present::budget::{Confirms, TimelineBudget};
use crate::present::store::{Held, Store};
use crate::present::verdict::{Second, Verdict, Verdicts};
use crate::present::{
    cadence_suffix, frame_interval_ns, Counters, PresentPriority, FALLBACK_PERIOD_NS,
};
use crate::sys::{now_monotonic_ns, sysprop};

/// `debug.punktfunk.latch_margin_us` (0..=8000 µs): PIN the submit margin for a sweep —
/// setprop + stream restart, no rebuild. Unset/invalid = `None` = the adaptive default.
fn latch_margin_ns() -> Option<i64> {
    let us = sysprop(c"debug.punktfunk.latch_margin_us")?
        .parse::<i64>()
        .ok()?;
    (0..=8_000).contains(&us).then_some(us * 1_000)
}

/// Latch samples + display confirms recorded by the `OnFrameRendered` callback thread, drained by
/// the presenter's 1 Hz `pf-present` line. Always on (independent of the HUD) — this is what makes
/// a HUD-off wireless A/B readable from logcat.
pub(crate) struct PresentMeter {
    inner: Mutex<PresentMeterInner>,
    /// Frames released to SurfaceFlinger that `OnFrameRendered` has not yet confirmed reached
    /// glass: the budget's structural rail and the pf-present line's queue-depth readout.
    /// Lock-free because the release side runs on the decode loop and the confirm side on the
    /// codec's callback thread, once per frame each.
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
    pub(crate) fn new() -> PresentMeter {
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
    /// release is settled and the budget may reopen. Clamped at zero — the arrival path renders
    /// without going through the timeline pump, so confirms can outnumber counted releases.
    pub(crate) fn note_latch(&self, latch_us: Option<u64>) {
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

    /// The confirm ledger now: releases still unconfirmed, and whether confirms happen on this
    /// device at all.
    fn outstanding(&self) -> Confirms {
        Confirms {
            unconfirmed: self.undisplayed.load(Ordering::Relaxed),
            live: self.confirms.load(Ordering::Relaxed),
        }
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
    pub(crate) fn note_decode(
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

/// One decoded output buffer held for release.
struct Output {
    index: usize,
    pts_us: u64,
    /// The output callback's `CLOCK_REALTIME` stamp — the pace metric's start (decoded→release).
    decoded_ns: i128,
}

/// The timeline presenter: the store, the budget, the down-rated rule, the verdicts and the 1 Hz
/// window.
struct Timeline {
    store: Store<Output>,
    budget: TimelineBudget,
    downrate: Downrate,
    downrated: bool,
    verdicts: Verdicts,
    counters: Counters,
    /// Decoded outputs, and whether an AU was lost, in this 1 Hz window.
    outputs: u64,
    lost: bool,
    pace_us: Vec<u64>,
    last_flush: Instant,
}

/// The grid a release snaps to: the panel's own period where it is known (the app's
/// choreographer stream can be down-rated below it), the callback period otherwise; 0 = neither.
fn grid_period_ns(clock: Option<&VsyncShared>) -> i64 {
    clock.map_or(0, |c| c.panel_period_ns().max(c.period_ns()))
}

impl Timeline {
    /// `source_hz` is the negotiated stream rate, the cadence cushion's ceiling
    /// ([`frame_interval_ns`]); `panel_hz` the mode-table rate Kotlin asked for.
    fn new(priority: PresentPriority, source_hz: u32, panel_hz: i32) -> Timeline {
        let budget = TimelineBudget::new(latch_margin_ns());
        log::info!(
            "presenter: latch margin {}us{}",
            budget.margin_ns() / 1_000,
            if budget.margin_pinned() {
                " (debug.punktfunk.latch_margin_us pin)"
            } else {
                " (adaptive — widens on latch misses)"
            }
        );
        let interval_ns = frame_interval_ns(source_hz);
        log::info!(
            "presenter: cadence clock on the source's timeline, frame interval {:.2}ms",
            interval_ns as f64 / 1e6
        );
        Timeline {
            store: Store::new(priority, interval_ns, super::cushion_tail()),
            budget,
            downrate: Downrate::new(panel_hz),
            downrated: false,
            verdicts: Verdicts::default(),
            counters: Counters::default(),
            outputs: 0,
            lost: false,
            pace_us: Vec::with_capacity(256),
            last_flush: Instant::now(),
        }
    }

    /// Hold one decoded, gate-approved output buffer; the store's evictions are released
    /// unrendered. Returns how many it dropped (the HUD's `skipped`). `decoded_mono_ns` is the
    /// monotonic twin of `decoded_ns`, stamped at the same instant on the codec's thread: the
    /// cadence loop and `releaseOutputBufferAtTime` live in `CLOCK_MONOTONIC`.
    fn submit(
        &mut self,
        codec: &MediaCodec,
        index: usize,
        pts_us: u64,
        decoded_ns: i128,
        decoded_mono_ns: i64,
    ) -> u64 {
        let frame = Output {
            index,
            pts_us,
            decoded_ns,
        };
        self.store.push(
            frame,
            pts_us,
            decoded_mono_ns,
            &mut self.counters,
            |stale| release_unrendered(codec, stale.index),
        )
    }

    /// The present decision point, run on every loop pass (frame arrivals, vsync ticks and the
    /// 5 ms housekeeping wake). Releases at most one frame: timeline-timed when the clock has
    /// one, ASAP otherwise. On a down-rated grid the budget stays open and the confirm ledger
    /// is not kept, since SurfaceFlinger drops superseded buffers unconfirmed. Returns `true`
    /// when a frame was released to glass.
    fn pump(
        &mut self,
        codec: &MediaCodec,
        clock: Option<&VsyncShared>,
        tracker: &DisplayTracker,
        meter: &PresentMeter,
        now_mono_ns: i64,
    ) -> bool {
        let grid = grid_period_ns(clock);
        self.downrated = self.downrate.follow(grid);
        let grid_period_ns = if grid > 0 { grid } else { FALLBACK_PERIOD_NS };
        let pick = self.budget.pick(
            &mut self.store,
            now_mono_ns,
            grid_period_ns,
            meter.outstanding(),
            self.downrated,
            &mut self.counters,
        );
        if pick.forgive {
            meter.forgive_outstanding();
        }
        let Some(Held { frame, due_ns }) = pick.frame else {
            return false;
        };
        // Under cadence targeting the floor is the frame's due time: the source's grid rather
        // than the network's.
        let target =
            clock.and_then(|c| c.next_target(self.budget.not_before_ns(now_mono_ns, due_ns)));
        let released = match target {
            Some(t) => codec
                .release_output_buffer_at_time_by_index(frame.index, t.expected_present_ns)
                .map_err(|e| log::warn!("presenter: release_at_time({}): {e}", frame.index)),
            None => codec
                .release_output_buffer_by_index(frame.index, true)
                .map_err(|e| log::warn!("presenter: release({}): {e}", frame.index)),
        };
        if released.is_err() {
            return false; // the buffer is gone either way; nothing to book-keep
        }
        let period = clock.map(|c| c.period_ns()).filter(|&p| p > 0);
        self.budget.released(
            now_mono_ns,
            target.map(|t| t.expected_present_ns),
            period,
            &mut self.counters,
        );
        if !self.downrated {
            meter.note_released();
        }
        let release_real_ns = now_realtime_ns();
        let pace_us = ((release_real_ns - frame.decoded_ns).max(0) / 1000) as u64;
        if self.pace_us.len() < 4096 {
            self.pace_us.push(pace_us);
        }
        tracker.note_rendered(frame.pts_us, frame.decoded_ns, release_real_ns);
        true
    }

    /// Release every held buffer unrendered — the teardown path, BEFORE `codec.stop()`.
    fn release_all(&mut self, codec: &MediaCodec) {
        for f in self.store.drain() {
            release_unrendered(codec, f.index);
        }
        self.budget.clear();
    }

    /// The 1 Hz `pf.present` line: `released` (to glass) / `displays` (OnFrameRendered confirms)
    /// / `paced` (store drops) / `noBudget` (waits on the closed budget) / `forced` (stale
    /// force-opens) / `qDry` (empty-store ticks) / `qWait` (pumps the unconfirmed rail held) /
    /// `unconfirmed` / `pace` (decoded→release) / `latch` (release→displayed) / `feed`+`codec`
    /// (received→queued, queued→decoded) / `e2e` (capture→decoded, skew-corrected) / `vsync`
    /// and `panel` periods / `confirmReopen` (budgets a confirm reopened before the prediction).
    /// `forced` and `qWait` read 0 on a healthy device.
    ///
    /// It carries the cadence loop's health ([`cadence_suffix`]). It also feeds
    /// the second's latch p50 to [`TimelineBudget::adapt_margin`] and the second to the
    /// [`Verdicts`], whose verdict it returns; `lost` folds into the window. An idle second
    /// logs nothing and keeps its counters for the next.
    fn flush_log(
        &mut self,
        meter: &PresentMeter,
        clock: Option<&VsyncShared>,
        lost: bool,
    ) -> Option<Verdict> {
        self.lost |= lost;
        if self.last_flush.elapsed() < Duration::from_secs(1) {
            return None;
        }
        self.last_flush = Instant::now();
        let (latch, displays, feed, codec, e2e) = meter.drain();
        let c = &mut self.counters;
        let second = Second {
            released: c.released,
            displays,
            skipped: c.skipped,
            outputs: std::mem::take(&mut self.outputs),
            confirms_live: meter.outstanding().live,
            unbroken: !std::mem::take(&mut self.lost),
        };
        let verdict = self.verdicts.second(second, grid_period_ns(clock));
        if c.released == 0 && displays == 0 {
            return verdict; // idle stream — nothing worth a line
        }
        let (pace_p50, pace_max) = p50_max_ms(std::mem::take(&mut self.pace_us));
        let (feed_p50, feed_max) = p50_max_ms(feed);
        let (codec_p50, codec_max) = p50_max_ms(codec);
        let (e2e_p50, e2e_max) = p50_max_ms(e2e);
        let latch_samples = latch.len();
        let (latch_p50, latch_max) = p50_max_ms(latch);
        let period_ms = clock.map(|c| c.period_ns() as f64 / 1e6).unwrap_or(0.0);
        let panel_ns = clock.map(|c| c.panel_period_ns()).unwrap_or(0);
        let outstanding = meter.outstanding().unconfirmed;
        // Session-cumulative (the loop's counters survive `reset`): `late` is a rate, and one
        // second is too few frames to read a sub-percent criterion off.
        let cadence = cadence_suffix(self.store.cadence_health());
        log::info!(
            target: "pf.present",
            "released={} displays={} paced={} noBudget={} forced={} qDry={} \
             qWait={} unconfirmed={} \
             paceMs p50={:.2} max={:.2} latchMs p50={:.2} max={:.2} \
             feedMs p50={:.2} max={:.2} codecMs p50={:.2} max={:.2} \
             e2eMs p50={:.2} max={:.2} \
             vsyncMs={:.2} panelMs={:.2} confirmReopen={}{}",
            c.released,
            displays,
            c.skipped,
            c.no_budget,
            c.forced,
            c.dry,
            c.queue_waits,
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
            c.confirmed,
            cadence,
        );
        c.released = 0;
        if self
            .budget
            .adapt_margin((latch_p50 * 1e6) as i64, latch_samples, panel_ns)
        {
            log::warn!(
                "presenter: latch p50 {:.2}ms over the {:.2}ms panel period — margin widened to {}us",
                latch_p50,
                panel_ns as f64 / 1e6,
                self.budget.margin_ns() / 1_000
            );
        }
        if c.queue_waits > 0 {
            log::warn!(
                "presenter: {} pump(s) held back — {} release(s) still unconfirmed by \
                 OnFrameRendered (the display is not keeping up with the release rate)",
                c.queue_waits,
                outstanding
            );
        }
        c.skipped = 0;
        c.no_budget = 0;
        c.forced = 0;
        c.dry = 0;
        c.queue_waits = 0;
        c.confirmed = 0;
        verdict
    }
}

fn release_unrendered(codec: &MediaCodec, index: usize) {
    if let Err(e) = codec.release_output_buffer_by_index(index, false) {
        log::warn!("presenter: release_output_buffer({index}, false): {e}");
    }
}

/// The arrival path: only the pass's newest output renders, at once, and only when core shows
/// it; every other output is released unrendered.
fn arrival(ctx: &Ctx, ready: Vec<OutputReady>, shows: &mut dyn FnMut(u64) -> bool) -> Routed {
    let mut r = Routed::default();
    let last = ready.len().saturating_sub(1);
    for (i, o) in ready.into_iter().enumerate() {
        let present = shows(o.pts_us);
        let render = i == last && present;
        match ctx.codec.release_output_buffer_by_index(o.index, render) {
            Ok(()) if render => {
                r.rendered += 1;
                ctx.tracker
                    .note_rendered(o.pts_us, o.decoded_ns, now_realtime_ns());
            }
            Ok(()) => {
                r.discarded += 1;
                r.skipped += 1;
            }
            Err(e) => log::warn!(
                "decode: release_output_buffer_by_index({}, {render}): {e}",
                o.index
            ),
        }
    }
    r
}

/// The SurfaceView sink: the timeline presenter, or the arrival path when `timeline` is `None`.
pub(super) struct SurfaceViewSink {
    /// The dataspace the window was last tagged with.
    applied_ds: Option<DataSpace>,
    timeline: Option<Timeline>,
}

impl SurfaceViewSink {
    /// The timeline presenter at `priority`; `source_hz` is the negotiated stream rate,
    /// `panel_hz` the mode-table rate Kotlin asked for.
    pub(super) fn timeline(
        window: &NativeWindow,
        color: &ColorInfo,
        priority: PresentPriority,
        source_hz: u32,
        panel_hz: i32,
    ) -> SurfaceViewSink {
        let timeline = Timeline::new(priority, source_hz, panel_hz);
        SurfaceViewSink::tagged(window, color, Some(timeline))
    }

    /// The arrival path.
    pub(super) fn arrival(window: &NativeWindow, color: &ColorInfo) -> SurfaceViewSink {
        SurfaceViewSink::tagged(window, color, None)
    }

    /// The window takes the negotiated colour before the first frame; a decoder's own report
    /// refines it later, and many decoders never send one.
    fn tagged(
        window: &NativeWindow,
        color: &ColorInfo,
        timeline: Option<Timeline>,
    ) -> SurfaceViewSink {
        let mut applied_ds = None;
        apply_dataspace(
            window,
            color_dataspace(color),
            &mut applied_ds,
            "negotiated",
        );
        SurfaceViewSink {
            applied_ds,
            timeline,
        }
    }
}

impl Sink for SurfaceViewSink {
    fn is_asc(&self) -> bool {
        false
    }

    fn needs_clock(&self) -> bool {
        self.timeline.is_some()
    }

    /// A shown output goes to the store, which the pump releases budgeted; a withheld one is
    /// released unrendered, so the window keeps the last frame frozen on.
    fn on_outputs(
        &mut self,
        ctx: &Ctx,
        ready: Vec<OutputReady>,
        shows: &mut dyn FnMut(u64) -> bool,
    ) -> Routed {
        let Some(t) = self.timeline.as_mut() else {
            return arrival(ctx, ready, shows);
        };
        let codec = &ctx.codec;
        let mut r = Routed::default();
        t.outputs += ready.len() as u64;
        for o in ready {
            if shows(o.pts_us) {
                let dropped = t.submit(codec, o.index, o.pts_us, o.decoded_ns, o.decoded_mono_ns);
                r.skipped += dropped;
                r.discarded += dropped;
            } else {
                if let Err(e) = codec.release_output_buffer_by_index(o.index, false) {
                    log::warn!("decode: release_output_buffer_by_index({}): {e}", o.index);
                }
                r.discarded += 1;
                r.skipped += 1;
            }
        }
        r
    }

    fn pump(&mut self, ctx: &Ctx, clock: Option<&VsyncShared>) -> bool {
        let Some(t) = self.timeline.as_mut() else {
            return false;
        };
        let now = now_monotonic_ns();
        t.pump(&ctx.codec, clock, &ctx.tracker, &ctx.meter, now)
    }

    /// A `Skipping` verdict hands the rest of the run to the arrival path: the held frames go
    /// back unrendered first.
    fn flush(&mut self, ctx: &Ctx, clock: Option<&VsyncShared>, lost: bool) -> Option<Verdict> {
        let t = self.timeline.as_mut()?;
        match t.flush_log(&ctx.meter, clock, lost) {
            Some(Verdict::Skipping { permille }) => {
                log::warn!(
                    "presenter: verdict skipping {permille}‰ over 5 s on an unbroken stream — \
                     arrival release for this session"
                );
                t.release_all(&ctx.codec);
                self.timeline = None;
                None
            }
            verdict => verdict,
        }
    }

    fn on_vsync(&mut self, _ctx: &Ctx) {
        if let Some(t) = self.timeline.as_mut() {
            t.budget.on_vsync();
        }
    }

    /// Never arrives: this sink applies no transactions.
    fn on_present_complete(&mut self, _pc: PresentComplete, _clock_offset: i64, _ctx: &Ctx) {}

    /// A format change re-tags the window, and neither the stream nor the codec naming a colour
    /// puts the negotiated one back.
    fn on_format_changed(
        &mut self,
        ctx: &Ctx,
        _picture: Option<(i32, i32)>,
        stream: Option<ColorInfo>,
    ) {
        let source = if stream.is_some() {
            "stream"
        } else {
            "decoder"
        };
        match stream_dataspace(&ctx.codec, stream) {
            Some(ds) => apply_dataspace(&ctx.window, ds, &mut self.applied_ds, source),
            None => {
                self.applied_ds = None;
                let ds = color_dataspace(&ctx.client.video_format().color);
                apply_dataspace(&ctx.window, ds, &mut self.applied_ds, "negotiated");
            }
        }
    }

    fn reset_cadence(&mut self) {
        if let Some(t) = self.timeline.as_mut() {
            t.store.reset_cadence();
        }
    }

    fn release_all(&mut self, codec: &MediaCodec) {
        if let Some(t) = self.timeline.as_mut() {
            t.release_all(codec);
        }
    }

    fn into_stale_layer(self: Box<Self>) -> Option<Layer> {
        None
    }
}
