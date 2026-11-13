//! Direct `ext-image-copy-capture-v1` capture, with no portal and no PipeWire. The compositor
//! fills a buffer this side allocates and answers `ready`; the loop re-arms at once, so the
//! delivered rate is the compositor's repaint rate, a frame's age is the copy itself, and a
//! still desktop delivers nothing (`design/linux-consumer-driven-capture.md` §8). HDR rides
//! `wp_color_management_v1`: a BT.2020 PQ output is captured in its own packed 10-bit buffer,
//! with its mastering volume as the stream's HDR10 metadata; the compositor never tone-maps.
//!
//! - `protocol.rs`: the wayland-scanner modules and the state every callback writes.
//! - `session.rs`: connect, bind, read the output's colour, start, build the pool; HDR probes.
//! - `pool.rs`: the client-allocated dmabufs and the free list a dropped hold returns them to.
//! - `frame_loop.rs`: the frame loop's model: what each compositor event does to the frame out.
//! - `pump.rs`: the frame loop's shell: poll, dispatch, step the model, do what it says.
//! - `capturer.rs`: [`WlCapturer`], the encode loop's end of the thread.
//! - `mod.rs`: the thread's spawn and body, and `Direct`, this capture's delivery rules.

use self::pool::{BufHold, FreeList, GbmPool};
use self::protocol::{CaptureSession, CaptureSource, CmOutput, State};
use super::delivery::{
    self, Buffer, GpuLane, ImportOutcome, Lanes, Memory, NoHold, PassthroughFallback, Path,
};
use super::pipewire::RenderFence;
use super::pw_cursor::CursorState;
use super::{FrameSlot, StreamShared};
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, HdrMeta, PixelFormat};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::Duration;
use wayland_client::protocol::{wl_buffer, wl_registry};
use wayland_client::{Connection, EventQueue, QueueHandle};

mod frame_loop;
mod pool;
mod protocol;
mod session;
pub(crate) use session::{hdr_capture_globals, output_is_hdr10};
mod capturer;
mod pump;
pub(crate) use capturer::WlCapturer;

struct WlHandles {
    slot: FrameSlot,
    wake: std::sync::mpsc::Receiver<()>,
    shared: StreamShared,
    quit: Arc<AtomicBool>,
    join: std::thread::JoinHandle<()>,
    /// The output's mastering volume once a 10-bit PQ pool is up; `None` on an SDR capture.
    hdr_meta: Option<HdrMeta>,
}

/// Spawn the capture thread for `output_name`. `want_hdr` asks for the output's packed
/// 10-bit buffer and fails unless its description is BT.2020 PQ; `header_pts` is
/// [`Knobs::header_pts`](crate::knobs::Knobs::header_pts).
fn spawn(
    output_name: String,
    policy: crate::ZeroCopyPolicy,
    want_hdr: bool,
    header_pts: bool,
    slot: FrameSlot,
    shared: StreamShared,
) -> Result<WlHandles> {
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let quit = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Option<HdrMeta>>>();
    let join = std::thread::Builder::new()
        .name("punktfunk-wl-capture".into())
        .spawn({
            let (slot, shared, quit) = (slot.clone(), shared.clone(), quit.clone());
            move || {
                pf_frame::thread_qos::boost_thread_priority(true);
                if let Err(e) = run(
                    &output_name,
                    policy,
                    want_hdr,
                    header_pts,
                    slot,
                    wake_tx,
                    &shared,
                    &quit,
                    ready_tx,
                ) {
                    // Teardown races the loop: the host drops the encoder (and with it the
                    // import worker) while a capture is in flight. Once `quit` is set that
                    // is shutdown, not a fault, and must not mark the capturer broken.
                    if quit.load(Ordering::Relaxed) {
                        tracing::debug!(error = %format!("{e:#}"), "direct wayland capture stopped during teardown");
                    } else {
                        tracing::error!(error = %format!("{e:#}"), "direct wayland capture failed");
                        shared.mark_broken();
                    }
                }
                shared.report(super::StreamStatus::Paused);
            }
        })
        .context("spawn wayland capture thread")?;
    // The caller must not see a capturer whose session never started: a failure here is
    // the signal to fall back to the portal, and that decision cannot be taken later.
    let hdr_meta = match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(meta)) => meta,
        Ok(Err(e)) => {
            quit.store(true, Ordering::Relaxed);
            let _ = join.join();
            return Err(e);
        }
        Err(_) => {
            // Detached, not joined: a wedged compositor holds the thread in a roundtrip that
            // never reads `quit`. It exits once that returns.
            quit.store(true, Ordering::Relaxed);
            drop(join);
            bail!("direct wayland capture did not start within 5s");
        }
    };
    Ok(WlHandles {
        slot,
        wake: wake_rx,
        shared,
        quit,
        join,
        hdr_meta,
    })
}

/// The capture thread's body. A setup failure goes to the opener through `started`, which
/// falls back to the portal; a failure once streaming ends the loop and marks it broken.
/// `Ok` goes out only after `open` stored the status and the format the opener reads.
#[allow(clippy::too_many_arguments)]
fn run(
    output_name: &str,
    policy: crate::ZeroCopyPolicy,
    want_hdr: bool,
    header_pts: bool,
    slot: FrameSlot,
    wake: SyncSender<()>,
    shared: &StreamShared,
    quit: &AtomicBool,
    started: std::sync::mpsc::Sender<Result<Option<HdrMeta>>>,
) -> Result<()> {
    match session::open(output_name, &policy, want_hdr, shared) {
        Ok(opened) => {
            let _ = started.send(Ok(opened.hdr_meta));
            pump::pump(opened, &slot, &wake, shared, quit, header_pts)
        }
        Err(e) => {
            let _ = started.send(Err(e));
            Ok(())
        }
    }
}

/// A capture session ready to stream: what [`session::open`] set up and [`pump::pump`]
/// drives. Fields drop in declaration order: the buffers and the pool before the importer,
/// every proxy before the connection.
struct Opened {
    /// Buffers the encoder does not hold; a returned one writes to `wake_r`.
    free: Arc<FreeList>,
    /// Each pool buffer wrapped as a `wl_buffer` once; they live as long as the pool.
    buffers: Vec<wl_buffer::WlBuffer>,
    pool: GbmPool,
    importer: Option<pf_zerocopy::Importer>,
    /// NVENC converts the held dmabuf itself; the importer only named the modifiers.
    raw_lane: bool,
    fourcc: u32,
    format: PixelFormat,
    size: (u32, u32),
    hdr_meta: Option<HdrMeta>,
    session: CaptureSession,
    source: CaptureSource,
    /// Kept bound to see the head re-lit (`image_description_changed`).
    _cm_out: Option<CmOutput>,
    st: State,
    _registry: wl_registry::WlRegistry,
    qh: QueueHandle<State>,
    queue: EventQueue<State>,
    conn: Connection,
    /// Read end of the wakeup pipe: the setup waits and the frame loop's poll.
    wake_r: OwnedFd,
}

impl Opened {
    /// The frame for buffer `idx`, which the compositor just filled, stamped `pts_ns`. The
    /// CUDA import reads it here and hands it straight back to the pool; the raw lane passes
    /// the dmabuf on under a [`BufHold`] that returns it when the encoder lets go.
    ///
    /// `ready` carries no fence, and a compositor may send it with the copy still queued
    /// behind the dmabuf's implicit fence, which a CUDA read does not wait on. Both lanes
    /// wait it out first, bounded as on the portal path.
    fn deliver(&mut self, idx: usize, pts_ns: u64, shared: &StreamShared) -> Result<CapturedFrame> {
        let bo = &self.pool.bos[idx];
        let fd = bo.fd.as_fd();
        let (w, h) = self.size;
        let buf = Buffer {
            width: w as usize,
            height: h as usize,
            format: Some(self.format),
            pts_ns,
            memory: Memory::DmaBuf,
            fd: Some(fd),
            modifier: match bo.modifier {
                pf_zerocopy::gbm::DRM_FORMAT_MOD_INVALID => 0,
                m => m,
            },
            offset: bo.offset,
            stride: bo.stride,
            size: 0,
            mapoffset: 0,
            mapped: None,
            plane1: None,
            fence: pf_dmabuf::fence::export_sync_file(fd)
                .ok()
                .flatten()
                .map(RenderFence::SyncFile),
        };
        let import = self.importer.is_some() && !self.raw_lane;
        let lanes = Lanes {
            passthrough: !import,
            gpu: import.then_some(GpuLane {
                consumer_imports: false,
                rebuild_tiled_ten_bit: false,
            }),
            cpu: false,
        };
        let mut direct = Direct {
            shared,
            free: &self.free,
            idx,
            fourcc: self.fourcc,
            importer: self.importer.as_mut(),
            frame: None,
        };
        delivery::deliver(lanes, buf, &mut direct);
        direct
            .frame
            .unwrap_or_else(|| Err(anyhow!("direct capture delivered no frame")))
    }
}

/// The direct capture's side of the lanes: a [`BufHold`] per published buffer, and no CPU
/// lane, so a failed import or dup ends the capture and it rebuilds.
struct Direct<'o> {
    shared: &'o StreamShared,
    free: &'o Arc<FreeList>,
    idx: usize,
    fourcc: u32,
    importer: Option<&'o mut pf_zerocopy::Importer>,
    /// What the lanes made of the buffer.
    frame: Option<Result<CapturedFrame>>,
}

impl Path for Direct<'_> {
    fn shared(&self) -> &StreamShared {
        self.shared
    }

    fn hold(&mut self) -> Result<pf_frame::FrameHold, NoHold> {
        Ok(Arc::new(BufHold {
            list: self.free.clone(),
            idx: self.idx,
        }))
    }

    /// The slot carries no fence, so the render is waited out before the frame is handed on.
    fn publish(&mut self, frame: CapturedFrame, fence: Option<RenderFence>) {
        delivery::wait(fence);
        self.frame = Some(Ok(frame));
    }

    /// The compositor paints the pointer in (`PaintCursors`).
    fn cursor(&self) -> Option<pf_frame::CursorOverlay> {
        None
    }

    fn baked_cursor(&self) -> Option<&CursorState> {
        None
    }

    /// Out of descriptors: the capture ends, as a failed import does.
    fn dup_failed(&mut self, error: std::io::Error) {
        let e =
            anyhow::Error::new(error).context("dup the capture dmabuf (raise the host's NOFILE)");
        self.frame = Some(Err(e));
    }

    fn fell_back(&mut self, _reason: PassthroughFallback) -> bool {
        false
    }

    /// A LINEAR or tiled import as the buffer's modifier names it, the buffer back to the pool
    /// at once. A strike per lost capture: the rebuild allocates the same buffers.
    fn import(
        &mut self,
        plane: pf_zerocopy::DmabufPlane,
        fmt: PixelFormat,
        w: u32,
        h: u32,
        modifier: u64,
    ) -> Option<ImportOutcome> {
        let importer = self.importer.as_deref_mut()?;
        let (kind, modifier) = match modifier {
            0 => (pf_zerocopy::ImportKind::Linear, None),
            m => (pf_zerocopy::ImportKind::Tiled, Some(m)),
        };
        let imported = delivery::import_once(
            importer,
            kind,
            &plane,
            w,
            h,
            self.fourcc,
            modifier,
            self.shared,
        );
        self.free.put(self.idx);
        Some(match imported {
            Ok(buf) => ImportOutcome::Frame(buf, fmt),
            Err(e) => {
                self.shared.health.note_gpu_import_lost();
                let lost = anyhow!("GPU import of the captured dmabuf failed: {e:#}");
                self.frame = Some(Err(lost));
                ImportOutcome::Dropped
            }
        })
    }
}
