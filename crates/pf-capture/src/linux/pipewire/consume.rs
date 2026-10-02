//! One PipeWire buffer to one published frame: the wire stamp, the producer fence, then raw
//! passthrough, a held dmabuf for the GPU import, or the CPU de-pad.

use super::hold::{holds_possible, zerocopy_hold_enabled};
use super::plan::{
    gpu_import, passthrough_fallback_action, ImportOutcome, PassthroughFallback,
    PassthroughFallbackAction,
};
use super::queue::{RenderFence, RENDER_GUARD};
use super::UserData;
use crate::linux::pw_cursor::composite_cursor;
use crate::linux::sync_timeline::{plane_count, SyncPoints};
use crate::{CapturedFrame, DmabufFrame, FramePayload, PixelFormat};
use pf_dmabuf::{ReadMap, Share};
use pipewire as pw;
use pw::spa;
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd, RawFd};
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

/// Upper bounds (µs) of the fence-wait histogram; last bucket is overflow.
///
/// ≤100 µs is noise; >1 ms is a stall on a 60 Hz 16.6 ms budget. Coarse on purpose: the
/// question is whether the tail is ~0 or milliseconds.
const FENCE_WAIT_BUCKETS_US: [u64; 6] = [100, 500, 1_000, 2_000, 5_000, 10_000];

/// How long taken frames waited on their renders, arrival to take.
///
/// The producer's buffer is out for that long before encode starts. A `NoFence` majority
/// means nothing fenced the frames, not that the renders were quick.
#[derive(Debug, Default, Clone, Copy)]
pub(in crate::linux) struct FenceWaitStats {
    samples: u64,
    total_us: u64,
    max_us: u64,
    /// Overflow bucket: one past the last bound.
    buckets: [u64; FENCE_WAIT_BUCKETS_US.len() + 1],
    signaled: u64,
    no_fence: u64,
    timed_out: u64,
    /// Older frames passed over for a newer finished one.
    passed: u64,
}

impl FenceWaitStats {
    pub(super) fn record(&mut self, us: u64) {
        self.samples += 1;
        self.total_us += us;
        self.max_us = self.max_us.max(us);
        let idx = FENCE_WAIT_BUCKETS_US
            .iter()
            .position(|&b| us <= b)
            .unwrap_or(FENCE_WAIT_BUCKETS_US.len());
        self.buckets[idx] += 1;
    }

    /// One taken frame. Every 300 under `PUNKTFUNK_PERF`, the line: about 5 s at 60 fps.
    pub(in crate::linux) fn took(&mut self, t: &super::Taken) {
        use pf_dmabuf::fence::WaitOutcome;
        self.record(t.waited.as_micros() as u64);
        self.passed += t.passed as u64;
        match t.outcome {
            WaitOutcome::Signaled => self.signaled += 1,
            WaitOutcome::NoFence => self.no_fence += 1,
            WaitOutcome::TimedOut => self.timed_out += 1,
        }
        if !(pf_host_config::config().perf && self.is_meaningful() && self.samples % 300 == 0) {
            return;
        }
        let q = |p: f64| match self.quantile_bucket_us(p) {
            Some(Some(us)) => format!("<={us}us"),
            Some(None) => format!(
                ">{}us",
                FENCE_WAIT_BUCKETS_US[FENCE_WAIT_BUCKETS_US.len() - 1]
            ),
            None => "n/a".to_string(),
        };
        tracing::info!(
            samples = self.samples,
            mean_us = self.mean_us(),
            max_us = self.max_us,
            p50 = %q(0.50),
            p99 = %q(0.99),
            signaled = self.signaled,
            no_fence = self.no_fence,
            timed_out = self.timed_out,
            passed = self.passed,
            "render fence wait, arrival to take (the producer's buffer is out this long \
             before encode starts)"
        );
    }

    /// Bucket upper bound for the `q`-quantile, µs; inner `None` is overflow.
    /// Counts up to the quantile rather than interpolating.
    pub(super) fn quantile_bucket_us(&self, q: f64) -> Option<Option<u64>> {
        if self.samples == 0 {
            return None;
        }
        // 0-based index of the sample at `q`; q=1.0 is the last sample.
        let target = ((self.samples as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, &count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= target {
                return Some(FENCE_WAIT_BUCKETS_US.get(i).copied());
            }
        }
        Some(None)
    }

    pub(super) fn mean_us(&self) -> u64 {
        self.total_us.checked_div(self.samples).unwrap_or(0)
    }

    /// 100 frames ≈ 1.7 s at 60 fps — past the point one outlier dominates p99.
    pub(super) fn is_meaningful(&self) -> bool {
        self.samples >= 100
    }
}

/// Run the fallback for a broken raw-passthrough frame: pick the action for
/// `(reason, ud.modifier)`, emit its once-per-reason line, and act on it.
/// `true` = the caller falls through to the mmap de-pad; `false` = the frame
/// ends here — dropped, or the tiled offer refused and the capture flagged to
/// rebuild on LINEAR.
fn handle_passthrough_fallback(ud: &mut UserData, reason: PassthroughFallback) -> bool {
    let action = passthrough_fallback_action(reason, ud.modifier);
    // Once per distinct reason (`.process` is per-frame). The running count separates a
    // persistent downgrade from a one-frame hiccup at renegotiation.
    if let Some(frames) = ud.passthrough_fallbacks.note(reason) {
        tracing::warn!(
            frames,
            "zero-copy raw-dmabuf passthrough did not take this frame: {} — {} ({})",
            reason.as_str(),
            match action {
                PassthroughFallbackAction::Cpu => {
                    "it falls back to the CPU capture path, costing a full-resolution mmap \
                     de-pad plus the encoder's own upload on every such frame"
                }
                PassthroughFallbackAction::Drop => {
                    "the frame is DROPPED — the CPU de-pad path needs the negotiated format \
                     too, so nothing streams while this persists"
                }
                PassthroughFallbackAction::DropTiledAndRebuild => {
                    "the tiled offer is refused for this capture identity and it rebuilds on \
                     LINEAR"
                }
            },
            reason.hint()
        );
    }
    match action {
        PassthroughFallbackAction::Cpu => true,
        PassthroughFallbackAction::Drop => false,
        PassthroughFallbackAction::DropTiledAndRebuild => {
            if ud.signals.health.refuse_passthrough_tiled() {
                tracing::warn!(
                    "tiled raw-dmabuf passthrough could not continue ({}) — capture rebuilds \
                     on LINEAR",
                    reason.as_str()
                );
            }
            ud.signals.broken.store(true, Ordering::Relaxed);
            false
        }
    }
}

/// Log a frame-drop reason once per process (`.process` runs per frame).
fn warn_once(msg: &'static str) {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap();
    if !seen.contains(&msg) {
        seen.push(msg);
        tracing::warn!("{msg}");
    }
}

fn supported_data_plane_count(count: u32) -> Option<usize> {
    (1..=2).contains(&count).then_some(count as usize)
}

/// How often the wire-pts provenance line is emitted. Matches the audio plane's stats cadence.
pub(super) const PTS_REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// `CLOCK_REALTIME − CLOCK_MONOTONIC`, ns.
///
/// PipeWire stamps `spa_meta_header.pts` in `CLOCK_MONOTONIC`; the wire speaks realtime-since-epoch.
/// A failed read reports 0, which puts every rebased stamp outside the 50 ms plausibility window
/// and falls the stream back to delivery stamps — the safe direction.
pub(in crate::linux) fn realtime_minus_monotonic_ns() -> i64 {
    let rt = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes one `timespec` through the pointer and touches nothing else;
    // `ts` is a live, properly aligned local. A non-zero return leaves it untouched.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return 0;
    }
    rt - (ts.tv_sec * 1_000_000_000 + ts.tv_nsec)
}

/// `data`'s fd, borrowed for as long as `data` is. `None` when the data carries no fd.
fn data_fd(data: &pw::spa::buffer::Data) -> Option<BorrowedFd<'_>> {
    let fd = RawFd::try_from(data.as_raw().fd)
        .ok()
        .filter(|&fd| fd >= 0)?;
    // SAFETY: `data` borrows a `spa_data` of a buffer this side holds; its fd stays open
    // while that borrow lives, and a non-negative fd is a valid `BorrowedFd`.
    Some(unsafe { BorrowedFd::borrow_raw(fd) })
}

/// A CLOEXEC dup of `data`'s fd, so a published frame keeps the dmabuf past the requeue.
/// `None`: the data carries no fd, or the process is out of descriptors.
fn dup_data_fd(data: &pw::spa::buffer::Data) -> Option<OwnedFd> {
    data_fd(data)?.try_clone_to_owned().ok()
}

/// Whether the selected GPU's driver rounds a linear import pitch: iHD does; an unknown
/// selection is treated as one, a CPU copy being the cheaper mistake.
fn linear_pitch_rounds() -> bool {
    static RULE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *RULE.get_or_init(|| {
        pf_gpu::selected_gpu().is_none_or(|g| g.info.vendor_id == pf_gpu::VENDOR_INTEL)
    })
}

fn packed_frame_geometry(
    width: usize,
    height: usize,
    bytes_per_pixel: usize,
    reported_stride: usize,
) -> Option<(usize, usize, usize, usize)> {
    if height == 0 {
        return None;
    }
    let row = width.checked_mul(bytes_per_pixel)?;
    let stride = if reported_stride == 0 {
        row
    } else {
        reported_stride
    };
    if stride < row {
        return None;
    }
    let needed = stride.checked_mul(height - 1)?.checked_add(row)?;
    let tight = row.checked_mul(height)?;
    Some((row, stride, needed, tight))
}

/// De-pad / import one PipeWire buffer and push it to the encoder.
///
/// Called from `.process` with the newest drained buffer. `datas` uses the same transparent
/// cast as libspa's `Buffer::datas_mut`, so the safe `Data` accessors keep working. `pw_buf`
/// is identity for [`UserData::try_defer`] only — never dereferenced here. `stream` is the
/// stream running this `.process`; `try_defer` requeues a stale held buffer on it.
/// The lanes in order: [`try_passthrough`], [`try_gpu_hold`], then [`cpu_depad`].
pub(super) fn consume_frame(
    ud: &mut UserData,
    spa_buf: *mut spa::sys::spa_buffer,
    pw_buf: *mut pw::sys::pw_buffer,
    stream: *mut pw::sys::pw_stream,
    hdr_pts_ns: Option<i64>,
) {
    // Inactive: skip the de-pad (expensive at 5K).
    if !ud.signals.active.load(Ordering::Relaxed) {
        return;
    }
    // GPU import lost: skip per-frame work until the rebuild tears this stream down.
    if ud.signals.broken.load(Ordering::Relaxed) {
        return;
    }
    // Read before `datas` below borrows the same array mutably.
    // SAFETY: `spa_buf` is the buffer this callback holds.
    let sync_points = unsafe { SyncPoints::of(spa_buf) };
    // SAFETY: the dequeued buffer stays held for this callback. We reject counts outside the
    // one/two-plane formats this function supports before using PipeWire's array pointer;
    // the sync datas behind the planes never enter the slice.
    let datas: &mut [pw::spa::buffer::Data] = unsafe {
        if spa_buf.is_null() || (*spa_buf).datas.is_null() {
            &mut []
        } else if let Some(len) = supported_data_plane_count(plane_count(spa_buf)) {
            std::slice::from_raw_parts_mut((*spa_buf).datas as *mut pw::spa::buffer::Data, len)
        } else {
            &mut []
        }
    };
    if datas.is_empty() {
        return;
    }
    let sz = ud.info.size();
    let (w, h) = (sz.width as usize, sz.height as usize);
    if w == 0 || h == 0 {
        return; // format not negotiated yet
    }

    let pts_ns = stamp_frame(ud, hdr_pts_ns);
    let fence = if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf {
        render_fence(ud, sync_points, data_fd(&datas[0]))
    } else {
        None
    };
    let mut arrival = Arrival {
        datas,
        w,
        h,
        pts_ns,
        pw_buf,
        stream,
        fence,
    };
    if ud.vaapi_passthrough && try_passthrough(ud, &mut arrival) {
        return;
    }
    if try_gpu_hold(ud, &mut arrival) {
        return;
    }
    cpu_depad(ud, arrival);
}

/// One arrival: its planes, size and wire stamp, the identity [`UserData::try_defer`]
/// holds it by, and the fence on its pixels. A lane that publishes the dmabuf passes the
/// fence on; a lane that reads the pixels here waits it out first.
struct Arrival<'a> {
    datas: &'a mut [pw::spa::buffer::Data],
    w: usize,
    h: usize,
    pts_ns: u64,
    pw_buf: *mut pw::sys::pw_buffer,
    stream: *mut pw::sys::pw_stream,
    fence: Option<RenderFence>,
}

/// Wait the render out on the loop thread: the pixels are read before `.process` returns.
fn wait_here(fence: Option<RenderFence>) {
    if let Some(f) = fence {
        let _ = f.wait(RENDER_GUARD);
    }
}

/// The wire stamp for this arrival, taken once before de-pad or import. Sampling at publish
/// put CPU work inside the timestamp and let the three lanes drift apart. Emits the
/// provenance line every [`PTS_REPORT_EVERY`].
fn stamp_frame(ud: &mut UserData, hdr_pts_ns: Option<i64>) -> u64 {
    let delivery_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let hdr_pts_ns = hdr_pts_ns.filter(|&p| p > 0);
    // Observe even if we ship the delivery stamp: which clock is cleaner on this host is the
    // question; a diagnostic that runs only after you trust the answer cannot inform it.
    ud.pts.observe(hdr_pts_ns, delivery_ns);
    let stamp = crate::pts_provenance::wire_pts(
        ud.hdr_pts_enabled.then_some(hdr_pts_ns).flatten(),
        delivery_ns,
        ud.rt_minus_mono_ns,
    );
    if ud.hdr_pts_enabled && hdr_pts_ns.is_some() && !stamp.from_header {
        ud.pts.implausible += 1;
    }
    if ud.pts_reported.elapsed() >= PTS_REPORT_EVERY {
        if let Some(r) = ud.pts.report() {
            tracing::info!(
                frames = r.frames,
                with_hdr = r.with_hdr,
                samples = r.samples,
                period_us = r.period_us,
                // `frames + skipped` ≈ the window's expected count when the
                // producer skipped ticks; a producer running slow moves
                // `period_us` instead and leaves this at 0.
                skipped = r.skipped,
                // Tighter hdr_mad than delivery_mad ⇒ compositor stamp is worth shipping;
                // equally ragged ⇒ the producer composes irregularly and no stamp fixes it.
                hdr_mad_us = r.hdr_mad_us,
                delivery_mad_us = r.delivery_mad_us,
                offset_p50_ms = r.offset_p50_ms,
                implausible = r.implausible,
                hdr_pts_used = ud.hdr_pts_enabled,
                held_drops = ud.held_drops,
                undamaged = ud.undamaged,
                // Buffers the producer sent that never arrived, their release signalled here.
                undelivered = ud.defer.undelivered.load(Ordering::Relaxed),
                // Session total of raw-passthrough frames that took the CPU copy instead.
                cpu_fallbacks = ud.passthrough_fallbacks.frames,
                // Buffers the producer sent again while held (PipeWire < 1.6); each re-held.
                resent = ud.signals.resent.load(Ordering::Relaxed),
                pool_depth = ud.pool.live,
                "capture wire-pts provenance"
            );
        }
        ud.pts.reset_window();
        ud.pts_reported = std::time::Instant::now();
        // Clocks drift by µs over a window; re-pair here so a multi-hour session stays honest.
        ud.rt_minus_mono_ns = realtime_minus_monotonic_ns();
    }
    stamp.pts_ns
}

/// The fence on this buffer's render, for whoever reads the pixels to wait on. `None`:
/// nothing fences them, and a read may see the frame before.
///
/// The render is fenced at `sync`'s acquire point when the stream negotiated explicit sync,
/// else by the dmabuf's implicit fence (none on NVIDIA). Not waited here: this is the loop
/// thread, the only one that hands buffers back.
fn render_fence(
    ud: &UserData,
    sync: Option<SyncPoints>,
    plane: Option<BorrowedFd<'_>>,
) -> Option<RenderFence> {
    static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    if let Some((dev, p)) = ud.sync.as_ref().zip(sync) {
        if ONCE.swap(false, Ordering::Relaxed) {
            tracing::info!(
                "dmabuf explicit sync active (SyncTimeline): frames wait on the producer's \
                 acquire point and hand-back signals its release point"
            );
        }
        // SAFETY: the syncobj fd belongs to the buffer this callback holds.
        let timeline = unsafe { BorrowedFd::borrow_raw(p.acquire_fd) }
            .try_clone_to_owned()
            .ok()?;
        return Some(RenderFence::Acquire {
            dev: dev.clone(),
            timeline,
            point: p.acquire_point,
        });
    }
    match pf_dmabuf::fence::export_sync_file(plane?) {
        Ok(fence) => {
            if ONCE.swap(false, Ordering::Relaxed) {
                tracing::info!(
                    fenced = fence.is_some(),
                    "dmabuf implicit sync active (fenced: frames wait on the driver's fence; \
                     not fenced: a read may see the frame before)"
                );
            }
            fence.map(RenderFence::SyncFile)
        }
        Err(e) => {
            if ONCE.swap(false, Ordering::Relaxed) {
                tracing::warn!(
                    error = %e,
                    "dmabuf EXPORT_SYNC_FILE failed — nothing fences the producer's render, \
                     a read may see the frame before"
                );
            }
            None
        }
    }
}

/// A producer NV12/P010's chroma `(offset, stride)`: plane 1's chunk, in the same buffer
/// object as plane 0. BO identity is inode, not fd number. `Err` = a two-BO frame, which
/// cannot travel the single-fd import; the caller drops it. `Ok(None)` for packed RGB.
#[allow(clippy::result_unit_err)]
fn second_plane(
    fmt: PixelFormat,
    datas: &[pw::spa::buffer::Data],
) -> Result<Option<(u32, u32)>, ()> {
    let planar = matches!(fmt, PixelFormat::Nv12 | PixelFormat::P010);
    if !planar || datas.len() < 2 || datas[1].fd() <= 0 {
        return Ok(None);
    }
    // SAFETY: zeroed `libc::stat` is a valid POD initializer; both fds are owned by the live
    // PipeWire buffer for this callback, and `fstat` only writes the out-param structs, whose
    // fields are read only after the `== 0` success checks.
    let same_bo = unsafe {
        let mut s0: libc::stat = std::mem::zeroed();
        let mut s1: libc::stat = std::mem::zeroed();
        libc::fstat(datas[0].fd(), &mut s0) == 0
            && libc::fstat(datas[1].fd(), &mut s1) == 0
            && (s0.st_dev, s0.st_ino) == (s1.st_dev, s1.st_ino)
    };
    if !same_bo {
        warn_once(
            "the planes live in different buffer objects — frames dropped (single-fd import only)",
        );
        return Err(());
    }
    let c1 = datas[1].chunk();
    Ok(Some((c1.offset(), c1.stride().max(0) as u32)))
}

/// Raw DMA-BUF passthrough: packed RGB for GPU CSC, or producer NV12/P010 without another
/// convert. `true` = the frame ends here, published or dropped; `false` = take the CPU de-pad.
/// A broken frame names its reason: a silent fall-through CPU-touches every frame on a session
/// that negotiated zero-copy.
fn try_passthrough(ud: &mut UserData, a: &mut Arrival) -> bool {
    let (datas, w, h) = (&*a.datas, a.w, a.h);
    let reason = 'passthrough: {
        let Some(fmt) = ud.format else {
            break 'passthrough PassthroughFallback::NoFormat;
        };
        if datas[0].type_() != pw::spa::buffer::DataType::DmaBuf {
            break 'passthrough PassthroughFallback::NotDmabuf;
        }
        let Some(fourcc) = pf_frame::drm_fourcc(fmt) else {
            break 'passthrough PassthroughFallback::NoFourcc;
        };
        let chunk = datas[0].chunk();
        let offset = chunk.offset();
        let stride = chunk.stride().max(0) as u32;
        // Dropped, not downgraded: de-padding as linear would scramble chroma.
        let Ok(plane1) = second_plane(fmt, datas) else {
            return true;
        };
        // iHD reads a linear import at a pitch rounded to 64 bytes, so an odd pitch shears
        // the picture. Mutter's RENDERING-only GBM buffers pad only for SCANOUT. The
        // de-pad copy below uploads through a driver-allocated surface instead.
        if ud.modifier == 0
            && linear_pitch_rounds()
            && (stride % 64 != 0 || plane1.is_some_and(|(_, s)| s % 64 != 0))
        {
            break 'passthrough PassthroughFallback::UnalignedPitch;
        }
        // Dup so the fd outlives SPA recycle. Content stability is `try_defer`: a raw
        // frame is published only under a hold, so the producer can never rewrite a
        // DMA-BUF the encoder still reads. No hold — shallow pool or
        // PUNKTFUNK_ZEROCOPY_HOLD=0 — is a safe CPU fallback, never an unsafe publish.
        let Some(dup) = dup_data_fd(&datas[0]) else {
            break 'passthrough PassthroughFallback::DupFailed;
        };
        let Some(hold) = ud.try_defer(a.pw_buf, a.stream) else {
            // A shortage, not a broken frame: drop it as the import lane does — every hold
            // is with the encoder, the next arrival takes the one that comes back. The CPU
            // copy on this thread starves the requeues that would end the shortage; a
            // tiled rebuild asks KWin for a new output each time.
            // Only a pool that can never hold falls through, or nothing would stream.
            if holds_possible(zerocopy_hold_enabled(), ud.pool.live) {
                ud.held_drops += 1;
                return true;
            }
            break 'passthrough PassthroughFallback::NoHold;
        };
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w as u32,
            height: h as u32,
            pts_ns: a.pts_ns,
            format: fmt,
            payload: FramePayload::Dmabuf(DmabufFrame {
                fd: dup,
                fourcc,
                modifier: ud.modifier,
                offset,
                stride,
                plane1,
                hold: Some(hold),
                health: ud.signals.health.clone(),
                rebuild: ud.signals.broken.clone(),
            }),
            // RGB→NV12 backends blend cursor-as-metadata. Gamescope burns the pointer in;
            // native NV12/P010 has none.
            cursor: ud.cursor.overlay(),
        };
        ud.publish(frame, a.fence.take());
        // Once per geometry, not once: a resize renegotiates the pool, and a stale
        // stride against a new size is a sheared picture.
        static LAST: std::sync::Mutex<(usize, usize, u32, u32)> =
            std::sync::Mutex::new((0, 0, 0, 0));
        let geometry = (w, h, offset, stride);
        let changed = std::mem::replace(
            &mut *LAST.lock().unwrap_or_else(|e| e.into_inner()),
            geometry,
        ) != geometry;
        if changed {
            tracing::info!(
                w,
                h,
                offset,
                stride,
                // The held buffer's own fd: the consumer may already have closed the dup.
                fd_size = data_fd(&datas[0]).map_or(0, |fd| pf_dmabuf::byte_len(fd).unwrap_or(0)),
                modifier = ud.modifier,
                fourcc = format_args!("{:#010x}", fourcc),
                source = match fmt {
                    PixelFormat::Nv12 => "producer-native NV12",
                    PixelFormat::P010 => "producer-native P010",
                    _ => "packed RGB (encoder GPU CSC)",
                },
                "zero-copy: handing the raw DMA-BUF to the encoder"
            );
        }
        return true;
    };
    !handle_passthrough_fallback(ud, reason)
}

/// dmabuf + importer: hand the held buffer to the consumer, which imports at its own tick
/// (`import_held`), so arrivals above the wire rate cost nothing here. A buffer that cannot be
/// held is dropped while holds are possible at all (every hold is with the encoder) and
/// imported here only when this pool can never hold. A producer NV12 rides only a hold, with
/// its chroma plane: the importer reads packed RGB, so an unheld one feeds the raw latch,
/// which withdraws the planar offer. `true` = the frame ends here; `false` = take the CPU
/// de-pad.
fn try_gpu_hold(ud: &mut UserData, a: &mut Arrival) -> bool {
    let (datas, w, h) = (&*a.datas, a.w, a.h);
    let mut gpu_import_broken = false;
    if ud.signals.has_importer.load(Ordering::Relaxed) {
        if let Some(fmt) = ud.format {
            let hdr_tiled = fmt.is_hdr_rgb10() && ud.modifier != 0;
            if hdr_tiled && !ud.hdr_tiled_raw {
                warn_once(
                    "HDR frame arrived with a tiled modifier — the GPU de-tile blit is 8-bit, so \
                     this stream falls back to the CPU path (the producer ignored our LINEAR-only \
                     HDR offer)",
                );
            }
            if datas[0].type_() == pw::spa::buffer::DataType::DmaBuf
                && (!hdr_tiled || ud.hdr_tiled_raw)
            {
                let Some(fourcc) = pf_frame::drm_fourcc(fmt) else {
                    return true; // format has no DRM fourcc mapping — skip the frame
                };
                let plane = pf_zerocopy::DmabufPlane {
                    fd: datas[0].fd(),
                    offset: datas[0].chunk().offset(),
                    stride: datas[0].chunk().stride().max(0) as u32,
                };
                let Ok(plane1) = second_plane(fmt, datas) else {
                    return true;
                };
                if let Some(dup) = dup_data_fd(&datas[0]) {
                    if let Some(hold) = ud.try_defer(a.pw_buf, a.stream) {
                        let frame = CapturedFrame {
                            provenance: Default::default(),
                            width: w as u32,
                            height: h as u32,
                            pts_ns: a.pts_ns,
                            format: fmt,
                            payload: FramePayload::Dmabuf(DmabufFrame {
                                fd: dup,
                                fourcc,
                                modifier: ud.modifier,
                                offset: plane.offset,
                                stride: plane.stride,
                                plane1,
                                hold: Some(hold),
                                health: ud.signals.health.clone(),
                                rebuild: ud.signals.broken.clone(),
                            }),
                            cursor: ud.cursor.overlay(),
                        };
                        ud.publish(frame, a.fence.take());
                        return true;
                    }
                    if holds_possible(zerocopy_hold_enabled(), ud.pool.live) {
                        ud.held_drops += 1;
                        return true;
                    }
                }
                if matches!(fmt, PixelFormat::Nv12 | PixelFormat::P010) {
                    let health = &ud.signals.health;
                    if health.note_raw_import_failure(ud.modifier, "producer NV12 without a hold") {
                        ud.signals.broken.store(true, Ordering::Relaxed);
                    }
                    return true;
                }
                wait_here(a.fence.take());
                let cell = ud.signals.importer.clone();
                let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(importer) = guard.as_mut() {
                    match gpu_import(
                        importer,
                        ud.import_policy,
                        &mut ud.import_state,
                        &ud.signals,
                        fmt,
                        w as u32,
                        h as u32,
                        plane,
                        ud.modifier,
                    ) {
                        ImportOutcome::Frame(devbuf, out_fmt) => {
                            let frame = CapturedFrame {
                                provenance: Default::default(),
                                width: w as u32,
                                height: h as u32,
                                pts_ns: a.pts_ns,
                                format: out_fmt,
                                payload: FramePayload::Cuda(devbuf),
                                cursor: ud.cursor.overlay(),
                            };
                            ud.publish(frame, None);
                            return true;
                        }
                        ImportOutcome::Dropped => return true,
                        ImportOutcome::ImporterLost => gpu_import_broken = true,
                    }
                }
                if gpu_import_broken {
                    *guard = None;
                }
            }
        }
    }
    if gpu_import_broken {
        ud.signals.has_importer.store(false, Ordering::Relaxed);
    }
    false
}

/// The CPU lane: wait the render out, de-pad one packed plane out of the mapped buffer, blit
/// the pointer unless the host places it, and publish.
fn cpu_depad(ud: &mut UserData, a: Arrival) {
    let Arrival {
        datas,
        w,
        h,
        pts_ns,
        fence,
        ..
    } = a;
    wait_here(fence);
    let d = &mut datas[0];
    // LINEAR dmabufs also land here (gamescope).
    let data_type = d.type_();
    // `mapoffset` is this spa_data's start in the fd — non-zero when one fd is pooled.
    // PipeWire's MAP_BUFFERS slice already starts there; our self-mmap maps from 0, so
    // add it (`region_off`). Skip it and we index the wrong buffer; `needed > avail` cannot
    // catch that because `avail` is the whole-fd mapping.
    let map_off = d.as_raw().mapoffset as usize;
    let (size, chunk_off, stride) = {
        let c = d.chunk();
        (
            c.size() as usize,
            c.offset() as usize,
            c.stride().max(0) as usize,
        )
    };
    let Some(fmt) = ud.format else { return }; // unsupported/not negotiated
                                               // This de-pad assumes one packed plane. `bytes_per_pixel` reports 4 for NV12, so a
                                               // native NV12 buffer (`stride ≈ w`, two planes) always trips `stride < row` and blames
                                               // the producer. The second plane is not in `datas[0]`; arriving here is a host bug
                                               // (NV12 offer without the raw-dmabuf passthrough that consumes it).
    if matches!(fmt, PixelFormat::Nv12 | PixelFormat::P010) {
        warn_once(
            "negotiated a producer-native planar format but this capture fell back to the CPU \
             de-pad path, which handles single-plane packed formats only — frames dropped (the \
             planar offer is only valid under the raw-dmabuf passthrough that imports it)",
        );
        return;
    }
    let bpp = fmt.bytes_per_pixel();
    let Some((row, stride, needed, tight_len)) = packed_frame_geometry(w, h, bpp, stride) else {
        warn_once("invalid or overflowing packed-frame geometry — frames dropped");
        return;
    };
    // dmabuf chunks commonly report size 0; fall back to the computed span.
    let size = if size == 0 { needed } else { size };
    // mmap the fd ourselves at fstat length. xdg-desktop-portal-wlr MemFd reports
    // `data.maxsize` past the mapped bytes — reading to maxsize segfaults. Also covers
    // MAP_BUFFERS skipping Vulkan dmabufs. MemPtr (no fd) is same-process: trust `d.data()`.
    let fd = data_fd(d).filter(|fd| fd.as_raw_fd() > 0);
    let fd_len = fd
        .and_then(|fd| pf_dmabuf::byte_len(fd).ok())
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n > 0);
    // Prefer our fstat-sized mmap; else PipeWire's MAP_BUFFERS slice. `fd_len` is required:
    // falling back to `offset + needed` maps a producer-invented length and can SIGBUS past
    // the object. Without a real length, decline to self-map.
    let mapping = fd
        .zip(fd_len)
        .and_then(|(fd, len)| ReadMap::new(fd, len, Share::Shared).ok());
    let self_mapped = mapping.as_ref().map(ReadMap::bytes);
    // Self-mmap starts at fd offset 0, so this spa_data begins at `mapoffset`; MAP_BUFFERS
    // already begins there. Checked add — both halves are producer-controlled.
    let (buf, region_off): (&[u8], usize) = if let Some(b) = self_mapped {
        match map_off.checked_add(chunk_off) {
            Some(off) => (b, off),
            None => {
                warn_once("mapoffset + chunk offset overflows — frames dropped");
                return;
            }
        }
    } else if let Some(data) = d.data() {
        (data, chunk_off)
    } else {
        warn_once("buffer has no mappable data — frames dropped");
        return;
    };
    // Need stride*(h-1)+row valid bytes within [region_off, region_off+size).
    if region_off > buf.len() {
        return;
    }
    let avail = buf.len() - region_off;
    {
        // First-frame geometry — a compositor/GPU layout mismatch is otherwise silent here.
        use std::sync::atomic::{AtomicBool, Ordering};
        static ONCE: AtomicBool = AtomicBool::new(true);
        if ONCE.swap(false, Ordering::Relaxed) {
            tracing::info!(
                stride, size, chunk_off, map_off, region_off, buf_len = buf.len(), needed,
                data_type = ?data_type, fd_len = ?fd_len, self_mapped = self_mapped.is_some(),
                "capture CPU de-pad geometry (first frame)"
            );
        }
    }
    if needed > avail || needed > size {
        warn_once("buffer smaller than frame span — frames dropped");
        return;
    }
    let region = &buf[region_off..region_off + size.min(avail)];
    let mut tight = vec![0u8; tight_len];
    for y in 0..h {
        tight[y * row..y * row + row].copy_from_slice(&region[y * stride..y * stride + row]);
    }
    // Blit the latched pointer (no-op when hidden or not packed RGB) unless the host places it:
    // a baked copy would double the forwarded or blended one. The producer's hardware cursor
    // plane stays out of the captured buffer.
    if !ud.signals.host_places_cursor.load(Ordering::Relaxed) {
        composite_cursor(&mut tight, w, h, fmt, &ud.cursor);
    }
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: w as u32,
        height: h as u32,
        pts_ns,
        format: fmt,
        payload: FramePayload::Cpu(tight),
        // Already composited into `tight` — nothing for the encoder to blend.
        cursor: None,
    };
    ud.publish(frame, None);
}

#[cfg(test)]
mod tests {
    use super::{
        packed_frame_geometry, supported_data_plane_count, FenceWaitStats, FENCE_WAIT_BUCKETS_US,
    };

    #[test]
    fn only_supported_pipewire_plane_counts_become_slice_lengths() {
        assert_eq!(supported_data_plane_count(0), None);
        assert_eq!(supported_data_plane_count(1), Some(1));
        assert_eq!(supported_data_plane_count(2), Some(2));
        assert_eq!(supported_data_plane_count(3), None);
        assert_eq!(supported_data_plane_count(u32::MAX), None);
    }

    #[test]
    fn packed_frame_geometry_rejects_overflow_and_short_stride() {
        assert_eq!(packed_frame_geometry(4, 3, 4, 0), Some((16, 16, 48, 48)));
        assert_eq!(packed_frame_geometry(4, 3, 4, 15), None);
        assert_eq!(packed_frame_geometry(1, 0, 4, 4), None);
        assert_eq!(packed_frame_geometry(usize::MAX, 2, 4, 0), None);
        assert_eq!(
            packed_frame_geometry(usize::MAX / 2, 3, 1, usize::MAX / 2),
            None
        );
    }

    // A p99 one bucket low would call the wait free; one bucket high would justify moving
    // load-bearing sync off the loop thread for nothing.

    /// An empty histogram must say "no answer", not "zero" — the second looks like p99 ≈ 0.
    #[test]
    fn an_empty_histogram_has_no_quantile() {
        let s = FenceWaitStats::default();
        assert_eq!(s.quantile_bucket_us(0.99), None);
        assert_eq!(s.mean_us(), 0);
        assert!(!s.is_meaningful(), "it must not be trusted yet either");
    }

    /// Every sample in the first bucket must put p99 there too.
    #[test]
    fn an_all_fast_distribution_puts_p99_in_the_first_bucket() {
        let mut s = FenceWaitStats::default();
        for _ in 0..1000 {
            s.record(3);
        }
        assert_eq!(
            s.quantile_bucket_us(0.50),
            Some(Some(FENCE_WAIT_BUCKETS_US[0]))
        );
        assert_eq!(
            s.quantile_bucket_us(0.99),
            Some(Some(FENCE_WAIT_BUCKETS_US[0]))
        );
        assert_eq!(s.mean_us(), 3);
    }

    /// Fast median, heavy tail: p50 stays low and p99 finds the tail. A smear cannot tell them apart.
    #[test]
    fn a_heavy_tail_moves_p99_without_moving_p50() {
        let mut s = FenceWaitStats::default();
        for _ in 0..980 {
            s.record(10); // fast majority
        }
        for _ in 0..20 {
            s.record(6_000); // 2 % of frames stall milliseconds
        }
        assert_eq!(
            s.quantile_bucket_us(0.50),
            Some(Some(FENCE_WAIT_BUCKETS_US[0])),
            "the median is still free"
        );
        assert_eq!(
            s.quantile_bucket_us(0.99),
            Some(Some(10_000)),
            "...but the p99 must land in the 5-10ms bucket, not with the median"
        );
        assert_eq!(s.max_us, 6_000);
    }

    /// Anything past the last edge reports as overflow rather than being clamped into the last
    /// bucket — "worse than 10 ms" is a distinct finding and must not read as "10 ms".
    #[test]
    fn waits_past_the_last_edge_report_as_overflow() {
        let mut s = FenceWaitStats::default();
        s.record(99_000);
        assert_eq!(s.quantile_bucket_us(0.99), Some(None));
    }

    /// Bucket edges are inclusive upper bounds, so a sample exactly ON an edge belongs to that
    /// bucket and not the next one up.
    #[test]
    fn bucket_edges_are_inclusive() {
        for (i, &edge) in FENCE_WAIT_BUCKETS_US.iter().enumerate() {
            let mut s = FenceWaitStats::default();
            s.record(edge);
            assert_eq!(
                s.quantile_bucket_us(1.0),
                Some(Some(edge)),
                "a sample of exactly {edge}us belongs in bucket {i}"
            );
        }
    }
}
