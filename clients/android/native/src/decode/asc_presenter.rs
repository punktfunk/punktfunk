//! The ASurfaceControl present backend (default): MediaCodec → `AImageReader` → `ASurfaceControl`
//! transactions, each aimed at its own vsync slot on a present-phased grid.
//!
//! Every completion reports the latch time, the present fence and the previous buffer's release
//! fence ([`super::surface_control::PresentComplete`]). The present fence signals at the vsync
//! that scanned the frame out and phases the [`SlotClock`]; the latch is only the compositor's
//! wakeup, one work duration earlier, and that duration differs per device.
//!
//! SurfaceFlinger defers a transaction only while `desiredPresentTime >= expectedPresentTime`, so
//! two frames applied in one period with targets before the next vsync latch together and only
//! the newer shows. Every frame therefore gets its own slot ([`pace_slot`]) and targets
//! `present − period/2`: ready for that slot, not the one before, half a period of tolerance each
//! way. Under `latency` a frame is held at most one slot beyond reachable, only when the hold
//! saves a drop, and the hold is shed at the first gap; under `smooth` the [`CadenceClock`] due
//! time is the earliest instant and the buffer is the bound. Until a present sample exists (or
//! on a device without present fences) the budget is one pending as-soon-as-possible present.
//!
//! Memory safety does not rest on the fences: SurfaceFlinger holds its own buffer reference from
//! `setBuffer`, so an early delete at worst tears. The fences are the correctness of timing.

use ndk::hardware_buffer::HardwareBuffer;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::MediaCodec;
use ndk::native_window::NativeWindow;
use punktfunk_core::phase::{pace_slot, CadenceClock, CadenceTuning, SlotClock, SlotIntervals};
use std::collections::VecDeque;
use std::ffi::CStr;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Instant;

use super::async_loop::DecodeEvent;
use super::latency::{now_realtime_ns, p50_max_ms};
use super::presenter::{cadence_suffix, PresentPriority};
use super::surface_control::{fence_signal_ns, Layer, PresentComplete};
use super::vsync::now_monotonic_ns;

/// Reader pool depth. Must cover the codec's own in-flight outputs + the presenter's held candidate
/// / FIFO + the buffers still latched on SurfaceFlinger awaiting their release fence. Eight is
/// generous for a one-in-flight-ish presenter and small enough that no device balks.
const READER_MAX_IMAGES: i32 = 8;

/// Fallback panel period while none has been learned yet — one 120 Hz frame.
const FALLBACK_PERIOD_NS: i64 = 8_333_333;

/// Apply-ahead beyond the learned lead: the binder hop plus SurfaceFlinger's wakeup→latch gap.
/// Starts at zero (measured as enough on the A024), widens [`APPLY_MARGIN_STEP_NS`] per slot miss
/// to [`APPLY_MARGIN_MAX_NS`], and narrows one step per [`MARGIN_DECAY_PRESENTS`] clean presents.
const APPLY_MARGIN_NS: i64 = 0;
const APPLY_MARGIN_STEP_NS: i64 = 500_000;
const APPLY_MARGIN_MAX_NS: i64 = 2_500_000;
const MARGIN_DECAY_PRESENTS: u32 = 120;

/// A completion that never arrives force-opens the budget after this long (the gate's backstop).
const STALE_REOPEN_NS: i64 = 100_000_000;

/// Completed presents whose fence is still pending; older entries are occluded frames and drop.
const AWAITING_CAP: usize = 8;

/// A present fence pending longer than this never signals (surface occluded / mode switch).
const FENCE_PATIENCE_NS: i64 = 200_000_000;

/// One image acquired from the reader, held until it is presented (or dropped as a newest-wins
/// eviction). An unpresented image returns with its acquire fence as the release fence, so the
/// codec cannot reuse its buffer before its write finishes.
struct Acquired {
    image: Option<Image>,
    buffer: HardwareBuffer,
    fence: Option<OwnedFd>,
    pts_us: u64,
    /// `CLOCK_REALTIME` decode-output stamp (for the skew-corrected end-to-end).
    decoded_real: i128,
    /// The source's due time on the cadence grid (`CLOCK_MONOTONIC`), `None` under latency.
    due_ns: Option<i64>,
}

impl Acquired {
    /// Transfer a presented image out after SurfaceFlinger has consumed its acquire fence.
    fn take_presented_image(&mut self) -> Image {
        debug_assert!(
            self.fence.is_none(),
            "presented image still owns its acquire fence"
        );
        self.image
            .take()
            .expect("acquired image already transferred")
    }
}

impl Drop for Acquired {
    fn drop(&mut self) {
        let Some(image) = self.image.take() else {
            return;
        };
        release_image(image, self.fence.take());
    }
}

/// Return an unused image only after the codec's pending write finishes.
fn release_image(image: Image, fence: Option<OwnedFd>) {
    match fence {
        Some(fence) => image.delete_async(fence),
        None => drop(image),
    }
}

/// One image applied to SurfaceFlinger, awaiting its completion (metrics) and its successor's
/// completion (the release fence that frees it back to the pool).
struct Presented {
    seq: u64,
    image: Image,
    pts_us: u64,
    decoded_real: i128,
    /// `CLOCK_REALTIME` / `CLOCK_MONOTONIC` instants the transaction was applied.
    release_real: i128,
    release_mono: i64,
    /// The vsync slot the scheduler aimed at; `None` for an as-soon-as-possible target.
    slot: Option<i64>,
}

/// One completed present: what the clock and the display metrics need once its vsync is known.
struct PresentSample {
    slot: Option<i64>,
    latch_ns: i64,
    pts_us: u64,
    decoded_real: i128,
    release_real: i128,
    release_mono: i64,
}

/// A completed present whose fence has not signalled yet.
struct Awaiting {
    sample: PresentSample,
    fence: OwnedFd,
}

/// The ASurfaceControl present backend.
pub(super) struct AscBackend {
    reader: ImageReader,
    /// Cached reader window handed to `MediaCodec::configure` as the decoder's output surface.
    reader_window: NativeWindow,
    layer: Layer,
    /// `None` under latency; the source-cadence loop under smooth.
    cadence: Option<CadenceClock>,
    /// FIFO capacity: 0 = newest-wins (latency); 1..=3 = the smoothing store depth.
    fifo_capacity: usize,
    /// The negotiated source frame interval — the cadence cushion ceiling.
    frame_interval_ns: i64,
    /// Transactions applied but not yet completed.
    inflight: u32,

    // -- held images --
    /// Latency: the newest acquired image not yet presented. Smooth leaves this `None`.
    candidate: Option<Acquired>,
    /// Smooth: images held for their due time, oldest first.
    fifo: VecDeque<Acquired>,
    /// Images on SurfaceFlinger, oldest first, awaiting release.
    presented: VecDeque<Presented>,
    /// Completed presents awaiting their present fence, oldest first.
    awaiting: VecDeque<Awaiting>,

    // -- the slot scheduler --
    clock: SlotClock,
    /// The last slot assigned or observed; the next frame never shares it.
    last_slot: Option<i64>,
    /// Slots a frame may be held beyond the first reachable one: 1 under latency, the buffer
    /// under smooth. 0 is the collision the sysprop A/B reproduces.
    max_ahead: i64,
    apply_margin_ns: i64,
    /// Presents since the last slot miss; [`MARGIN_DECAY_PRESENTS`] of them narrow the margin.
    clean_presents: u32,
    /// `debug.punktfunk.asc_pacing` = 0: as-soon-as-possible targets, two pending — the pre-overhaul
    /// behaviour, for the on-glass A/B.
    pacing: bool,
    /// `debug.punktfunk.asc_hold_chain` = 0 under `latency`: a hold never follows a hold — the
    /// frame shares the held frame's slot and SurfaceFlinger keeps the newer. Default 1: the
    /// depth-one elastic queue, which on an exact-rate source holds every frame after the first
    /// burst (A024: 71 of 120 a second, one period of latency). The on-glass A/B.
    hold_chain: bool,
    /// The last assigned slot was a hold.
    last_held: bool,
    /// A present fence has been read at least once this session.
    fence_live: bool,
    last_latch_ns: i64,
    last_present_ns: i64,
    last_observed_slot: Option<i64>,
    #[cfg(debug_assertions)]
    jitter_us: u64,
    #[cfg(debug_assertions)]
    rng: u64,

    /// `ADataSpace` for the transaction (BT709 for SDR — never untagged; see `color_dataspace`).
    dataspace: i32,
    /// The session's HDR10 volume, sent with every transaction. `None` on SDR.
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// Layer frame-rate vote (source Hz), applied once.
    frame_rate: f32,
    src_w: i32,
    src_h: i32,

    // -- bookkeeping --
    next_seq: u64,
    /// Decode stamps parked at `on_output`, keyed by the pts the codec echoes onto the buffer:
    /// `(pts_us, decoded_real_ns, decoded_mono_ns)`.
    stamps: VecDeque<(u64, i128, i64)>,

    // -- 1 Hz pf.present window --
    released: u64,
    skipped: u64,
    displays: u64,
    /// Completions that carried no latch time: the transaction never reached glass as a frame.
    unlatched: u64,
    forced: u64,
    held: u64,
    slot_miss: u64,
    coalesced: u64,
    intervals: SlotIntervals,
    latch_us: Vec<u64>,
    pace_us: Vec<u64>,
    e2e_us: Vec<u64>,
    phase_err_us: Vec<u64>,
    last_flush: Instant,
}

impl AscBackend {
    /// Create the reader + compositor layer, or `None` on API < 29 / init failure (the caller then
    /// runs the SurfaceView presenter). `window` is the SurfaceView's `ANativeWindow`; `src_w/h` the
    /// negotiated decode size; `surface_size` the LIVE view size the layer composites into;
    /// `src_crop` the part of the buffer it shows;
    /// `panel_hz` the mode-table panel rate (seeds the clock);
    /// `dataspace` the `ADataSpace` from the negotiated colour; `source_hz` the negotiated stream rate.
    ///
    /// `overlay` sets the reader's gralloc ask. `true` adds `COMPOSER_OVERLAY`, letting HWC scan
    /// the buffer out directly instead of paying a GPU composition pass — the right default, and
    /// what every device the presenter was tuned on allocates without blinking. It is also the one
    /// reader parameter that can make `AMediaCodec_start` fail AFTER a clean configure: start is
    /// where ACodec dequeues (= gralloc-allocates) every codec output buffer from this reader's
    /// window, with our consumer usage OR'd into the decoder's own producer bits — and an old
    /// 32-bit OMX BSP (the Mi TV Stick's Amlogic gralloc) can refuse the combined
    /// overlay + GPU-sampled + vendor-vdec allocation outright. `false` asks for
    /// `GPU_SAMPLED_IMAGE` alone — the SurfaceTexture shape every TextureView/WebView video path
    /// exercises, the most universally allocatable there is; SurfaceFlinger then GPU-composites the
    /// layer (one 1080p quad — noise), and everything else about the backend is identical.
    /// (`READER_MAX_IMAGES` is NOT a start-time factor — consumer-side images allocate lazily
    /// during streaming — so usage is the only axis a start-failure retry needs.)
    #[allow(clippy::too_many_arguments)]
    pub(super) fn create(
        window: &NativeWindow,
        src_w: i32,
        src_h: i32,
        surface_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
        src_crop: std::sync::Arc<std::sync::atomic::AtomicU64>,
        panel_hz: i32,
        dataspace: i32,
        source_hz: u32,
        priority: PresentPriority,
        overlay: bool,
    ) -> Option<AscBackend> {
        let layer = Layer::create(window, surface_size, src_crop)?;
        let mut usage = ndk::hardware_buffer::HardwareBufferUsage::GPU_SAMPLED_IMAGE;
        if overlay {
            usage |= ndk::hardware_buffer::HardwareBufferUsage::COMPOSER_OVERLAY;
        }
        let reader = match ImageReader::new_with_usage(
            src_w.max(1),
            src_h.max(1),
            ImageFormat::PRIVATE,
            usage,
            READER_MAX_IMAGES,
        ) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("asc: ImageReader init failed ({e:?}) — falling back to SurfaceView");
                return None;
            }
        };
        let reader_window = match reader.window() {
            Ok(w) => w,
            Err(e) => {
                log::warn!("asc: ImageReader has no window ({e:?}) — falling back to SurfaceView");
                return None;
            }
        };
        let frame_interval_ns = match source_hz {
            0 => FALLBACK_PERIOD_NS,
            hz => 1_000_000_000 / i64::from(hz),
        };
        let (fifo_capacity, cadence, max_ahead) = match priority {
            PresentPriority::Latency => (0usize, None, 1i64),
            PresentPriority::Smooth { buffer } => (
                buffer,
                Some(CadenceClock::new(CadenceTuning::snapping())),
                (buffer as i64).clamp(1, 3),
            ),
        };
        let pacing = sysprop(c"debug.punktfunk.asc_pacing").is_none_or(|v| v != "0");
        let hold_chain = sysprop(c"debug.punktfunk.asc_hold_chain").is_none_or(|v| v != "0");
        #[cfg(debug_assertions)]
        let jitter_us = sysprop(c"debug.punktfunk.asc_jitter_us")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            .min(50_000);
        log::info!(
            "asc: backend up — {}, reader usage {} ({}x{} @ {} Hz src, panel seed {} Hz, dataspace {:#x}, pacing {}, hold chain {})",
            match priority {
                PresentPriority::Latency => "latency (newest-wins)".to_string(),
                PresentPriority::Smooth { buffer } => format!("smooth (buffer {buffer})"),
            },
            if overlay { "overlay" } else { "gpu-only" },
            src_w,
            src_h,
            source_hz,
            panel_hz,
            dataspace,
            if pacing { "on" } else { "OFF (sysprop)" },
            if hold_chain { "on" } else { "OFF (sysprop)" },
        );
        Some(AscBackend {
            reader,
            reader_window,
            layer,
            cadence,
            fifo_capacity,
            frame_interval_ns,
            inflight: 0,
            candidate: None,
            fifo: VecDeque::new(),
            presented: VecDeque::new(),
            awaiting: VecDeque::new(),
            clock: SlotClock::seeded(panel_hz),
            last_slot: None,
            max_ahead,
            apply_margin_ns: APPLY_MARGIN_NS,
            clean_presents: 0,
            pacing,
            hold_chain,
            last_held: false,
            fence_live: false,
            last_latch_ns: 0,
            last_present_ns: 0,
            last_observed_slot: None,
            #[cfg(debug_assertions)]
            jitter_us,
            #[cfg(debug_assertions)]
            rng: 0x9E37_79B9_7F4A_7C15,
            dataspace,
            hdr_meta: None,
            frame_rate: if source_hz > 0 { source_hz as f32 } else { 0.0 },
            src_w: src_w.max(1),
            src_h: src_h.max(1),
            next_seq: 0,
            stamps: VecDeque::new(),
            released: 0,
            skipped: 0,
            displays: 0,
            unlatched: 0,
            forced: 0,
            held: 0,
            slot_miss: 0,
            coalesced: 0,
            intervals: SlotIntervals::default(),
            latch_us: Vec::with_capacity(256),
            pace_us: Vec::with_capacity(256),
            e2e_us: Vec::with_capacity(256),
            phase_err_us: Vec::with_capacity(256),
            last_flush: Instant::now(),
        })
    }

    /// The decoder output surface (the reader's window) for `MediaCodec::configure`.
    pub(super) fn reader_window(&self) -> &NativeWindow {
        &self.reader_window
    }

    /// Re-anchor the cadence loop on the next frame — the discontinuity hook the decode loop calls
    /// when the re-anchor gate arms (a loss froze the picture and the decoder recovered behind it,
    /// so the source→presentable delay the loop measured no longer holds). No-op under latency.
    pub(super) fn reset_cadence(&mut self) {
        if let Some(c) = self.cadence.as_mut() {
            c.reset();
        }
    }

    /// Route one decoded output buffer: render it into the reader when `present` (the re-anchor
    /// gate approved it), else drop it off-glass. Parks the decode stamps for the pts the codec
    /// echoes onto the buffer so `pump` can pair the latency metrics after acquire.
    pub(super) fn on_output(
        &mut self,
        codec: &MediaCodec,
        index: usize,
        pts_us: u64,
        decoded_real: i128,
        decoded_mono: i64,
        present: bool,
    ) {
        if present {
            self.stamps.push_back((pts_us, decoded_real, decoded_mono));
            if self.stamps.len() > 128 {
                self.stamps.pop_front();
            }
        }
        if let Err(e) = codec.release_output_buffer_by_index(index, present) {
            log::warn!("asc: release_output_buffer_by_index({index}, {present}): {e}");
        }
    }

    /// Pop the decode stamps for `pts_us`, evicting older entries (decode order == input order).
    fn take_stamp(&mut self, pts_us: u64) -> Option<(i128, i64)> {
        while let Some(&(p, real, mono)) = self.stamps.front() {
            if p > pts_us {
                break;
            }
            self.stamps.pop_front();
            if p == pts_us {
                return Some((real, mono));
            }
        }
        None
    }

    /// Slot scheduling is live: pacing wanted and the clock has a real present behind it.
    fn paced(&self) -> bool {
        self.pacing && self.clock.phased()
    }

    /// The panel period the smoothing store reaches ahead by.
    fn period_ns(&self) -> i64 {
        match self.clock.period_ns() {
            0 => FALLBACK_PERIOD_NS,
            p => p,
        }
    }

    /// The desired present time for a frame whose earliest presentable instant is `earliest`
    /// (`CLOCK_MONOTONIC`), and the slot it aims at. `0` = as soon as possible: the bootstrap
    /// before the first present sample, a device without present fences, or pacing switched off.
    fn next_target(&mut self, earliest: i64) -> (i64, Option<i64>) {
        if !self.paced() {
            return (0, None);
        }
        let reachable = self
            .clock
            .slot_after(earliest + self.clock.lead_ns() + self.apply_margin_ns);
        let mut slot = pace_slot(reachable, self.last_slot, self.max_ahead);
        let mut held = slot > reachable;
        if held && self.last_held && !self.hold_chain && self.fifo_capacity == 0 {
            // A hold never follows a hold: share the held frame's slot, SurfaceFlinger keeps the
            // newer, the stale held frame drops (`coalesced` counts it).
            slot = self.last_slot.unwrap_or(reachable);
            held = false;
        }
        if held {
            self.held += 1;
        }
        self.last_held = held;
        self.last_slot = Some(slot);
        (
            self.clock.present_ns(slot) - self.clock.period_ns() / 2,
            Some(slot),
        )
    }

    /// Drain the reader into the held set (newest-wins candidate, or the smoothing FIFO), then
    /// present the due frame if the budget is open. `panel_period_ns` is the choreographer's
    /// panel period (0 = none yet): the compositor's own statement of the grid, which a source
    /// below the panel rate can never bias the way present spacings can. Returns `true` when a
    /// frame was applied.
    pub(super) fn pump(
        &mut self,
        now_mono: i64,
        panel_period_ns: i64,
        ev_tx: &mpsc::Sender<DecodeEvent>,
    ) -> bool {
        if panel_period_ns > 0 {
            self.clock.set_period(panel_period_ns);
        }
        self.drain_reader();
        // The budget: one undisplayed transaction until the slots are live (then two — distinct
        // targets cannot collide, and SurfaceFlinger holds the second), force-opened when a
        // completion never comes.
        let cap = if self.pacing && !self.paced() { 1 } else { 2 };
        if self.inflight >= cap {
            // The oldest transaction still awaiting its completion; a count with no entry behind
            // it is bookkeeping drift and reopens too.
            let stale = self
                .presented
                .iter()
                .rev()
                .nth(self.inflight as usize - 1)
                .is_none_or(|p| now_mono - p.release_mono > STALE_REOPEN_NS);
            if !stale {
                return false;
            }
            self.forced += 1;
            self.inflight = 0;
        }
        // Pick the frame to present.
        let frame = if self.fifo_capacity == 0 {
            self.candidate.take()
        } else {
            let reach = now_mono + self.period_ns();
            match self.fifo.front() {
                Some(f) if f.due_ns.is_none_or(|due| due <= reach) => self.fifo.pop_front(),
                _ => return false,
            }
        };
        let Some(mut frame) = frame else {
            return false;
        };
        #[cfg(debug_assertions)]
        let now_mono = self.inject_jitter(now_mono);
        let earliest = frame.due_ns.map_or(now_mono, |d| d.max(now_mono));
        let (target, slot) = self.next_target(earliest);
        let seq = self.next_seq;
        let applied = self.layer.present(
            &frame.buffer,
            self.src_w,
            self.src_h,
            &mut frame.fence,
            target,
            self.dataspace,
            self.hdr_meta.as_ref(),
            // The layer's fixed-source rate — applied once, at layer config (see `Layer::present`).
            self.frame_rate,
            seq,
            ev_tx,
        );
        if !applied {
            return false; // transaction failed; the image drops here, back to the pool
        }
        let release_real = now_realtime_ns();
        let pace_us = ((release_real - frame.decoded_real).max(0) / 1000) as u64;
        self.pace_us.push(pace_us);
        let image = frame.take_presented_image();
        self.presented.push_back(Presented {
            seq,
            image,
            pts_us: frame.pts_us,
            decoded_real: frame.decoded_real,
            release_real,
            release_mono: now_monotonic_ns(),
            slot,
        });
        self.inflight += 1;
        self.next_seq += 1;
        self.released += 1;
        true
    }

    /// `debug.punktfunk.asc_jitter_us`: a uniform 0..N µs stall before the apply, so the collision
    /// this scheduler exists for reproduces on a quiet link. Returns the time after the stall.
    /// Debug builds only.
    #[cfg(debug_assertions)]
    fn inject_jitter(&mut self, now_mono: i64) -> i64 {
        if self.jitter_us == 0 {
            return now_mono;
        }
        // xorshift64: no dependency, no quality requirement.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        std::thread::sleep(std::time::Duration::from_micros(self.rng % self.jitter_us));
        now_monotonic_ns()
    }

    /// Acquire newly rendered images: latency keeps only the newest (older ones drop back to the
    /// pool as they are superseded); smooth keeps order up to capacity.
    ///
    /// One `acquireNextImageAsync` at a time, never `acquireLatestImageAsync`: that call
    /// (`NdkImageReader.cpp`, unfixed as of AOSP main) drains through one `int*` fence out-param it
    /// overwrites per image, releases each dropped image with its SUCCESSOR's fence, and hands the
    /// caller the last fence it already gave away. `setBuffer` then transfers that stale fd to
    /// SurfaceFlinger, which closes it a second time — an `fdsan` `SIGABRT` on the decode thread
    /// the moment a burst gives it two images to collapse.
    fn drain_reader(&mut self) {
        if self.fifo_capacity == 0 {
            // Newest-wins: collapse the burst to the freshest buffer ourselves. Each superseded
            // candidate returns with its acquire fence, so the codec cannot reuse it too early.
            while let Some(acq) = self.acquire() {
                if self.candidate.replace(acq).is_some() {
                    self.skipped += 1; // an un-presented candidate was superseded
                }
            }
        } else {
            // Smooth: pull every ready image in order into the FIFO, evicting the oldest past cap.
            while let Some(acq) = self.acquire() {
                self.fifo.push_back(acq);
                while self.fifo.len() > self.fifo_capacity {
                    self.fifo.pop_front();
                    self.skipped += 1;
                }
            }
        }
    }

    /// Acquire the next image and pair its decode stamps + cadence due. `None` when the reader is
    /// empty or a transient acquire error occurs.
    fn acquire(&mut self) -> Option<Acquired> {
        // SAFETY: we never touch the image's pixels. Its acquire fence goes to SurfaceFlinger when
        // presented or back to AImageReader as the release fence when discarded.
        let res = unsafe { self.reader.acquire_next_image_async() };
        let (image, fence) = match res {
            Ok(AcquireResult::Image(pair)) => pair,
            Ok(_) => return None, // no buffer available / max acquired
            Err(e) => {
                log::warn!("asc: acquire image failed: {e:?}");
                return None;
            }
        };
        let buffer = match image.hardware_buffer() {
            Ok(b) => b,
            Err(e) => {
                log::warn!("asc: image has no hardware buffer: {e:?}");
                release_image(image, fence);
                return None;
            }
        };
        // The buffer timestamp is the pts the codec echoed (ns); pair the parked decode stamps.
        let pts_ns = image.timestamp().unwrap_or(0).max(0);
        let pts_us = (pts_ns / 1000) as u64;
        let (decoded_real, decoded_mono) = self
            .take_stamp(pts_us)
            .unwrap_or((now_realtime_ns(), now_monotonic_ns()));
        let due_ns = self.cadence.as_mut().map(|c| {
            c.due_ns(
                pts_us.saturating_mul(1000),
                decoded_mono,
                self.frame_interval_ns,
            )
        });
        Some(Acquired {
            image: Some(image),
            buffer,
            fence,
            pts_us,
            decoded_real,
            due_ns,
        })
    }

    /// A completed transaction: reopen the budget, queue the present sample behind its fence (or
    /// record it off the latch when there is none), and free the buffer this frame replaced with
    /// its release fence. Runs on the decode thread (the callback only forwarded the data).
    pub(super) fn on_present_complete(
        &mut self,
        pc: PresentComplete,
        clock_offset: i64,
        stats: &crate::stats::VideoStats,
        video_e2e: &AtomicU64,
    ) {
        self.inflight = self.inflight.saturating_sub(1);
        if pc.latch_ns == 0 {
            self.unlatched += 1;
        }
        if pc.latch_ns > 0 {
            // Two transactions latched in one commit share the latch instant: SurfaceFlinger showed
            // only the newer. The present-interval histogram sees the same pair as a 0-slot spacing.
            if pc.latch_ns == self.last_latch_ns {
                self.coalesced += 1;
            }
            self.last_latch_ns = pc.latch_ns;
            if let Some(p) = self.presented.iter().find(|p| p.seq == pc.seq) {
                let sample = PresentSample {
                    slot: p.slot,
                    latch_ns: pc.latch_ns,
                    pts_us: p.pts_us,
                    decoded_real: p.decoded_real,
                    release_real: p.release_real,
                    release_mono: p.release_mono,
                };
                match pc.present_fence {
                    Some(fence) => {
                        self.awaiting.push_back(Awaiting { sample, fence });
                        if self.awaiting.len() > AWAITING_CAP {
                            // Occluded: that fence never signals; its latch still counts.
                            let Awaiting { sample, .. } = self.awaiting.pop_front().unwrap();
                            let latch = sample.latch_ns;
                            self.on_present(sample, latch, false, clock_offset, stats, video_e2e);
                        }
                    }
                    // No present fence on this device: the latch is the best instant there is.
                    None => {
                        self.on_present(sample, pc.latch_ns, false, clock_offset, stats, video_e2e)
                    }
                }
            }
            self.poll_fences(clock_offset, stats, video_e2e);
        }
        // Retire every buffer this transaction replaced (seq < completed): the immediate
        // predecessor gets the real release fence, any older straggler a plain delete (memory-safe
        // — SurfaceFlinger holds its own reference until it is actually done).
        let mut retired: Vec<Presented> = Vec::new();
        while self.presented.front().is_some_and(|p| p.seq < pc.seq) {
            retired.push(self.presented.pop_front().unwrap());
        }
        match (retired.pop(), pc.prev_release_fence) {
            (Some(last), Some(fence)) => last.image.delete_async(fence),
            (Some(last), None) => drop(last.image),
            (None, Some(fence)) => drop(fence),
            (None, None) => {}
        }
        // (`retired` now holds only older stragglers, dropped here — plain AImage_delete.)
        drop(retired);
    }

    /// Read every present fence that has signalled, oldest first; a pending one stops the scan
    /// (order is what the intervals measure). One that outlives its patience never signals and
    /// is recorded off its latch instead. Runs on every completion and on every vsync tick, so
    /// the last frame before a pause is scored off its fence, not off its latch 200 ms later.
    pub(super) fn poll_fences(
        &mut self,
        clock_offset: i64,
        stats: &crate::stats::VideoStats,
        video_e2e: &AtomicU64,
    ) {
        let now = now_monotonic_ns();
        while let Some(front) = self.awaiting.front() {
            match fence_signal_ns(front.fence.as_fd()) {
                Some(present_ns) => {
                    let Awaiting { sample, .. } = self.awaiting.pop_front().unwrap();
                    self.fence_live = true;
                    self.on_present(sample, present_ns, true, clock_offset, stats, video_e2e);
                }
                None if now - front.sample.release_mono > FENCE_PATIENCE_NS => {
                    let Awaiting { sample, .. } = self.awaiting.pop_front().unwrap();
                    let latch = sample.latch_ns;
                    self.on_present(sample, latch, false, clock_offset, stats, video_e2e);
                }
                None => break,
            }
        }
    }

    /// One frame on glass at `present_ns`: with a `real` vsync (a present fence) phase the clock
    /// and correct the pacer by what actually happened; either way score the interval and record
    /// the display metrics. A latch stands in for the vsync where no fence exists — it never
    /// phases the clock, so such a device keeps the one-pending as-soon-as-possible budget.
    fn on_present(
        &mut self,
        a: PresentSample,
        present_ns: i64,
        real: bool,
        clock_offset: i64,
        stats: &crate::stats::VideoStats,
        video_e2e: &AtomicU64,
    ) {
        if present_ns == self.last_present_ns {
            self.intervals.note(0); // the coalesced pair's second half
        } else if present_ns > self.last_present_ns {
            if real {
                self.observe_present(present_ns, a.latch_ns, a.slot);
            } else if self.last_present_ns > 0 {
                let p = self.period_ns();
                self.intervals
                    .note((present_ns - self.last_present_ns + p / 2).div_euclid(p));
            }
            self.last_present_ns = present_ns;
        }
        // Apply → on glass: what the HUD and the log line call `latch`, which is SurfaceFlinger's
        // whole share and not its latch instant.
        let on_glass_ns = (present_ns - a.release_mono).clamp(0, 10_000_000_000);
        let displayed_real = a.release_real + on_glass_ns as i128;
        let e2e_ns = displayed_real + clock_offset as i128 - a.pts_us as i128 * 1000;
        let latch_use = (on_glass_ns / 1000) as u64;
        self.latch_us.push(latch_use);
        self.displays += 1;
        if e2e_ns > 0 && e2e_ns < 10_000_000_000 {
            self.e2e_us.push((e2e_ns / 1000) as u64);
            // Publish glass-to-glass RAW for the audio plane to align against.
            video_e2e.store(e2e_ns as u64, Ordering::Relaxed);
        }
        stats.note_displayed(
            a.pts_us * 1000,
            a.decoded_real,
            a.release_real,
            displayed_real,
        );
    }

    /// A real vsync for a frame the scheduler aimed at `assigned`: phase the clock, score the slot
    /// interval, and correct the pacer when the frame landed later than aimed.
    fn observe_present(&mut self, present_ns: i64, latch_ns: i64, assigned: Option<i64>) {
        if let Some(assigned) = assigned {
            if self.clock.phased() {
                let err = present_ns - self.clock.present_ns(assigned);
                self.phase_err_us.push(err.unsigned_abs() / 1000);
            }
        }
        let observed = self.clock.observe(present_ns, latch_ns);
        if let Some(prev) = self.last_observed_slot {
            self.intervals.note(observed - prev);
        }
        self.last_observed_slot = Some(observed);
        if assigned.is_some_and(|s| observed > s) {
            // SurfaceFlinger woke before the apply landed, or the uid's frame-rate override
            // skipped that vsync: the next frame must not aim at the slot this one took.
            self.slot_miss += 1;
            self.clean_presents = 0;
            self.apply_margin_ns =
                (self.apply_margin_ns + APPLY_MARGIN_STEP_NS).min(APPLY_MARGIN_MAX_NS);
        } else {
            self.clean_presents += 1;
            if self.clean_presents >= MARGIN_DECAY_PRESENTS {
                self.clean_presents = 0;
                self.apply_margin_ns = (self.apply_margin_ns - APPLY_MARGIN_STEP_NS).max(0);
            }
        }
        if self.last_slot.is_none_or(|l| observed > l) {
            self.last_slot = Some(observed);
        }
    }

    /// Publish the reader-drop count to the HUD and emit the 1 Hz `pf.present` mirror line. Called
    /// once per loop pass; the `skipped` counter feeds the HUD each pass, the log line at 1 Hz.
    pub(super) fn flush(&mut self, stats: &crate::stats::VideoStats) {
        if self.skipped > 0 {
            stats.note_skipped(std::mem::take(&mut self.skipped));
        }
        if self.last_flush.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        self.last_flush = Instant::now();
        if self.released == 0 && self.displays == 0 {
            return; // idle
        }
        let (latch_p50, latch_max) = p50_max_ms(std::mem::take(&mut self.latch_us));
        let (pace_p50, pace_max) = p50_max_ms(std::mem::take(&mut self.pace_us));
        let (e2e_p50, e2e_max) = p50_max_ms(std::mem::take(&mut self.e2e_us));
        let (err_p50, err_max) = p50_max_ms(std::mem::take(&mut self.phase_err_us));
        let intervals = std::mem::take(&mut self.intervals);
        let cadence = cadence_suffix(self.cadence.as_ref().map(CadenceClock::health));
        stats.note_cadence(intervals.judder_permille(), self.coalesced);
        log::info!(
            target: "pf.present",
            "asc released={} displays={} inflight={} qDepth={} paceMs p50={:.2} max={:.2} \
             latchMs p50={:.2} max={:.2} e2eMs p50={:.2} max={:.2} panelMs={:.2} leadMs={:.2} \
             marginUs={} held={} slotMiss={} coalesced={} n0={} n1={} n2={} n3+={} judder={}‰ \
             mode={} phaseErrMs p50={:.2} max={:.2} forced={} unlatched={} fence={}{}",
            self.released,
            self.displays,
            self.inflight,
            self.fifo.len(),
            pace_p50,
            pace_max,
            latch_p50,
            latch_max,
            e2e_p50,
            e2e_max,
            self.clock.period_ns() as f64 / 1e6,
            self.clock.lead_ns() as f64 / 1e6,
            self.apply_margin_ns / 1000,
            self.held,
            self.slot_miss,
            self.coalesced,
            intervals.n0,
            intervals.n1,
            intervals.n2,
            intervals.n3p,
            intervals.judder_permille(),
            intervals.mode(),
            err_p50,
            err_max,
            self.forced,
            self.unlatched,
            u8::from(self.fence_live),
            cadence,
        );
        self.released = 0;
        self.displays = 0;
        self.unlatched = 0;
        self.held = 0;
        self.slot_miss = 0;
        self.coalesced = 0;
    }

    /// Teardown: return every held image before the reader and codec go away. Unpresented images
    /// retain their acquire fences; SurfaceFlinger owns refs to the presented images until done.
    pub(super) fn release_all(&mut self) {
        self.candidate = None;
        self.fifo.clear();
        self.presented.clear();
        self.awaiting.clear();
    }
}

impl AscBackend {
    /// The HDR10 volume sent with every subsequent transaction.
    pub(super) fn set_hdr_meta(&mut self, meta: Option<punktfunk_core::quic::HdrMeta>) {
        self.hdr_meta = meta;
    }

    /// Update the `ADataSpace` applied to every subsequent transaction (a refinement from the
    /// codec's output format — the analogue of the SurfaceView path's `apply_reported_dataspace`;
    /// the negotiated colour set the initial value at create).
    pub(super) fn set_dataspace(&mut self, dataspace: i32) {
        if self.dataspace != dataspace {
            self.dataspace = dataspace;
            log::info!("asc: buffer dataspace now {dataspace:#x}");
        }
    }
}

/// A `debug.punktfunk.*` system property, trimmed; `None` when unset.
fn sysprop(name: &CStr) -> Option<String> {
    let mut buf = [0u8; 92]; // PROP_VALUE_MAX
                             // SAFETY: __system_property_get with a valid name + PROP_VALUE_MAX buffer is always safe.
    let n = unsafe { libc::__system_property_get(name.as_ptr(), buf.as_mut_ptr().cast()) };
    (n > 0).then(|| {
        String::from_utf8_lossy(&buf[..n as usize])
            .trim()
            .to_string()
    })
}

/// Whether the ASurfaceControl backend is selected. Default ON; `debug.punktfunk.present_backend =
/// surfaceview` forces the legacy SurfaceView presenter (the field escape hatch, no rebuild). Any
/// other value — or an ASC init failure downstream — still lands on ASC-then-fallback.
pub(super) fn asc_backend_selected() -> bool {
    sysprop(c"debug.punktfunk.present_backend").as_deref() != Some("surfaceview")
}
