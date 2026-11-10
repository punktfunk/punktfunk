//! One PipeWire buffer to one published frame: the wire stamp and the producer fence, then
//! the [`delivery`] lanes under this stream's rules ([`Portal`]).

use super::ledger::{holds_possible, zerocopy_hold_enabled};
use super::plan::{
    gpu_import, passthrough_fallback_action, ImportOutcome, PassthroughFallback,
    PassthroughFallbackAction,
};
use super::queue::RenderFence;
use super::UserData;
use crate::linux::delivery::{self, warn_once, Buffer, GpuLane, Lanes, Memory, NoHold, Path};
use crate::linux::pw_cursor::CursorState;
use crate::linux::sync_timeline::{plane_count, StreamBuffers, SyncPoints};
use crate::linux::{StreamShared, StreamStatus};
use crate::PixelFormat;
use pipewire as pw;
use pw::spa;
use std::os::fd::{BorrowedFd, RawFd};
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
/// `(reason, ud.modifier, planar)`, emit its once-per-reason line, and act on it.
/// `true` = the caller falls through to the mmap de-pad; `false` = the frame
/// ends here — dropped, or the tiled or planar offer refused and the capture
/// flagged to rebuild without it.
fn handle_passthrough_fallback(ud: &mut UserData, reason: PassthroughFallback) -> bool {
    let planar = matches!(ud.format, Some(PixelFormat::Nv12 | PixelFormat::P010));
    let action = passthrough_fallback_action(reason, ud.modifier, planar);
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
                PassthroughFallbackAction::DropPlanarAndRebuild => {
                    "the CPU path reads packed RGB only, so the planar offer is refused for \
                     this capture identity and it rebuilds on packed RGB"
                }
            },
            reason.hint()
        );
    }
    match action {
        PassthroughFallbackAction::Cpu => true,
        PassthroughFallbackAction::Drop => false,
        PassthroughFallbackAction::DropTiledAndRebuild => {
            if ud.shared.health.refuse(pf_zerocopy::Refusal::Tiled) {
                tracing::warn!(
                    "tiled raw-dmabuf passthrough could not continue ({}) — capture rebuilds \
                     on LINEAR",
                    reason.as_str()
                );
            }
            ud.shared.mark_broken();
            false
        }
        PassthroughFallbackAction::DropPlanarAndRebuild => {
            ud.shared.health.refuse(pf_zerocopy::Refusal::Planar);
            ud.shared.mark_broken();
            false
        }
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

/// Whether the VA encoder's driver rounds a linear import pitch: iHD does. The device is the
/// one [`pf_gpu::linux_render_node`] names, which the encoder opens; `selected_gpu` is the
/// console's view and names the NVIDIA card on a hybrid. An unknown node is treated as Intel,
/// a CPU copy being the cheaper mistake.
fn linear_pitch_rounds() -> bool {
    static RULE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *RULE.get_or_init(|| {
        let node = pf_gpu::linux_render_node();
        pf_gpu::enumerate()
            .iter()
            .find(|g| g.handle.render_node.as_deref() == Some(node.as_path()))
            .is_none_or(|g| g.vendor_id == pf_gpu::VENDOR_INTEL)
    })
}

/// De-pad / import one PipeWire buffer and push it to the encoder.
///
/// Called from `.process` with the newest drained buffer. `datas` uses the same transparent
/// cast as libspa's `Buffer::datas_mut`, so the safe `Data` accessors keep working. `pw_buf`
/// is identity for [`UserData::try_defer`] only — never dereferenced here. `stream` is the
/// stream running this `.process`; `try_defer` requeues a stale held buffer on it.
/// The lanes in order: passthrough under VAAPI or PyroWave, the GPU lane while an importer
/// is present, then the CPU de-pad.
pub(super) fn consume_frame(
    ud: &mut UserData,
    spa_buf: *mut spa::sys::spa_buffer,
    pw_buf: *mut pw::sys::pw_buffer,
    stream: &dyn StreamBuffers,
    hdr_pts_ns: Option<i64>,
) {
    // Inactive: skip the de-pad (expensive at 5K).
    if !ud.shared.active.load(Ordering::Relaxed) {
        return;
    }
    // GPU import lost: skip per-frame work until the rebuild tears this stream down.
    if ud.shared.status() == StreamStatus::Broken {
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
            warn_once(
                "the producer's buffers carry a plane count this capture does not read (one or \
                 two) — frames dropped",
            );
            &mut []
        }
    };
    let Some((d0, rest)) = datas.split_first_mut() else {
        return;
    };
    let sz = ud.info.size();
    let (w, h) = (sz.width as usize, sz.height as usize);
    if w == 0 || h == 0 {
        return; // format not negotiated yet
    }

    let pts_ns = stamp_frame(ud, hdr_pts_ns);
    let mut buf = buffer(d0, rest.first(), ud, (w, h), pts_ns);
    if buf.memory == Memory::DmaBuf {
        buf.fence = render_fence(ud, pw_buf as usize, sync_points, buf.fd);
    }
    let importer = &ud.importer;
    let lanes = Lanes {
        passthrough: ud.vaapi_passthrough,
        gpu: importer.present.load(Ordering::Relaxed).then(|| GpuLane {
            // An in-process importer's GL context is current on this thread only.
            consumer_imports: !importer.in_process.load(Ordering::Relaxed),
            rebuild_tiled_ten_bit: !ud.hdr_tiled_raw,
        }),
        cpu: true,
    };
    delivery::deliver(lanes, buf, &mut Portal { ud, pw_buf, stream });
}

/// Plane 0 (`d0`) and plane 1 of one arrival as a [`Buffer`], with no fence yet.
fn buffer<'a>(
    d0: &'a mut pw::spa::buffer::Data,
    d1: Option<&pw::spa::buffer::Data>,
    ud: &UserData,
    (width, height): (usize, usize),
    pts_ns: u64,
) -> Buffer<'a> {
    use pw::spa::buffer::DataType;
    let memory = match d0.type_() {
        DataType::Invalid => Memory::Invalid,
        DataType::MemPtr => Memory::MemPtr,
        DataType::MemFd => Memory::MemFd,
        DataType::DmaBuf => Memory::DmaBuf,
        DataType::MemId => Memory::MemId,
        _ => Memory::Unknown,
    };
    let chunk = d0.chunk();
    let (offset, stride, size) = (chunk.offset(), chunk.stride().max(0) as u32, chunk.size());
    let mapoffset = d0.as_raw().mapoffset;
    let raw_fd = RawFd::try_from(d0.as_raw().fd).ok().filter(|&fd| fd >= 0);
    // SAFETY: `d0` borrows, for `'a`, a `spa_data` of a buffer this callback holds; its fd
    // stays open while that borrow lives, and a non-negative fd is a valid `BorrowedFd`.
    let fd = raw_fd.map(|fd| unsafe { BorrowedFd::borrow_raw(fd) });
    let plane1 = d1.filter(|d| d.fd() > 0).map(|d| {
        let c = d.chunk();
        (d.fd(), c.offset(), c.stride().max(0) as u32)
    });
    Buffer {
        width,
        height,
        format: ud.format,
        pts_ns,
        memory,
        fd,
        modifier: ud.modifier,
        offset,
        stride,
        size,
        mapoffset,
        mapped: d0.data().map(|data| &*data),
        plane1,
        fence: None,
    }
}

/// The PipeWire stream's side of the lanes: the pool ledger's holds, the frame queue, and
/// this stream's fallback and import rules.
struct Portal<'a, 's> {
    ud: &'a mut UserData,
    pw_buf: *mut pw::sys::pw_buffer,
    stream: &'s dyn StreamBuffers,
}

impl Path for Portal<'_, '_> {
    fn shared(&self) -> &StreamShared {
        &self.ud.shared
    }

    /// The hold cap is the pool census, so a shortage means the census is wrong, not that
    /// every hold is with the encoder: the frame drops, the guard for that count. A CPU copy
    /// here would starve the requeues, and a tiled rebuild asks KWin for a new output each
    /// time. Only a pool that can never hold falls through, or nothing would stream.
    fn hold(&mut self) -> Result<pf_frame::FrameHold, NoHold> {
        if let Some(hold) = self.ud.try_defer(self.pw_buf, self.stream) {
            return Ok(hold);
        }
        if holds_possible(zerocopy_hold_enabled(), self.ud.pool.live) {
            self.ud.held_drops += 1;
            return Err(NoHold::Short);
        }
        Err(NoHold::Never)
    }

    fn publish(&mut self, frame: crate::CapturedFrame, fence: Option<RenderFence>) {
        self.ud.publish(frame, fence);
    }

    /// RGB→NV12 backends blend cursor-as-metadata. Gamescope burns the pointer in;
    /// native NV12/P010 has none.
    fn cursor(&self) -> Option<pf_frame::CursorOverlay> {
        self.ud.cursor.overlay()
    }

    /// The latched pointer, unless the host places it: a baked copy would double it.
    fn baked_cursor(&self) -> Option<&CursorState> {
        let host_places = self.ud.cursor_slot.host_places.load(Ordering::Relaxed);
        (!host_places).then_some(&self.ud.cursor)
    }

    /// iHD reads a linear import at a pitch rounded to 64 bytes, so an odd pitch shears
    /// the picture. Mutter's RENDERING-only GBM buffers pad only for SCANOUT. The CPU de-pad
    /// uploads through a driver-allocated surface instead. Only a producer the tiled offer
    /// did not reach hands the VA lane such a buffer: one on another GPU, or after a refusal.
    fn refuses_passthrough(
        &self,
        buf: &Buffer,
        plane1: Option<(u32, u32)>,
    ) -> Option<PassthroughFallback> {
        let odd = buf.stride % 64 != 0 || plane1.is_some_and(|(_, s)| s % 64 != 0);
        (buf.modifier == 0 && self.ud.va_import && odd && linear_pitch_rounds())
            .then_some(PassthroughFallback::UnalignedPitch)
    }

    /// Once per geometry, not once: a resize renegotiates the pool, and a stale stride
    /// against a new size is a sheared picture.
    fn passed_through(&mut self, buf: &Buffer, fmt: PixelFormat, fourcc: u32) {
        static LAST: std::sync::Mutex<(usize, usize, u32, u32)> =
            std::sync::Mutex::new((0, 0, 0, 0));
        let geometry = (buf.width, buf.height, buf.offset, buf.stride);
        let changed = std::mem::replace(
            &mut *LAST.lock().unwrap_or_else(|e| e.into_inner()),
            geometry,
        ) != geometry;
        if changed {
            tracing::info!(
                w = buf.width,
                h = buf.height,
                offset = buf.offset,
                stride = buf.stride,
                // The held buffer's own fd: the consumer may already have closed the dup.
                fd_size = buf.fd.map_or(0, |fd| pf_dmabuf::byte_len(fd).unwrap_or(0)),
                modifier = buf.modifier,
                fourcc = format_args!("{:#010x}", fourcc),
                source = match fmt {
                    PixelFormat::Nv12 => "producer-native NV12",
                    PixelFormat::P010 => "producer-native P010",
                    _ => "packed RGB (encoder GPU CSC)",
                },
                "zero-copy: handing the raw DMA-BUF to the encoder"
            );
        }
    }

    fn fell_back(&mut self, reason: PassthroughFallback) -> bool {
        handle_passthrough_fallback(self.ud, reason)
    }

    /// Through the shared importer, graded by [`gpu_import`]. A LINEAR failure retires it,
    /// and later arrivals take the CPU lane.
    fn import(
        &mut self,
        plane: pf_zerocopy::DmabufPlane,
        fmt: PixelFormat,
        w: u32,
        h: u32,
        modifier: u64,
    ) -> Option<ImportOutcome> {
        let ud = &mut *self.ud;
        let cell = ud.importer.slot.clone();
        let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
        let importer = guard.as_mut()?;
        let policy = ud.import_policy;
        let state = &mut ud.import_state;
        let outcome = gpu_import(
            importer, policy, state, &ud.shared, fmt, w, h, plane, modifier,
        );
        if matches!(outcome, ImportOutcome::ImporterLost) {
            *guard = None;
            drop(guard);
            ud.importer.present.store(false, Ordering::Relaxed);
        }
        Some(outcome)
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
                undelivered = ud.ledger.undelivered.load(Ordering::Relaxed),
                // Session total of raw-passthrough frames that took the CPU copy instead.
                cpu_fallbacks = ud.passthrough_fallbacks.frames,
                // Buffers the producer sent again while held (PipeWire < 1.6); each re-held.
                resent = ud.shared.resent.load(Ordering::Relaxed),
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
/// on the timeline the ledger imported for `buf`, else by the dmabuf's implicit fence (none
/// on NVIDIA). Not waited here: this is the loop thread, the only one that hands buffers back.
fn render_fence(
    ud: &UserData,
    buf: usize,
    sync: Option<SyncPoints>,
    plane: Option<BorrowedFd<'_>>,
) -> Option<RenderFence> {
    static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
    if let Some((_, p)) = ud.sync.as_ref().zip(sync) {
        if ONCE.swap(false, Ordering::Relaxed) {
            tracing::info!(
                "dmabuf explicit sync active (SyncTimeline): frames wait on the producer's \
                 acquire point and hand-back signals its release point"
            );
        }
        let timeline = ud.ledger.timelines(buf, &p).ok()?.acquire;
        return Some(RenderFence::Acquire {
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

#[cfg(test)]
mod tests {
    use super::{supported_data_plane_count, FenceWaitStats, FENCE_WAIT_BUCKETS_US};

    #[test]
    fn only_supported_pipewire_plane_counts_become_slice_lengths() {
        assert_eq!(supported_data_plane_count(0), None);
        assert_eq!(supported_data_plane_count(1), Some(1));
        assert_eq!(supported_data_plane_count(2), Some(2));
        assert_eq!(supported_data_plane_count(3), None);
        assert_eq!(supported_data_plane_count(u32::MAX), None);
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
