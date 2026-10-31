//! The ASurfaceControl sink (default): MediaCodec → `AImageReader` → `ASurfaceControl`
//! transactions, each aimed at the vsync slot [`Slots`] picks on a grid phased on the
//! compositor's own cycle.
//!
//! Every completion reports the latch time, the present fence and the previous buffer's release
//! fence ([`PresentComplete`]). The latch plus the learned lead is where the compositor presents,
//! the instant its readiness check for a desired present time reads, and phases the slot clock;
//! the present fence signals when the frame reached glass, which a display queue can put whole
//! refreshes later. Until a present sample exists (or on a device without present fences) the
//! budget is one pending as-soon-as-possible present.
//!
//! Memory safety does not rest on the fences: SurfaceFlinger holds its own buffer reference from
//! `setBuffer`, so an early delete at worst tears. The fences are the correctness of timing.

use ndk::hardware_buffer::HardwareBuffer;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::MediaCodec;
use ndk::native_window::NativeWindow;
use punktfunk_core::quic::ColorInfo;
use std::collections::VecDeque;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::super::async_loop::{Ctx, DecodeEvent, OutputReady};
use super::super::display::stream_dataspace;
use super::super::latency::{now_realtime_ns, p50_max_ms, take_by_pts};
use super::super::surface_control::{
    fence_errored, fence_signal_ns, present, Layer, PresentComplete,
};
use super::super::vsync::VsyncShared;
use super::{Downrate, Routed, Sink};
use crate::present::budget::AscBudget;
use crate::present::slots::Slots;
use crate::present::store::{Held, Store};
use crate::present::verdict::{Second, Verdict, Verdicts};
use crate::present::{cadence_suffix, frame_interval_ns, Counters, PresentPriority};
use crate::session::PictureLayers;
use crate::sys::{now_monotonic_ns, sysprop};

/// Reader pool depth. Must cover the codec's own in-flight outputs + the presenter's held candidate
/// / FIFO + the buffers still latched on SurfaceFlinger awaiting their release fence. Eight is
/// generous for a one-in-flight-ish presenter and small enough that no device balks.
const READER_MAX_IMAGES: i32 = 8;

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
    /// A duplicate of the acquire fence `setBuffer` took: it signals when the decoder's pixels
    /// are written, which an early-output decoder reports after the output callback.
    acquire: Option<OwnedFd>,
}

/// One completed present: what the clock and the display metrics need once its vsync is known.
struct PresentSample {
    slot: Option<i64>,
    latch_ns: i64,
    pts_us: u64,
    decoded_real: i128,
    release_real: i128,
    release_mono: i64,
    acquire: Option<OwnedFd>,
}

/// A completed present whose fence has not signalled yet.
struct Awaiting {
    sample: PresentSample,
    fence: OwnedFd,
}

/// The ASurfaceControl present backend.
pub(crate) struct AscSink {
    reader: ImageReader,
    /// Cached reader window handed to `MediaCodec::configure` as the decoder's output surface.
    reader_window: NativeWindow,
    /// The picture window's layer, the one the slot clock and the present fence follow.
    layer: Layer,
    /// A dual-screen handheld's second window's layer, built from `layers` when Kotlin attaches
    /// one and dropped when it goes; `second_gen` is the generation it was built at.
    second: Option<Layer>,
    second_gen: u64,
    layers: std::sync::Arc<PictureLayers>,
    /// The layer mask the last present used; a bit that clears hides its layer once.
    shown: u8,

    // -- the policy (`crate::present`) --
    /// Newest-wins candidate (latency) or the smoothing FIFO, images not yet presented.
    store: Store<Acquired>,
    budget: AscBudget,
    slots: Slots,
    downrate: Downrate,
    downrated: bool,
    verdicts: Verdicts,

    // -- images on SurfaceFlinger --
    /// Images on SurfaceFlinger, oldest first, awaiting release.
    presented: VecDeque<Presented>,
    /// Completed presents awaiting their present fence, oldest first.
    awaiting: VecDeque<Awaiting>,
    #[cfg(debug_assertions)]
    jitter_us: u64,
    #[cfg(debug_assertions)]
    rng: u64,

    /// `ADataSpace` for the transaction (never untagged; see `layer_dataspace`).
    dataspace: i32,
    /// Android 12+: the codec's reported colour may replace `dataspace`. Earlier releases take
    /// only the few values `layer_dataspace` admits, so the negotiated one stands.
    refine: bool,
    /// The session's HDR10 volume, sent with every transaction. `None` on SDR.
    hdr_meta: Option<punktfunk_core::quic::HdrMeta>,
    /// Layer frame-rate vote (source Hz), applied once.
    frame_rate: f32,
    src_w: i32,
    src_h: i32,

    // -- bookkeeping --
    next_seq: u64,
    /// Decode stamps parked at `on_output`, keyed by the pts the codec echoes onto the buffer:
    /// `(pts_us, (decoded_real_ns, decoded_mono_ns))`.
    stamps: VecDeque<(u64, (i128, i64))>,

    // -- 1 Hz pf.present window --
    counters: Counters,
    /// Decoded outputs, store evictions (the HUD takes `counters.skipped` every pass), and
    /// whether an AU was lost, in this window: the verdicts' second.
    outputs: u64,
    skipped: u64,
    lost: bool,
    latch_us: Vec<u64>,
    pace_us: Vec<u64>,
    e2e_us: Vec<u64>,
    phase_err_us: Vec<u64>,
    /// Frames acquired with a pending fence (an early output), fences that resolved in error, and
    /// the fence's signal minus the output callback: the codec time the HUD `decode` misses.
    fenced: u64,
    fence_err: u64,
    fence_tail_us: Vec<u64>,
    last_flush: Instant,
}

impl AscSink {
    /// Create the reader + compositor layer, or `None` on API < 29 / init failure (the caller then
    /// runs the SurfaceView presenter). `window` is the SurfaceView's `ANativeWindow`; `src_w/h` the
    /// negotiated decode size; `surface_size` the LIVE view size the layer composites into;
    /// `src_crop` the part of the buffer it shows; `layers` the second window and the shown mask;
    /// `panel_hz` the mode-table panel rate (seeds the clock); `dataspace` the `ADataSpace` from the
    /// negotiated colour; `source_hz` the negotiated stream rate, which the layer votes, switching
    /// the panel non-seamlessly when `force_switch` ([`super::super::DecodeOptions::force_rate_switch`]).
    ///
    /// `overlay` sets the reader's gralloc ask. `true` adds `COMPOSER_OVERLAY` so HWC scans the
    /// buffer out directly: the default. It is also the one reader parameter that can make
    /// `AMediaCodec_start` fail after a clean configure: start gralloc-allocates every codec
    /// output buffer from this reader's window with our consumer usage OR'd into the decoder's
    /// producer bits, and an old 32-bit OMX BSP (the Mi TV Stick's Amlogic gralloc) can refuse the
    /// combined allocation. `false` asks for `GPU_SAMPLED_IMAGE` alone, the SurfaceTexture shape
    /// every TextureView path exercises; SurfaceFlinger then GPU-composites the layer. Images
    /// allocate lazily, so `READER_MAX_IMAGES` is no start-time factor.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create(
        window: &NativeWindow,
        src_w: i32,
        src_h: i32,
        surface_size: std::sync::Arc<std::sync::atomic::AtomicU64>,
        src_crop: std::sync::Arc<std::sync::atomic::AtomicU64>,
        layers: std::sync::Arc<PictureLayers>,
        panel_hz: i32,
        dataspace: i32,
        source_hz: u32,
        priority: PresentPriority,
        overlay: bool,
        force_switch: bool,
    ) -> Option<AscSink> {
        let layer = Layer::create(window, surface_size, src_crop, force_switch)?;
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
        let pacing = sysprop(c"debug.punktfunk.asc_pacing").is_none_or(|v| v != "0");
        let hold = sysprop(c"debug.punktfunk.asc_hold_chain").as_deref() == Some("1");
        let hold_band_ns = sysprop(c"debug.punktfunk.asc_hold_band_us")
            .and_then(|v| v.parse::<i64>().ok())
            .map(|us| us.clamp(0, 50_000) * 1000);
        #[cfg(debug_assertions)]
        let jitter_us = sysprop(c"debug.punktfunk.asc_jitter_us")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            .min(50_000);
        log::info!(
            "asc: backend up — {}, reader usage {} ({}x{} @ {} Hz src, panel seed {} Hz, dataspace {:#x}, pacing {}, hold chain {}, hold band {})",
            match priority {
                PresentPriority::Latency => "latency (one-MAD cushion)".to_string(),
                PresentPriority::Smooth { buffer } => format!("smooth (buffer {buffer})"),
            },
            if overlay { "overlay" } else { "gpu-only" },
            src_w,
            src_h,
            source_hz,
            panel_hz,
            dataspace,
            if pacing { "on" } else { "OFF (sysprop)" },
            if hold { "ON (sysprop)" } else { "off" },
            hold_band_ns.map_or("auto".to_string(), |ns| format!("{} µs (sysprop)", ns / 1000)),
        );
        Some(AscSink {
            reader,
            reader_window,
            layer,
            second: None,
            // Behind the live value by one, so the first pump follows a window attached before
            // the backend came up.
            second_gen: layers.generation.load(Ordering::Acquire).wrapping_sub(1),
            layers,
            shown: PictureLayers::PRIMARY,
            store: Store::new(
                priority,
                frame_interval_ns(source_hz),
                super::cushion_tail(),
            ),
            budget: AscBudget::default(),
            slots: Slots::new(panel_hz, priority, pacing, hold, hold_band_ns),
            downrate: Downrate::new(panel_hz),
            downrated: false,
            verdicts: Verdicts::default(),
            presented: VecDeque::new(),
            awaiting: VecDeque::new(),
            #[cfg(debug_assertions)]
            jitter_us,
            #[cfg(debug_assertions)]
            rng: 0x9E37_79B9_7F4A_7C15,
            dataspace,
            refine: crate::sys::sdk_int() >= 31,
            hdr_meta: None,
            frame_rate: if source_hz > 0 { source_hz as f32 } else { 0.0 },
            src_w: src_w.max(1),
            src_h: src_h.max(1),
            next_seq: 0,
            stamps: VecDeque::new(),
            counters: Counters::default(),
            outputs: 0,
            skipped: 0,
            lost: false,
            latch_us: Vec::with_capacity(256),
            pace_us: Vec::with_capacity(256),
            e2e_us: Vec::with_capacity(256),
            phase_err_us: Vec::with_capacity(256),
            fenced: 0,
            fence_err: 0,
            fence_tail_us: Vec::with_capacity(256),
            last_flush: Instant::now(),
        })
    }

    /// The decoder output surface (the reader's window) for `MediaCodec::configure`.
    pub(crate) fn reader_window(&self) -> &NativeWindow {
        &self.reader_window
    }

    /// Route one decoded output buffer: render it into the reader when `present` (the re-anchor
    /// gate approved it), else drop it off-glass. Parks the decode stamps for the pts the codec
    /// echoes onto the buffer so the pump can pair the latency metrics after acquire.
    fn on_output(
        &mut self,
        codec: &MediaCodec,
        index: usize,
        pts_us: u64,
        decoded_real: i128,
        decoded_mono: i64,
        present: bool,
    ) {
        if present {
            self.stamps
                .push_back((pts_us, (decoded_real, decoded_mono)));
            if self.stamps.len() > 128 {
                self.stamps.pop_front();
            }
        }
        if let Err(e) = codec.release_output_buffer_by_index(index, present) {
            log::warn!("asc: release_output_buffer_by_index({index}, {present}): {e}");
        }
    }

    /// Follow Kotlin's second window and shown mask: rebuild the second layer on a new window
    /// (or drop it with the window), and hide a layer whose bit cleared; the next present shows
    /// the ones set. Nothing shown reads as the first layer: a frame must land somewhere.
    fn sync_layers(&mut self) {
        let generation = self.layers.generation.load(Ordering::Acquire);
        if generation != self.second_gen {
            self.second_gen = generation;
            if let Some(mut old) = self.second.take() {
                old.hide();
            }
            let window = crate::session::lock_recover(&self.layers.second_window).clone();
            self.second = window.and_then(|w| {
                Layer::create(
                    &w,
                    self.layers.second_size.clone(),
                    self.layers.second_crop.clone(),
                    self.layer.force_switch(),
                )
            });
            log::info!(
                "asc: second picture layer {}",
                if self.second.is_some() { "up" } else { "down" }
            );
        }
        let mut mask = self.layers.shown.load(Ordering::Relaxed);
        if self.second.is_none() {
            mask &= PictureLayers::PRIMARY;
        }
        if mask == 0 {
            mask = PictureLayers::PRIMARY;
        }
        if mask == self.shown {
            return;
        }
        if self.shown & !mask & PictureLayers::PRIMARY != 0 {
            self.layer.hide();
        }
        if self.shown & !mask & PictureLayers::SECOND != 0 {
            if let Some(s) = self.second.as_mut() {
                s.hide();
            }
        }
        self.shown = mask;
    }

    /// Drain the reader into the store, then present the due frame if the budget is open.
    /// `panel_period_ns` is the choreographer's panel period (0 = none yet); on a down-rated
    /// panel every frame goes out at once, as soon as possible, uncapped. Returns `true` when a
    /// frame was applied.
    fn present_due(
        &mut self,
        now_mono: i64,
        panel_period_ns: i64,
        ev_tx: &mpsc::Sender<DecodeEvent>,
    ) -> bool {
        self.slots.set_period(panel_period_ns);
        self.downrated = self.downrate.follow(panel_period_ns);
        self.drain_reader();
        let reach = now_mono + self.slots.period_ns();
        let Some(Held { frame, due_ns }) = self.budget.pick(
            &mut self.store,
            now_mono,
            self.slots.bootstrapping(),
            self.presented
                .iter()
                .map(|p| self.slots.latch_ns_for(p.slot, p.release_mono)),
            reach,
            self.downrated,
            &mut self.counters,
        ) else {
            return false;
        };
        let mut frame = frame;
        #[cfg(debug_assertions)]
        let now_mono = self.inject_jitter(now_mono);
        let earliest = due_ns.map_or(now_mono, |d| d.max(now_mono));
        let (target, slot) =
            self.slots
                .next_target(earliest, now_mono, self.downrated, &mut self.counters);
        let seq = self.next_seq;
        self.sync_layers();
        let (src_w, src_h, dataspace, frame_rate) =
            (self.src_w, self.src_h, self.dataspace, self.frame_rate);
        let mut targets: Vec<&mut Layer> = Vec::with_capacity(2);
        if self.shown & PictureLayers::PRIMARY != 0 {
            targets.push(&mut self.layer);
        }
        if self.shown & PictureLayers::SECOND != 0 {
            if let Some(s) = self.second.as_mut() {
                targets.push(s);
            }
        }
        // `setBuffer` takes the acquire fence; this duplicate is read once the frame is on glass
        // (`on_present`), for the decoder's own finish.
        let acquire = frame.fence.as_ref().and_then(|f| f.try_clone().ok());
        let applied = present(
            &mut targets,
            &frame.buffer,
            src_w,
            src_h,
            &mut frame.fence,
            target,
            dataspace,
            self.hdr_meta.as_ref(),
            // Each layer's fixed-source rate — applied once, at layer config (see `Layer::stage`).
            frame_rate,
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
            acquire,
        });
        self.budget.applied(&mut self.counters);
        self.next_seq += 1;
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

    /// Acquire newly rendered images into the store: latency keeps only the newest (older ones
    /// drop back to the pool as they are superseded); smooth keeps order up to capacity.
    ///
    /// One `acquireNextImageAsync` at a time, never `acquireLatestImageAsync`: that call
    /// (`NdkImageReader.cpp`, unfixed as of AOSP main) drains through one `int*` fence out-param it
    /// overwrites per image, releases each dropped image with its SUCCESSOR's fence, and hands the
    /// caller the last fence it already gave away. `setBuffer` then transfers that stale fd to
    /// SurfaceFlinger, which closes it a second time — an `fdsan` `SIGABRT` on the decode thread
    /// the moment a burst gives it two images to collapse.
    fn drain_reader(&mut self) {
        // Each evicted image returns with its acquire fence, so the codec cannot reuse it early.
        while let Some((acq, decoded_mono)) = self.acquire() {
            let pts_us = acq.pts_us;
            self.store
                .push(acq, pts_us, decoded_mono, &mut self.counters, drop);
        }
    }

    /// Acquire the next image and pair its decode stamps: the image and its monotonic decode
    /// instant. A fence still pending here is an early output (the pixels are not written yet),
    /// counted as `fenced`. `None` when the reader is empty or a transient acquire error occurs.
    fn acquire(&mut self) -> Option<(Acquired, i64)> {
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
        if fence
            .as_ref()
            .is_some_and(|f| fence_signal_ns(f.as_fd()).is_none())
        {
            self.fenced += 1;
        }
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
        let (decoded_real, decoded_mono) = take_by_pts(&mut self.stamps, pts_us)
            .unwrap_or((now_realtime_ns(), now_monotonic_ns()));
        let acquired = Acquired {
            image: Some(image),
            buffer,
            fence,
            pts_us,
            decoded_real,
        };
        Some((acquired, decoded_mono))
    }

    /// Read every present fence that has signalled, oldest first; a pending one stops the scan
    /// (order is what the intervals measure). One that outlives its patience never signals and
    /// is recorded off its latch instead. Runs on every completion and on every vsync tick, so
    /// the last frame before a pause is scored off its fence, not off its latch 200 ms later.
    fn poll_fences(
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

    /// One frame on glass at `present_ns`: the scheduler scores it ([`Slots::on_present`]; a
    /// `real` vsync is a present fence), then record the display metrics, the decoder's fence
    /// tail, and publish the glass-to-glass figure.
    fn on_present(
        &mut self,
        a: PresentSample,
        present_ns: i64,
        real: bool,
        clock_offset: i64,
        stats: &crate::stats::VideoStats,
        video_e2e: &AtomicU64,
    ) {
        if let Some(err_us) =
            self.slots
                .on_present(present_ns, a.latch_ns, a.slot, real, &mut self.counters)
        {
            self.phase_err_us.push(err_us);
        }
        // Apply → on glass: what the HUD and the log line call `latch`, which is SurfaceFlinger's
        // whole share and not its latch instant.
        let on_glass_ns = (present_ns - a.release_mono).clamp(0, crate::stats::SANE_NS as i64);
        let displayed_real = a.release_real + on_glass_ns as i128;
        let latch_use = (on_glass_ns / 1000) as u64;
        self.latch_us.push(latch_use);
        // On glass means the acquire fence resolved: its signal minus the output callback is the
        // codec time an early output hides from `decode`; an error means the display showed an
        // unfinished buffer.
        if let Some(acq) = a.acquire.as_ref() {
            let decoded_mono = a.release_mono - (a.release_real - a.decoded_real) as i64;
            match fence_signal_ns(acq.as_fd()) {
                Some(done) => self
                    .fence_tail_us
                    .push(((done - decoded_mono).max(0) / 1000) as u64),
                None if fence_errored(acq.as_fd()) => self.fence_err += 1,
                None => {}
            }
        }
        let pts_ns = a.pts_us.saturating_mul(1000);
        // Published RAW for the audio plane to align against.
        if let Some(us) = crate::stats::publish_e2e(video_e2e, displayed_real, clock_offset, pts_ns)
        {
            self.e2e_us.push(us);
        }
        stats.note_displayed(pts_ns, a.decoded_real, a.release_real, displayed_real);
    }

    /// Return every held image before the reader and codec go away. Unpresented images retain
    /// their acquire fences; SurfaceFlinger owns refs to the presented images until done.
    fn release_held(&mut self) {
        self.store.drain().for_each(drop);
        self.presented.clear();
        self.awaiting.clear();
    }

    /// Tear down everything but the picture window's layer, which keeps showing its last frame. A
    /// rebuilt decoder hides it once its own layer presents (see [`Layer::hide`]). The second
    /// window's layer goes dark here; the rebuilt backend makes its own.
    fn into_layer(mut self) -> Layer {
        self.release_held();
        if let Some(mut s) = self.second.take() {
            s.hide();
        }
        self.layer
    }

    /// The HDR10 volume sent with every subsequent transaction.
    pub(crate) fn set_hdr_meta(&mut self, meta: Option<punktfunk_core::quic::HdrMeta>) {
        self.hdr_meta = meta;
    }

    /// Wake the decode loop when a rendered frame reaches the reader. Codec2 queues it inside
    /// the render call, OMX later on ACodec's looper — without a wake the pass that could
    /// present it is the next AU, vsync or 5 ms timeout.
    pub(crate) fn wake_on_image(&mut self, tx: mpsc::Sender<DecodeEvent>) {
        let wake = Box::new(move |_: &ImageReader| {
            let _ = tx.send(DecodeEvent::ImageAvailable);
        });
        if let Err(e) = self.reader.set_image_listener(wake) {
            log::warn!("asc: image listener not set ({e:?}) — frames wait for the next wake");
        }
    }

    /// The decoded picture's size, which the layer's source rect crops against. The reader was
    /// sized at the session's first mode; an in-session mode change keeps the same reader.
    fn set_src_size(&mut self, w: i32, h: i32) {
        if (self.src_w, self.src_h) != (w.max(1), h.max(1)) {
            self.src_w = w.max(1);
            self.src_h = h.max(1);
            log::info!("asc: source picture now {w}x{h}");
        }
    }

    /// Update the `ADataSpace` applied to every subsequent transaction (a refinement from the
    /// codec's output format; the negotiated colour set the initial value at create). Ignored
    /// before Android 12.
    fn set_dataspace(&mut self, dataspace: i32) {
        if self.refine && self.dataspace != dataspace {
            self.dataspace = dataspace;
            log::info!("asc: buffer dataspace now {dataspace:#x}");
        }
    }
}

impl Sink for AscSink {
    fn is_asc(&self) -> bool {
        true
    }

    fn needs_clock(&self) -> bool {
        true
    }

    /// The pump composites the rendered images onto the layer, and the display stage is measured
    /// there from the real transaction latches. `skipped` here is the gate-withheld frames only;
    /// the store's drops ride [`Sink::flush`].
    fn on_outputs(
        &mut self,
        ctx: &Ctx,
        ready: Vec<OutputReady>,
        shows: &mut dyn FnMut(u64) -> bool,
    ) -> Routed {
        let mut r = Routed::default();
        self.outputs += ready.len() as u64;
        for o in ready {
            let present = shows(o.pts_us);
            r.skipped += u64::from(!present);
            self.on_output(
                &ctx.codec,
                o.index,
                o.pts_us,
                o.decoded_ns,
                o.decoded_mono_ns,
                present,
            );
        }
        r
    }

    /// The ASC sink phases on its own present fences and takes only the panel period from the
    /// choreographer.
    fn pump(&mut self, ctx: &Ctx, clock: Option<&VsyncShared>) -> bool {
        let Some(tx) = ctx.present_tx.as_ref() else {
            return false;
        };
        let panel = clock.map_or(0, VsyncShared::panel_period_ns);
        self.present_due(now_monotonic_ns(), panel, tx)
    }

    /// Publish the store's drops to the HUD every pass and emit the 1 Hz `pf.present` line,
    /// its counters per second. The second feeds the [`Verdicts`]: completions are live on
    /// every device, and a `Skipping` verdict turns slot pacing off for the session.
    fn flush(&mut self, ctx: &Ctx, _clock: Option<&VsyncShared>, lost: bool) -> Option<Verdict> {
        let stats = &ctx.stats;
        self.lost |= lost;
        if self.counters.skipped > 0 {
            let skipped = std::mem::take(&mut self.counters.skipped);
            self.skipped += skipped;
            stats.note_skipped(skipped);
        }
        if self.last_flush.elapsed() < Duration::from_secs(1) {
            return None;
        }
        self.last_flush = Instant::now();
        let skipped = std::mem::take(&mut self.skipped);
        let second = Second {
            released: self.counters.released,
            displays: self.counters.displays,
            skipped,
            outputs: std::mem::take(&mut self.outputs),
            confirms_live: true,
            unbroken: !std::mem::take(&mut self.lost),
        };
        let mut verdict = self.verdicts.second(second, self.slots.clock_period_ns());
        if let Some(Verdict::Skipping { permille }) = verdict {
            log::warn!(
                "presenter: verdict skipping {permille}‰ over 5 s on an unbroken stream — \
                 as-soon-as-possible targets for this session"
            );
            self.slots.pacing_off();
            verdict = None;
        }
        let c = &mut self.counters;
        if c.released == 0 && c.displays == 0 {
            return verdict; // idle
        }
        let (latch_p50, latch_max) = p50_max_ms(std::mem::take(&mut self.latch_us));
        let (pace_p50, pace_max) = p50_max_ms(std::mem::take(&mut self.pace_us));
        let (e2e_p50, e2e_max) = p50_max_ms(std::mem::take(&mut self.e2e_us));
        let (err_p50, err_max) = p50_max_ms(std::mem::take(&mut self.phase_err_us));
        let (tail_p50, tail_max) = p50_max_ms(std::mem::take(&mut self.fence_tail_us));
        let intervals = std::mem::take(&mut c.intervals);
        let cadence = cadence_suffix(self.store.cadence_health());
        stats.note_cadence(intervals.judder_permille(), c.coalesced);
        log::info!(
            target: "pf.present",
            "asc released={} displays={} skipped={} inflight={} qDepth={} paceMs p50={:.2} \
             max={:.2} latchMs p50={:.2} max={:.2} e2eMs p50={:.2} max={:.2} panelMs={:.2} \
             leadMs={:.2} marginUs={} held={} slotMiss={} coalesced={} n0={} n1={} n2={} n3+={} \
             judder={}‰ mode={} phaseErrMs p50={:.2} max={:.2} forced={} unlatched={} fence={} \
             fenced={} fenceErr={} fenceTailMs p50={:.2} max={:.2}{}",
            c.released,
            c.displays,
            skipped,
            self.budget.inflight(),
            self.store.fifo_len(),
            pace_p50,
            pace_max,
            latch_p50,
            latch_max,
            e2e_p50,
            e2e_max,
            self.slots.clock_period_ns() as f64 / 1e6,
            self.slots.lead_ns() as f64 / 1e6,
            self.slots.margin_ns() / 1000,
            c.held,
            c.slot_miss,
            c.coalesced,
            intervals.n0,
            intervals.n1,
            intervals.n2,
            intervals.n3p,
            intervals.judder_permille(),
            intervals.mode(),
            err_p50,
            err_max,
            c.forced,
            c.unlatched,
            u8::from(self.slots.fence_live()),
            std::mem::take(&mut self.fenced),
            std::mem::take(&mut self.fence_err),
            tail_p50,
            tail_max,
            cadence,
        );
        c.released = 0;
        c.displays = 0;
        c.forced = 0;
        c.unlatched = 0;
        c.held = 0;
        c.slot_miss = 0;
        c.coalesced = 0;
        verdict
    }

    /// Fences that signalled since the last completion, and the HDR volume: the host re-sends
    /// the source's grade on capture start and keyframes.
    fn on_vsync(&mut self, ctx: &Ctx) {
        self.poll_fences(ctx.offset(), &ctx.stats, &ctx.video_e2e);
        if ctx.client.video_format().color.is_hdr() {
            self.set_hdr_meta(ctx.client.latest_hdr_meta(Duration::ZERO));
        }
    }

    /// Reopen the budget, queue the present sample behind its fence (or record it off the latch
    /// when there is none), and free the buffer this frame replaced with its release fence. Runs
    /// on the decode thread; the callback only forwarded the data.
    fn on_present_complete(&mut self, pc: PresentComplete, clock_offset: i64, ctx: &Ctx) {
        let (stats, video_e2e) = (&*ctx.stats, &*ctx.video_e2e);
        self.budget.completed();
        self.slots.note_latch(pc.latch_ns, &mut self.counters);
        if pc.latch_ns > 0 {
            if let Some(p) = self.presented.iter_mut().find(|p| p.seq == pc.seq) {
                let sample = PresentSample {
                    slot: p.slot,
                    latch_ns: pc.latch_ns,
                    pts_us: p.pts_us,
                    decoded_real: p.decoded_real,
                    release_real: p.release_real,
                    release_mono: p.release_mono,
                    acquire: p.acquire.take(),
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

    /// ASC carries the colour on the transaction, not the SurfaceView window. Neither the stream
    /// nor the codec naming a colour leaves the negotiated dataspace standing.
    fn on_format_changed(
        &mut self,
        ctx: &Ctx,
        picture: Option<(i32, i32)>,
        stream: Option<ColorInfo>,
    ) {
        if let Some((w, h)) = picture {
            self.set_src_size(w, h);
        }
        if let Some(ds) = stream_dataspace(&ctx.codec, stream) {
            self.set_dataspace(i32::from(ds));
        }
    }

    fn reset_cadence(&mut self) {
        self.store.reset_cadence();
    }

    /// Drop every held image back to the reader pool before it goes away.
    fn release_all(&mut self, _codec: &MediaCodec) {
        self.release_held();
    }

    fn into_stale_layer(self: Box<Self>) -> Option<Layer> {
        Some((*self).into_layer())
    }
}
