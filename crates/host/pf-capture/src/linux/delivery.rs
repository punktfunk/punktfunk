//! One captured buffer to one published frame, the same way from the PipeWire stream and the
//! direct capture: hold and publish the dmabuf, import it to CUDA here, or copy and de-pad it.
//! The lanes are the mechanics; each capture's rules reach them through [`Path`].

use super::pipewire::{RenderFence, RENDER_GUARD};
use super::pw_cursor::{composite_cursor, CursorState};
use super::StreamShared;
use crate::{CapturedFrame, FramePayload, PixelFormat};
use pf_dmabuf::{ReadMap, Share};
use std::os::fd::{AsRawFd as _, BorrowedFd, OwnedFd, RawFd};

/// What backs a buffer's plane. Only a dmabuf takes the passthrough or the GPU lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::linux) enum Memory {
    Invalid,
    MemPtr,
    MemFd,
    DmaBuf,
    MemId,
    Unknown,
}

/// One captured buffer, free of PipeWire and GBM types. The fd is borrowed: a lane that
/// publishes the dmabuf dups it.
pub(in crate::linux) struct Buffer<'a> {
    pub width: usize,
    pub height: usize,
    /// `None` until the format is negotiated.
    pub format: Option<PixelFormat>,
    /// The wire stamp, taken by the caller before any lane runs.
    pub pts_ns: u64,
    pub memory: Memory,
    pub fd: Option<BorrowedFd<'a>>,
    /// DRM modifier; 0 = LINEAR.
    pub modifier: u64,
    /// Plane 0's chunk.
    pub offset: u32,
    pub stride: u32,
    /// The chunk's byte count; dmabufs commonly report 0.
    pub size: u32,
    /// This plane's start in its fd: non-zero when one fd is pooled.
    pub mapoffset: u32,
    /// The producer's own mapping, which already starts at `mapoffset`.
    pub mapped: Option<&'a [u8]>,
    /// Plane 1's fd and chunk `(offset, stride)`, when the producer sent one with an fd.
    pub plane1: Option<(RawFd, u32, u32)>,
    /// The render the pixels wait on. A lane that reads them here waits it out first.
    pub fence: Option<RenderFence>,
}

impl Buffer<'_> {
    /// A CLOEXEC dup of the fd, so a published frame keeps the dmabuf past the hand-back.
    fn dup(&self) -> std::io::Result<OwnedFd> {
        self.fd
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EBADF))?
            .try_clone_to_owned()
    }
}

/// Which lanes a capture runs, in this order, until one ends the frame.
#[derive(Debug, Clone, Copy)]
pub(in crate::linux) struct Lanes {
    /// Hold and publish the raw dmabuf for an encoder that imports it.
    pub passthrough: bool,
    pub gpu: Option<GpuLane>,
    /// Map, de-pad and publish CPU pixels.
    pub cpu: bool,
}

/// The GPU lane: hold the dmabuf for the consumer's import, or import it to CUDA here.
#[derive(Debug, Clone, Copy)]
pub(in crate::linux) struct GpuLane {
    /// The consumer imports a held buffer at its own tick; `false` imports on this thread.
    pub consumer_imports: bool,
    /// A tiled 10-bit frame refuses the lane and rebuilds the capture on LINEAR.
    pub rebuild_tiled_ten_bit: bool,
}

/// Why no hold was had for a published dmabuf.
pub(in crate::linux) enum NoHold {
    /// None free while the pool can hold: the frame drops.
    Short,
    /// This pool can never hold: the passthrough's fallback takes the frame.
    Never,
}

/// A capture's side of the lanes: its holds, its frame sink and its rules for what a lane
/// reports. The PipeWire stream and the direct capture each implement it once.
pub(in crate::linux) trait Path {
    fn shared(&self) -> &StreamShared;
    /// A hold that keeps the producer off this buffer until the frame drops.
    fn hold(&mut self) -> Result<pf_frame::FrameHold, NoHold>;
    /// Hand `frame` on. `fence` guards its pixels when the lane did not wait it out.
    fn publish(&mut self, frame: CapturedFrame, fence: Option<RenderFence>);
    /// The pointer a dmabuf or CUDA frame carries for the encoder to place.
    fn cursor(&self) -> Option<pf_frame::CursorOverlay>;
    /// The pointer the CPU copy blits into its pixels; `None` while the host places it.
    fn baked_cursor(&self) -> Option<&CursorState>;
    /// Why this capture refuses the passthrough of a frame the lane could publish.
    fn refuses_passthrough(
        &self,
        _buf: &Buffer,
        _plane1: Option<(u32, u32)>,
    ) -> Option<PassthroughFallback> {
        None
    }
    /// The passthrough published `buf`.
    fn passed_through(&mut self, _buf: &Buffer, _fmt: PixelFormat, _fourcc: u32) {}
    /// The passthrough could not dup the dmabuf; [`PassthroughFallback::DupFailed`] follows.
    fn dup_failed(&mut self, _error: std::io::Error) {}
    /// The passthrough could not take this frame. `true` = the next lane takes it.
    fn fell_back(&mut self, reason: PassthroughFallback) -> bool;
    /// Import `plane` to CUDA on this thread. `None`: no importer, the next lane takes it.
    fn import(
        &mut self,
        plane: pf_zerocopy::DmabufPlane,
        fmt: PixelFormat,
        w: u32,
        h: u32,
        modifier: u64,
    ) -> Option<ImportOutcome>;
}

/// A CUDA import's verdict. `ImporterLost` is a LINEAR failure: the caller retires the
/// importer and the stream continues on the CPU lane.
pub(in crate::linux) enum ImportOutcome {
    Frame(pf_zerocopy::DeviceBuffer, PixelFormat),
    Dropped,
    ImporterLost,
}

/// Why a raw-dmabuf passthrough frame fell through to the next lane.
///
/// Each variant is a different diagnosis. Without this, the `if` fell out silently and a
/// zero-copy session could pay CPU on every frame while logging a healthy open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::linux) enum PassthroughFallback {
    /// No format yet — transient around renegotiation.
    NoFormat,
    /// Producer delivered SHM/MemFd, not a dmabuf.
    NotDmabuf,
    /// Negotiated format has no DRM fourcc, so the encoder cannot describe it.
    NoFourcc,
    /// `F_DUPFD_CLOEXEC` failed (fd-limit, not graphics).
    DupFailed,
    /// A linear pitch off 64 bytes: iHD imports it at a rounded pitch and the picture shears.
    UnalignedPitch,
    /// This pool can never spare a deferred-requeue hold (depth ≤ reserve, or
    /// `PUNKTFUNK_ZEROCOPY_HOLD=0`), so no raw frame is safe to publish. A transient shortage
    /// on a pool that can hold never gets here: `.process` drops that arrival (`held_drops`).
    NoHold,
}

impl PassthroughFallback {
    pub(in crate::linux) fn bit(self) -> u8 {
        match self {
            PassthroughFallback::NoFormat => 1 << 0,
            PassthroughFallback::NotDmabuf => 1 << 1,
            PassthroughFallback::NoFourcc => 1 << 2,
            PassthroughFallback::DupFailed => 1 << 3,
            PassthroughFallback::UnalignedPitch => 1 << 4,
            PassthroughFallback::NoHold => 1 << 5,
        }
    }

    pub(in crate::linux) fn as_str(self) -> &'static str {
        match self {
            PassthroughFallback::NoFormat => "no format negotiated yet",
            PassthroughFallback::NotDmabuf => "the producer delivered an SHM/MemFd buffer",
            PassthroughFallback::NoFourcc => "the negotiated format has no DRM fourcc",
            PassthroughFallback::DupFailed => "F_DUPFD_CLOEXEC failed on the dmabuf fd",
            PassthroughFallback::UnalignedPitch => {
                "the dmabuf's pitch is not a multiple of 64 bytes"
            }
            PassthroughFallback::NoHold => {
                "this producer pool can never spare a deferred-requeue hold"
            }
        }
    }

    /// `NoFormat` drops the frame (CPU path needs a format too); the rest downgrade to CPU.
    pub(in crate::linux) fn falls_back_to_cpu(self) -> bool {
        !matches!(self, PassthroughFallback::NoFormat)
    }

    pub(in crate::linux) fn hint(self) -> &'static str {
        match self {
            PassthroughFallback::NoFormat => {
                "harmless if it stops: the first buffers can arrive before param_changed"
            }
            PassthroughFallback::NotDmabuf => {
                "the compositor accepted the dmabuf offer and is serving memory anyway — check \
                 PUNKTFUNK_FORCE_SHM and the compositor's allocator"
            }
            PassthroughFallback::NoFourcc => {
                "a capture format the encoder path cannot describe — file it, the negotiation \
                 should not have accepted it"
            }
            PassthroughFallback::DupFailed => "out of file descriptors — raise the host's NOFILE",
            PassthroughFallback::UnalignedPitch => {
                "the compositor pads linear buffers only for scanout, and iHD reads an odd pitch \
                 rounded — this width streams through the CPU copy instead of the raw import"
            }
            PassthroughFallback::NoHold => {
                "the pool is at or below the reserve, or PUNKTFUNK_ZEROCOPY_HOLD=0 — every frame \
                 takes the CPU copy rather than letting the producer rewrite a DMA-BUF the \
                 encoder still reads"
            }
        }
    }
}

/// Run `buf` down the lanes `lanes` enables until one ends the frame.
pub(in crate::linux) fn deliver(lanes: Lanes, mut buf: Buffer, path: &mut impl Path) {
    if lanes.passthrough && passthrough(path, &mut buf) {
        return;
    }
    if let Some(gpu) = lanes.gpu {
        if gpu_lane(path, &mut buf, gpu) {
            return;
        }
    }
    if lanes.cpu {
        copy_depad(path, buf);
    }
}

/// Wait the render out on this thread: the pixels are read before the call returns.
pub(in crate::linux) fn wait(fence: Option<RenderFence>) {
    if let Some(f) = fence {
        let _ = f.wait(RENDER_GUARD);
    }
}

/// One dmabuf → CUDA import as `kind`. A success notes the GPU lane's evidence; grading a
/// failure is the caller's.
#[allow(clippy::too_many_arguments)]
pub(in crate::linux) fn import_once(
    importer: &mut pf_zerocopy::Importer,
    kind: pf_zerocopy::ImportKind,
    plane: &pf_zerocopy::DmabufPlane,
    w: u32,
    h: u32,
    fourcc: u32,
    modifier: Option<u64>,
    shared: &StreamShared,
) -> anyhow::Result<pf_zerocopy::DeviceBuffer> {
    let imported = importer.import(kind, plane, w, h, fourcc, modifier);
    if imported.is_ok() {
        let lane = pf_zerocopy::Lane::Gpu;
        shared.health.note(&[pf_zerocopy::Evidence::Imported(lane)]);
    }
    imported
}

/// Log a frame-drop reason once per process (`.process` runs per frame).
pub(in crate::linux) fn warn_once(msg: &'static str) {
    use std::sync::Mutex;
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap();
    if !seen.contains(&msg) {
        seen.push(msg);
        tracing::warn!("{msg}");
    }
}

/// A producer NV12/P010's chroma `(offset, stride)`: plane 1's chunk, in the same buffer
/// object as plane 0. BO identity is inode, not fd number. `Err` = a two-BO frame, which
/// cannot travel the single-fd import; the caller drops it. `Ok(None)` for packed RGB.
#[allow(clippy::result_unit_err)]
fn second_plane(fmt: PixelFormat, buf: &Buffer) -> Result<Option<(u32, u32)>, ()> {
    let planar = matches!(fmt, PixelFormat::Nv12 | PixelFormat::P010);
    let Some((fd1, offset, stride)) = buf.plane1.filter(|_| planar) else {
        return Ok(None);
    };
    let fd0 = buf.fd.map_or(-1, |fd| fd.as_raw_fd());
    // SAFETY: zeroed `libc::stat` is a valid POD initializer; both fds belong to the buffer
    // `buf` describes, which its capture holds for this call, and `fstat` only writes the
    // out-param structs, whose fields are read only after the `== 0` success checks.
    let same_bo = unsafe {
        let mut s0: libc::stat = std::mem::zeroed();
        let mut s1: libc::stat = std::mem::zeroed();
        libc::fstat(fd0, &mut s0) == 0
            && libc::fstat(fd1, &mut s1) == 0
            && (s0.st_dev, s0.st_ino) == (s1.st_dev, s1.st_ino)
    };
    if !same_bo {
        warn_once(
            "the planes live in different buffer objects — frames dropped (single-fd import only)",
        );
        return Err(());
    }
    Ok(Some((offset, stride)))
}

/// What became of a frame offered under a hold ([`publish_held`]).
enum Held {
    /// Published as a held dmabuf: the frame ends here.
    Published,
    /// No hold was free while the pool can hold: dropped.
    Dropped,
    /// This pool can never hold: the caller's fallback takes the frame.
    NoHold,
}

/// Publish `dup` as a held dmabuf, so the producer cannot rewrite a buffer the encoder still
/// reads. The hold is not optional: capture never publishes a buffer without one.
fn publish_held(
    path: &mut impl Path,
    buf: &mut Buffer,
    fmt: PixelFormat,
    fourcc: u32,
    dup: OwnedFd,
    plane1: Option<(u32, u32)>,
) -> Held {
    let hold = match path.hold() {
        Ok(hold) => hold,
        Err(NoHold::Short) => return Held::Dropped,
        Err(NoHold::Never) => return Held::NoHold,
    };
    let shared = path.shared();
    let dmabuf = pf_frame::DmabufFrame {
        fd: dup,
        fourcc,
        modifier: buf.modifier,
        plane1,
        offset: buf.offset,
        stride: buf.stride,
        hold: Some(hold),
        health: shared.health.clone(),
        rebuild: shared.broken.clone(),
    };
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: buf.width as u32,
        height: buf.height as u32,
        pts_ns: buf.pts_ns,
        format: fmt,
        payload: FramePayload::Dmabuf(dmabuf),
        cursor: path.cursor(),
    };
    path.publish(frame, buf.fence.take());
    Held::Published
}

/// Raw DMA-BUF passthrough: packed RGB for GPU CSC, or producer NV12/P010 without another
/// convert. `true` = the frame ends here, published or dropped; `false` = the next lane.
/// A broken frame names its reason to the path, which decides whether the next lane runs.
fn passthrough(path: &mut impl Path, buf: &mut Buffer) -> bool {
    let reason = 'passthrough: {
        let Some(fmt) = buf.format else {
            break 'passthrough PassthroughFallback::NoFormat;
        };
        if buf.memory != Memory::DmaBuf {
            break 'passthrough PassthroughFallback::NotDmabuf;
        }
        let Some(fourcc) = pf_frame::drm_fourcc(fmt) else {
            break 'passthrough PassthroughFallback::NoFourcc;
        };
        // Dropped, not downgraded: de-padding as linear would scramble chroma.
        let Ok(plane1) = second_plane(fmt, buf) else {
            return true;
        };
        if let Some(reason) = path.refuses_passthrough(buf, plane1) {
            break 'passthrough reason;
        }
        let dup = match buf.dup() {
            Ok(dup) => dup,
            Err(e) => {
                path.dup_failed(e);
                break 'passthrough PassthroughFallback::DupFailed;
            }
        };
        match publish_held(path, buf, fmt, fourcc, dup, plane1) {
            Held::Published => {}
            Held::Dropped => return true,
            Held::NoHold => break 'passthrough PassthroughFallback::NoHold,
        }
        path.passed_through(buf, fmt, fourcc);
        return true;
    };
    !path.fell_back(reason)
}

/// dmabuf + importer: hand the held buffer to the consumer, which imports at its own tick,
/// or import it here when the consumer does not or this pool can never hold. A producer
/// NV12 rides only a hold, with its chroma plane: the importer reads packed RGB, so an
/// unheld one feeds the raw latch, which withdraws the planar offer. `true` = the frame ends
/// here; `false` = the next lane.
fn gpu_lane(path: &mut impl Path, buf: &mut Buffer, lane: GpuLane) -> bool {
    let Some(fmt) = buf.format else {
        return false;
    };
    if lane.rebuild_tiled_ten_bit && fmt.is_rgb10() && buf.modifier != 0 {
        let shared = path.shared();
        if shared.health.refuse(pf_zerocopy::Refusal::HdrTiled) {
            tracing::warn!(
                "10-bit frame arrived with a tiled modifier the producer was not offered — \
                 capture rebuilds on LINEAR"
            );
        }
        shared.mark_broken();
        return true;
    }
    if buf.memory != Memory::DmaBuf {
        return false;
    }
    let Some(fourcc) = pf_frame::drm_fourcc(fmt) else {
        return true; // format has no DRM fourcc mapping — skip the frame
    };
    let Ok(plane1) = second_plane(fmt, buf) else {
        return true;
    };
    if let Some(dup) = lane.consumer_imports.then(|| buf.dup().ok()).flatten() {
        if !matches!(
            publish_held(path, buf, fmt, fourcc, dup, plane1),
            Held::NoHold
        ) {
            return true;
        }
    }
    if matches!(fmt, PixelFormat::Nv12 | PixelFormat::P010) {
        let shared = path.shared();
        if shared
            .health
            .note_raw_import_failure(buf.modifier, "producer NV12 without a hold")
        {
            shared.mark_broken();
        }
        return true;
    }
    wait(buf.fence.take());
    let plane = pf_zerocopy::DmabufPlane {
        fd: buf.fd.map_or(-1, |fd| fd.as_raw_fd()),
        offset: buf.offset,
        stride: buf.stride,
    };
    let (w, h) = (buf.width as u32, buf.height as u32);
    match path.import(plane, fmt, w, h, buf.modifier) {
        Some(ImportOutcome::Frame(devbuf, out_fmt)) => {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: buf.pts_ns,
                format: out_fmt,
                payload: FramePayload::Cuda(devbuf),
                cursor: path.cursor(),
            };
            path.publish(frame, None);
            true
        }
        Some(ImportOutcome::Dropped) => true,
        Some(ImportOutcome::ImporterLost) | None => false,
    }
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

/// The CPU lane: wait the render out, de-pad one packed plane out of the mapped buffer (a
/// dmabuf's CPU read bracketed with `DMA_BUF_IOCTL_SYNC`), blit the pointer the path bakes in,
/// and publish.
fn copy_depad(path: &mut impl Path, buf: Buffer) {
    let Buffer {
        width: w,
        height: h,
        format,
        pts_ns,
        memory,
        fd,
        offset,
        stride,
        size,
        mapoffset,
        mapped,
        fence,
        ..
    } = buf;
    wait(fence);
    // LINEAR dmabufs also land here (gamescope).
    // `mapoffset` is this plane's start in the fd — non-zero when one fd is pooled. The
    // producer's own mapping already starts there; our self-mmap maps from 0, so add it
    // (`region_off`). `needed > avail` cannot catch a miss: `avail` is the whole-fd mapping.
    let map_off = mapoffset as usize;
    let (size, chunk_off, stride) = (size as usize, offset as usize, stride as usize);
    let Some(fmt) = format else { return }; // unsupported/not negotiated
                                            // This de-pad assumes one packed plane. `bytes_per_pixel` reports 4 for NV12, so a
                                            // native NV12 buffer would trip `stride < row` and blame the producer. Arriving here is a
                                            // host bug (NV12 offer without the raw-dmabuf passthrough that consumes it).
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
    // MAP_BUFFERS skipping Vulkan dmabufs. MemPtr (no fd) is same-process: trust `mapped`.
    let fd = fd.filter(|fd| fd.as_raw_fd() > 0);
    // After the fence wait: a cached, non-coherent exporter drops stale CPU lines here.
    let cpu_read = fd
        .filter(|_| memory == Memory::DmaBuf)
        .map(pf_dmabuf::CpuRead::begin);
    let fd_len = fd
        .and_then(|fd| pf_dmabuf::byte_len(fd).ok())
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n > 0);
    // Prefer our fstat-sized mmap; else the producer's mapping. `fd_len` is required:
    // falling back to `offset + needed` maps a producer-invented length and can SIGBUS past
    // the object. Without a real length, decline to self-map.
    let mapping = fd
        .zip(fd_len)
        .and_then(|(fd, len)| ReadMap::new(fd, len, Share::Shared).ok());
    let self_mapped = mapping.as_ref().map(ReadMap::bytes);
    // Checked add — both halves are producer-controlled.
    let (bytes, region_off): (&[u8], usize) = if let Some(b) = self_mapped {
        match map_off.checked_add(chunk_off) {
            Some(off) => (b, off),
            None => {
                warn_once("mapoffset + chunk offset overflows — frames dropped");
                return;
            }
        }
    } else if let Some(data) = mapped {
        (data, chunk_off)
    } else {
        warn_once("buffer has no mappable data — frames dropped");
        return;
    };
    // Need stride*(h-1)+row valid bytes within [region_off, region_off+size).
    if region_off > bytes.len() {
        return;
    }
    let avail = bytes.len() - region_off;
    {
        // First-frame geometry — a compositor/GPU layout mismatch is otherwise silent here.
        use std::sync::atomic::{AtomicBool, Ordering};
        static ONCE: AtomicBool = AtomicBool::new(true);
        if ONCE.swap(false, Ordering::Relaxed) {
            tracing::info!(
                stride, size, chunk_off, map_off, region_off, buf_len = bytes.len(), needed,
                data_type = format_args!("DataType::{memory:?}"), fd_len = ?fd_len,
                self_mapped = self_mapped.is_some(),
                "capture CPU de-pad geometry (first frame)"
            );
        }
    }
    if needed > avail || needed > size {
        warn_once("buffer smaller than frame span — frames dropped");
        return;
    }
    let region = &bytes[region_off..region_off + size.min(avail)];
    // Appended row by row, not zero-filled first: the fill is a full-frame memset.
    let mut tight = Vec::with_capacity(tight_len);
    for y in 0..h {
        tight.extend_from_slice(&region[y * stride..y * stride + row]);
    }
    drop(cpu_read);
    // A baked copy would double a pointer the host forwards or blends. The producer's
    // hardware cursor plane stays out of the captured buffer.
    if let Some(cursor) = path.baked_cursor() {
        composite_cursor(&mut tight, w, h, fmt, cursor);
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
    path.publish(frame, None);
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::fd::{AsFd as _, FromRawFd as _};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
    use std::sync::Arc;

    /// A memfd holding `bytes`: a producer's MemFd, or a dmabuf short of the fence ioctl.
    pub(in crate::linux) fn memfd(bytes: &[u8]) -> OwnedFd {
        // SAFETY: the name is a NUL-terminated literal, and a non-negative return is a new fd
        // this test owns alone.
        let fd = unsafe {
            let fd = libc::memfd_create(c"pf-delivery".as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0, "memfd_create");
            OwnedFd::from_raw_fd(fd)
        };
        let mut file = std::fs::File::from(fd.try_clone().unwrap());
        file.write_all(bytes).unwrap();
        fd
    }

    /// `width`x`height` LINEAR BGRx at `stride` in `fd`, stamped 7.
    pub(in crate::linux) fn buffer(
        fd: &OwnedFd,
        memory: Memory,
        (width, height): (usize, usize),
        stride: u32,
    ) -> Buffer<'_> {
        Buffer {
            width,
            height,
            format: Some(PixelFormat::Bgrx),
            pts_ns: 7,
            memory,
            fd: Some(fd.as_fd()),
            modifier: 0,
            offset: 0,
            stride,
            size: 0,
            mapoffset: 0,
            mapped: None,
            plane1: None,
            fence: None,
        }
    }

    pub(in crate::linux) type Verdict = fn() -> ImportOutcome;

    /// Counts the holds its frames dropped.
    struct Release(Arc<AtomicUsize>);

    impl Drop for Release {
        fn drop(&mut self) {
            self.0.fetch_add(1, Relaxed);
        }
    }

    /// A capture that records what the lanes did and answers as told.
    pub(in crate::linux) struct Fake {
        pub shared: StreamShared,
        /// `false`: this pool can never hold.
        pub holds: bool,
        pub released: Arc<AtomicUsize>,
        /// Each published frame, and whether a fence still guarded it.
        pub published: Vec<(CapturedFrame, bool)>,
        pub reasons: Vec<PassthroughFallback>,
        /// The fallback rule: `true` = the next lane takes the frame.
        pub next_lane: Box<dyn Fn(PassthroughFallback) -> bool>,
        /// The import verdict; `None` is no importer.
        pub import: Option<Verdict>,
        /// Each import's `(fd, offset, stride, modifier)`.
        pub imports: Vec<(i32, u32, u32, u64)>,
    }

    impl Fake {
        pub(in crate::linux) fn new() -> Fake {
            static IDENTITY: AtomicU64 = AtomicU64::new(0xde11_0000_0000);
            let health = pf_zerocopy::zero_copy_health(IDENTITY.fetch_add(1, Relaxed));
            Fake {
                shared: StreamShared::new(health),
                holds: true,
                released: Arc::default(),
                published: Vec::new(),
                reasons: Vec::new(),
                next_lane: Box::new(|_| false),
                import: None,
                imports: Vec::new(),
            }
        }

        /// The payloads published, by lane.
        pub(in crate::linux) fn lanes(&self) -> Vec<&'static str> {
            let lane = |(f, _): &(CapturedFrame, bool)| match f.payload {
                FramePayload::Cpu(_) => "cpu",
                FramePayload::Dmabuf(_) => "dmabuf",
                _ => "other",
            };
            self.published.iter().map(lane).collect()
        }
    }

    impl Path for Fake {
        fn shared(&self) -> &StreamShared {
            &self.shared
        }

        fn hold(&mut self) -> Result<pf_frame::FrameHold, NoHold> {
            if !self.holds {
                return Err(NoHold::Never);
            }
            Ok(Arc::new(Release(self.released.clone())))
        }

        fn publish(&mut self, frame: CapturedFrame, fence: Option<RenderFence>) {
            self.published.push((frame, fence.is_some()));
        }

        fn cursor(&self) -> Option<pf_frame::CursorOverlay> {
            None
        }

        fn baked_cursor(&self) -> Option<&CursorState> {
            None
        }

        fn fell_back(&mut self, reason: PassthroughFallback) -> bool {
            self.reasons.push(reason);
            (self.next_lane)(reason)
        }

        fn import(
            &mut self,
            plane: pf_zerocopy::DmabufPlane,
            _fmt: PixelFormat,
            _w: u32,
            _h: u32,
            modifier: u64,
        ) -> Option<ImportOutcome> {
            let verdict = self.import?;
            self.imports
                .push((plane.fd, plane.offset, plane.stride, modifier));
            Some(verdict())
        }
    }

    const CPU: Lanes = Lanes {
        passthrough: false,
        gpu: None,
        cpu: true,
    };

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

    /// The CPU lane maps the memfd itself and reads each row past `mapoffset` plus the chunk
    /// offset, dropping the stride's padding.
    #[test]
    fn the_cpu_lane_depads_a_memfd_from_its_offsets() {
        let bytes: Vec<u8> = (0..40).collect();
        let fd = memfd(&bytes);
        let mut buf = buffer(&fd, Memory::MemFd, (2, 2), 12);
        (buf.mapoffset, buf.offset) = (8, 4);
        let mut fake = Fake::new();
        deliver(CPU, buf, &mut fake);
        let [(frame, fenced)] = &fake.published[..] else {
            panic!("one frame, got {:?}", fake.lanes());
        };
        let FramePayload::Cpu(pixels) = &frame.payload else {
            panic!("a CPU frame");
        };
        let rows: Vec<u8> = (12..20).chain(24..32).collect();
        assert_eq!(pixels, &rows);
        assert_eq!((frame.width, frame.height, frame.pts_ns), (2, 2, 7));
        assert!(frame.cursor.is_none() && !fenced);
    }

    /// The held lane publishes a dup of the dmabuf under the capture's hold, passes the fence
    /// on for the consumer to wait, and the hold drops once, with the frame.
    #[test]
    fn a_held_frame_carries_its_hold_and_drops_it_once() {
        let fd = memfd(&[0; 64]);
        let mut buf = buffer(&fd, Memory::DmaBuf, (4, 4), 16);
        buf.fence = Some(RenderFence::SyncFile(memfd(&[])));
        let lanes = Lanes {
            passthrough: true,
            ..CPU
        };
        let mut fake = Fake::new();
        deliver(lanes, buf, &mut fake);
        let (frame, fenced) = fake.published.pop().expect("a held frame");
        assert!(fake.published.is_empty() && fenced);
        let FramePayload::Dmabuf(held) = &frame.payload else {
            panic!("a dmabuf frame");
        };
        assert_ne!(held.fd.as_raw_fd(), fd.as_raw_fd(), "the frame owns a dup");
        assert_eq!(
            (held.fourcc, held.stride, held.modifier),
            (pf_zerocopy::drm::XR24, 16, 0)
        );
        assert!(held.hold.is_some());
        assert_eq!(fake.released.load(Relaxed), 0);
        drop(frame);
        assert_eq!(fake.released.load(Relaxed), 1);
    }

    /// The GPU lane holds for a consumer that imports, imports here otherwise, and hands a
    /// lost or missing importer's frame to the CPU lane.
    #[test]
    fn the_gpu_lane_holds_or_imports_then_falls_to_the_cpu() {
        let gpu = |consumer_imports| Lanes {
            gpu: Some(GpuLane {
                consumer_imports,
                rebuild_tiled_ten_bit: true,
            }),
            ..CPU
        };
        let fd = memfd(&[0; 64]);
        let raw = fd.as_raw_fd();
        #[rustfmt::skip]
        let rows: [(bool, Option<Verdict>, &[&str], usize); 4] = [
            (true,  Some(|| ImportOutcome::Dropped),      &["dmabuf"], 0),
            (false, Some(|| ImportOutcome::Dropped),      &[],         1),
            (false, Some(|| ImportOutcome::ImporterLost), &["cpu"],    1),
            (false, None,                                 &["cpu"],    0),
        ];
        for (consumer_imports, import, lanes, imports) in rows {
            let mut fake = Fake {
                import,
                ..Fake::new()
            };
            deliver(
                gpu(consumer_imports),
                buffer(&fd, Memory::DmaBuf, (4, 4), 16),
                &mut fake,
            );
            assert_eq!(fake.lanes(), lanes, "consumer imports: {consumer_imports}");
            assert_eq!(fake.imports.len(), imports);
            assert!(fake.imports.iter().all(|&i| i == (raw, 0, 16, 0)));
        }
    }

    /// A tiled 10-bit frame rebuilds the portal capture before any import; the direct
    /// capture, which does not ask for the rebuild, imports it.
    #[test]
    fn a_tiled_ten_bit_frame_rebuilds_only_where_the_lane_asks() {
        let fd = memfd(&[0; 64]);
        for rebuild_tiled_ten_bit in [true, false] {
            let lanes = Lanes {
                passthrough: false,
                gpu: Some(GpuLane {
                    consumer_imports: false,
                    rebuild_tiled_ten_bit,
                }),
                cpu: false,
            };
            let mut buf = buffer(&fd, Memory::DmaBuf, (4, 4), 16);
            (buf.format, buf.modifier) = (Some(PixelFormat::X2Rgb10), 0x0300_0000_0000_0001);
            let mut fake = Fake {
                import: Some(|| ImportOutcome::Dropped),
                ..Fake::new()
            };
            deliver(lanes, buf, &mut fake);
            assert_eq!(fake.shared.broken.load(Relaxed), rebuild_tiled_ten_bit);
            assert_eq!(fake.imports.len(), usize::from(!rebuild_tiled_ten_bit));
        }
    }
}
