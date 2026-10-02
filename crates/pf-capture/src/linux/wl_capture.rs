//! Direct `ext-image-copy-capture-v1` capture, with no portal and no PipeWire.
//!
//! The compositor fills a buffer *we* allocate and answers `ready`; we re-arm at once.
//! Nothing paces the loop, so the delivered rate is the compositor's repaint rate and a
//! frame's age is the copy itself. On Hyprland that measured 165 fps against the portal's
//! 82, and 0.63 ms against 3.76 (`design/linux-consumer-driven-capture.md` §8).
//!
//! The protocol holds an outstanding request open until the content changes, so a still
//! desktop costs nothing and delivers nothing — the same shape the PipeWire path gets from
//! a compositor that paints on damage.

use super::gbm_pool::{render_node_for, GbmPool};
use super::{CaptureSignals, FrameSlot};
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, DmabufFrame, FramePayload, PixelFormat};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

// One module per protocol XML: the scanner emits the same private helper names into each,
// so two `generate_interfaces!` in one module collide. `icc` names an interface from
// `src_proto`, so both its code and its interface table import that module.
pub mod src_proto {
    #![allow(clippy::too_many_arguments, missing_docs)]
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/ext-image-capture-source-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/ext-image-capture-source-v1.xml");
}

pub mod icc {
    #![allow(clippy::too_many_arguments, missing_docs)]
    use super::src_proto::*;
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use super::super::src_proto::__interfaces::*;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/ext-image-copy-capture-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/ext-image-copy-capture-v1.xml");
}

pub mod dmabuf {
    #![allow(clippy::too_many_arguments, missing_docs)]
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/linux-dmabuf-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/linux-dmabuf-v1.xml");
}

use dmabuf::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1 as BufferParams;
use dmabuf::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1 as LinuxDmabuf;
use icc::ext_image_copy_capture_frame_v1::{
    Event as FrameEvent, ExtImageCopyCaptureFrameV1 as CaptureFrame,
};
use icc::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1 as CaptureManager;
use icc::ext_image_copy_capture_session_v1::{
    Event as SessionEvent, ExtImageCopyCaptureSessionV1 as CaptureSession,
};
use src_proto::ext_image_capture_source_v1::ExtImageCaptureSourceV1 as CaptureSource;
use src_proto::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1 as OutputSourceManager;

/// Buffers in the pool. Two are the working pair (one being filled, one in flight); the
/// rest absorb an encoder that holds a frame past the next capture. Above this the extra
/// memory buys nothing — the compositor only ever fills one at a time.
const POOL: usize = 4;

/// How long a capture may go without a `ready` before the stream is called dead. Long
/// enough that an idle desktop (which legitimately delivers nothing) never trips it; the
/// stream loop's own liveness check is what notices a genuinely stalled source.
const POLL_SLICE: std::time::Duration = std::time::Duration::from_millis(100);

/// Which buffers the encoder still holds, and the pipe that wakes the loop when one
/// comes back. A returned buffer must re-arm at once or the capture rate halves.
struct FreeList {
    free: Mutex<Vec<usize>>,
    /// Written by a dropping [`BufHold`] on whatever thread the encoder used.
    wake_w: OwnedFd,
}

impl FreeList {
    fn take(&self) -> Option<usize> {
        self.free.lock().ok()?.pop()
    }

    fn put(&self, idx: usize) {
        if let Ok(mut f) = self.free.lock() {
            f.push(idx);
        }
        // SAFETY: `wake_w` is this struct's live pipe write end; `write` reads one byte
        // from a local and touches no Rust memory beyond it. A full pipe already has a
        // pending wakeup, so a short write is correct to ignore.
        unsafe {
            let b = 1u8;
            libc::write(self.wake_w.as_raw_fd(), std::ptr::addr_of!(b).cast(), 1);
        }
    }
}

/// Returns its buffer to the pool when the last clone drops.
struct BufHold {
    list: Arc<FreeList>,
    idx: usize,
}

impl Drop for BufHold {
    fn drop(&mut self) {
        self.list.put(self.idx);
    }
}

/// Everything the Wayland callbacks mutate, in one place: `wayland-client` dispatches
/// against a single state value.
struct State {
    // Globals, bound once.
    source_mgr: Option<OutputSourceManager>,
    capture_mgr: Option<CaptureManager>,
    linux_dmabuf: Option<LinuxDmabuf>,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,

    // Session constraints, valid once `done` has landed.
    size: Option<(u32, u32)>,
    dmabuf_dev: Option<u64>,
    /// `(fourcc, modifiers)` in the order advertised.
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
    constraints_done: bool,
    stopped: bool,

    // Per-frame.
    /// Set by `ready`; the loop publishes and re-arms.
    ready: bool,
    failed: Option<u32>,
    /// Compositor's presentation stamp for the frame in flight, ns.
    presented_ns: Option<u64>,

    /// A renegotiation (the session re-sends constraints) invalidates the pool.
    resized: bool,
}

impl State {
    fn output_named(&self, name: &str) -> Option<&wl_output::WlOutput> {
        self.outputs
            .iter()
            .find(|(_, n)| n.as_deref() == Some(name))
            .map(|(o, _)| o)
    }
}

macro_rules! ignore_dispatch {
    ($($t:ty),+ $(,)?) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(
                _: &mut Self,
                _: &$t,
                _: <$t as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )+};
}
ignore_dispatch!(
    OutputSourceManager,
    CaptureManager,
    CaptureSource,
    LinuxDmabuf,
    BufferParams,
    wl_buffer::WlBuffer,
    wl_shm::WlShm,
);

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        st: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "ext_output_image_capture_source_manager_v1" => {
                st.source_mgr = Some(registry.bind(name, 1.min(version), qh, ()));
            }
            "ext_image_copy_capture_manager_v1" => {
                st.capture_mgr = Some(registry.bind(name, 1.min(version), qh, ()));
            }
            "zwp_linux_dmabuf_v1" => {
                // v3 is where `create_immed` lands; nothing above it is needed here.
                if version >= 3 {
                    st.linux_dmabuf = Some(registry.bind(name, 3, qh, ()));
                }
            }
            "wl_output" => {
                // v4 carries `name`, which is how the host addresses its own head.
                let o: wl_output::WlOutput = registry.bind(name, 4.min(version), qh, ());
                st.outputs.push((o, None));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        st: &mut Self,
        out: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(slot) = st.outputs.iter_mut().find(|(o, _)| o == out) {
                slot.1 = Some(name);
            }
        }
    }
}

impl Dispatch<CaptureSession, ()> for State {
    fn event(
        st: &mut Self,
        _: &CaptureSession,
        event: SessionEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            SessionEvent::BufferSize { width, height } => {
                if st.constraints_done && st.size != Some((width, height)) {
                    // A second constraint batch is a mode change: the pool no longer fits.
                    st.resized = true;
                }
                st.size = Some((width, height));
            }
            SessionEvent::DmabufDevice { device } => {
                let mut dev = [0u8; 8];
                let n = device.len().min(8);
                dev[..n].copy_from_slice(&device[..n]);
                st.dmabuf_dev = Some(u64::from_ne_bytes(dev));
            }
            SessionEvent::DmabufFormat { format, modifiers } => {
                let mods = modifiers
                    .chunks_exact(8)
                    .map(|c| u64::from_ne_bytes(c.try_into().unwrap_or([0; 8])))
                    .collect();
                st.dmabuf_formats.push((format, mods));
            }
            SessionEvent::Done => st.constraints_done = true,
            SessionEvent::Stopped => st.stopped = true,
            _ => {}
        }
    }
}

impl Dispatch<CaptureFrame, ()> for State {
    fn event(
        st: &mut Self,
        _: &CaptureFrame,
        event: FrameEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            FrameEvent::Ready => st.ready = true,
            FrameEvent::Failed { reason } => {
                st.failed = Some(match reason {
                    WEnum::Value(v) => v as u32,
                    WEnum::Unknown(v) => v,
                });
            }
            FrameEvent::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let secs = ((tv_sec_hi as u64) << 32) | tv_sec_lo as u64;
                st.presented_ns = Some(secs * 1_000_000_000 + tv_nsec as u64);
            }
            _ => {}
        }
    }
}

fn new_state() -> Result<State> {
    Ok(State {
        source_mgr: None,
        capture_mgr: None,
        linux_dmabuf: None,
        outputs: Vec::new(),
        size: None,
        dmabuf_dev: None,
        dmabuf_formats: Vec::new(),
        constraints_done: false,
        stopped: false,
        ready: false,
        failed: None,
        presented_ns: None,
        resized: false,
    })
}

/// Pick the format to allocate, and the modifiers to allocate it with.
///
/// Preference is the encoder's, not ours: `want` is what the consumer imports today.
/// Each fourcc needs an exact consumer list; missing or empty rejects that format.
fn choose_format(
    offered: &[(u32, Vec<u64>)],
    want: &[u32],
    importable: &[(u32, Vec<u64>)],
) -> Option<(u32, Vec<u64>)> {
    for w in want {
        let Some((fourcc, mods)) = offered.iter().find(|(f, _)| f == w) else {
            continue;
        };
        let Some((_, accepted)) = importable.iter().find(|(f, _)| f == w) else {
            continue;
        };
        let mut usable: Vec<u64> = mods
            .iter()
            .copied()
            .filter(|m| accepted.contains(m))
            .collect();
        usable.sort_by_key(|m| (*m != 0, *m));
        usable.dedup();
        if !usable.is_empty() {
            return Some((*fourcc, usable));
        }
    }
    None
}

/// Does this session's encoder need the EGL/CUDA importer?
///
/// libva and PyroWave's Vulkan import a dmabuf themselves. NVENC needs it for the modifier
/// list, and takes CUDA from it when its raw lane is off. Same split the PipeWire path makes
/// through `ZeroCopyPolicy`.
fn needs_cuda_import(policy: &crate::ZeroCopyPolicy) -> bool {
    policy.backend_is_gpu && !policy.backend_is_vaapi && !policy.pyrowave_session
}

fn fourcc_to_pixel(fourcc: u32) -> Option<PixelFormat> {
    // `XR24`/`AR24` are little-endian BGRx/BGRA, which is what the encoders ingest.
    match &fourcc.to_le_bytes() {
        b"XR24" => Some(PixelFormat::Bgrx),
        b"AR24" => Some(PixelFormat::Bgra),
        b"XB24" => Some(PixelFormat::Rgbx),
        b"AB24" => Some(PixelFormat::Rgba),
        _ => None,
    }
}

pub(super) struct WlHandles {
    pub(super) slot: FrameSlot,
    pub(super) wake: std::sync::mpsc::Receiver<()>,
    pub(super) signals: CaptureSignals,
    pub(super) quit: Arc<AtomicBool>,
    pub(super) join: std::thread::JoinHandle<()>,
}

/// Spawn the capture thread for `output_name`.
pub(super) fn spawn(
    output_name: String,
    policy: crate::ZeroCopyPolicy,
    slot: FrameSlot,
    signals: CaptureSignals,
) -> Result<WlHandles> {
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let quit = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
    let join = std::thread::Builder::new()
        .name("punktfunk-wl-capture".into())
        .spawn({
            let (slot, signals, quit) = (slot.clone(), signals.clone(), quit.clone());
            move || {
                if let Err(e) = run(&output_name, policy, slot, wake_tx, &signals, &quit, ready_tx)
                {
                    // Teardown races the loop: the host drops the encoder (and with it the
                    // import worker) while a capture is in flight. Once `quit` is set that
                    // is shutdown, not a fault, and must not mark the capturer broken.
                    if quit.load(Ordering::Relaxed) {
                        tracing::debug!(error = %format!("{e:#}"), "direct wayland capture stopped during teardown");
                    } else {
                        tracing::error!(error = %format!("{e:#}"), "direct wayland capture failed");
                        signals.broken.store(true, Ordering::Relaxed);
                    }
                }
                signals.streaming.store(false, Ordering::Relaxed);
            }
        })
        .context("spawn wayland capture thread")?;
    // The caller must not see a capturer whose session never started: a failure here is
    // the signal to fall back to the portal, and that decision cannot be taken later.
    match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(Ok(())) => {}
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
    }
    Ok(WlHandles {
        slot,
        wake: wake_rx,
        signals,
        quit,
        join,
    })
}

#[allow(clippy::too_many_arguments)]
fn run(
    output_name: &str,
    policy: crate::ZeroCopyPolicy,
    slot: FrameSlot,
    wake: SyncSender<()>,
    signals: &CaptureSignals,
    quit: &AtomicBool,
    started: std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    // One pipe for both waits below: the bounded constraints wait, and the frame loop's
    // "a buffer came back" wakeup.
    let (quit_pipe_r, wake_w) = pipe().context("wakeup pipe")?;
    let conn = Connection::connect_to_env().context("wayland connect")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = new_state()?;
    queue.roundtrip(&mut st).context("registry roundtrip")?;
    queue.roundtrip(&mut st).context("output-name roundtrip")?;

    let setup = (|| -> Result<_> {
        let source_mgr = st.source_mgr.clone().ok_or_else(|| {
            anyhow!("compositor has no ext_output_image_capture_source_manager_v1")
        })?;
        let capture_mgr = st
            .capture_mgr
            .clone()
            .ok_or_else(|| anyhow!("compositor has no ext_image_copy_capture_manager_v1"))?;
        let linux_dmabuf = st
            .linux_dmabuf
            .clone()
            .ok_or_else(|| anyhow!("compositor has no zwp_linux_dmabuf_v1 (v3+)"))?;
        let output = st
            .output_named(output_name)
            .ok_or_else(|| {
                anyhow!(
                    "no wl_output named {output_name} (have: {:?})",
                    st.outputs
                        .iter()
                        .filter_map(|(_, n)| n.clone())
                        .collect::<Vec<_>>()
                )
            })?
            .clone();
        Ok((source_mgr, capture_mgr, linux_dmabuf, output))
    })();
    let (source_mgr, capture_mgr, linux_dmabuf, output) = match setup {
        Ok(v) => v,
        Err(e) => {
            let _ = started.send(Err(e));
            return Ok(());
        }
    };

    let source = source_mgr.create_source(&output, &qh, ());
    // `options = 0`: no `paint_cursors`. The pointer rides the host's own cursor plane
    // (`CaptureSignals::cursor_live`), same as every other Linux source.
    let session = capture_mgr.create_session(
        &source,
        icc::ext_image_copy_capture_manager_v1::Options::empty(),
        &qh,
        (),
    );
    // Constraints arrive as a batch ending in `done`. Bounded: a `blocking_dispatch` here
    // would never return for a compositor that answers nothing, and `spawn`'s startup
    // timeout joins this thread — so a hang here would hang the caller too.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while !st.constraints_done && !st.stopped && std::time::Instant::now() < until {
        conn.flush().context("wayland flush")?;
        wait_readable(&conn, &quit_pipe_r, std::time::Duration::from_millis(50))?;
        queue
            .dispatch_pending(&mut st)
            .context("session dispatch")?;
    }
    if !st.constraints_done {
        let _ = started.send(Err(anyhow!("capture session sent no buffer constraints")));
        return Ok(());
    }

    // NVENC's raw lane converts the held dmabuf into its slot in one pass, as libva and
    // PyroWave import it themselves. Once this identity's raw latch or tiled refusal trips,
    // NVENC takes CUDA from the EGL/CUDA import instead. The importer is built before the
    // pool either way: it names the allocatable modifiers. One importer, one worker process.
    let raw_lane = policy.nvenc_raw_dmabuf
        && !signals.health.raw_disabled()
        && !signals.health.passthrough_tiled_refused();
    let mut importer = if needs_cuda_import(&policy) {
        match pf_zerocopy::Importer::new_for_capture() {
            Ok(i) => Some(i),
            Err(e) => {
                let _ = started.send(Err(anyhow!(
                    "this session's encoder needs the GPU importer, which did not start: {e:#}"
                )));
                return Ok(());
            }
        }
    } else {
        None
    };
    let build = build_pool(&st, importer.as_mut(), &policy);
    let (pool, fourcc, format, (w, h)) = match build {
        Ok(v) => v,
        Err(e) => {
            let _ = started.send(Err(e));
            return Ok(());
        }
    };

    // Wrap each dmabuf as a wl_buffer once; they live as long as the pool.
    let mut buffers: Vec<wl_buffer::WlBuffer> = Vec::with_capacity(pool.bos.len());
    for bo in &pool.bos {
        let params = linux_dmabuf.create_params(&qh, ());
        // An implicit layout (`INVALID`) goes out as-is: the compositor rejects it rather
        // than this side guessing one.
        let m = bo.modifier;
        params.add(
            bo.fd.as_fd(),
            0,
            bo.offset,
            bo.stride,
            (m >> 32) as u32,
            (m & 0xffff_ffff) as u32,
        );
        buffers.push(params.create_immed(
            w as i32,
            h as i32,
            fourcc,
            dmabuf::zwp_linux_buffer_params_v1::Flags::empty(),
            &qh,
            (),
        ));
        params.destroy();
    }

    let free = Arc::new(FreeList {
        free: Mutex::new((0..pool.bos.len()).collect()),
        wake_w,
    });

    signals.streaming.store(true, Ordering::Relaxed);
    signals.negotiated.store(true, Ordering::Relaxed);
    signals
        .frame_size
        .store((u64::from(w) << 32) | u64::from(h), Ordering::Relaxed);
    tracing::info!(
        output = output_name,
        w,
        h,
        fourcc = format_args!("{:#010x}", fourcc),
        modifier = pool.bos.first().map(|b| b.modifier).unwrap_or(0),
        pool = pool.bos.len(),
        raw_lane,
        "direct wayland capture: the compositor fills our dmabufs, no portal in the path"
    );
    let _ = started.send(Ok(()));

    let mut frame: Option<(CaptureFrame, usize)> = None;
    let mut delivered: u64 = 0;
    // Clocks drift by microseconds over a long session; re-paired on the same cadence as
    // the portal path's provenance window.
    let mut rt_minus_mono_ns = super::pipewire::realtime_minus_monotonic_ns();
    let mut repaired = std::time::Instant::now();
    while !quit.load(Ordering::Relaxed) && !st.stopped {
        // Arm whenever nothing is outstanding and a buffer is free. A returned buffer
        // wakes the poll below, so this runs the moment the encoder lets go.
        if frame.is_none() {
            if let Some(idx) = free.take() {
                let f = session.create_frame(&qh, ());
                f.attach_buffer(&buffers[idx]);
                f.damage_buffer(0, 0, w as i32, h as i32);
                f.capture();
                st.ready = false;
                st.failed = None;
                st.presented_ns = None;
                frame = Some((f, idx));
            }
        }
        if repaired.elapsed() >= std::time::Duration::from_secs(30) {
            rt_minus_mono_ns = super::pipewire::realtime_minus_monotonic_ns();
            repaired = std::time::Instant::now();
        }
        conn.flush().context("wayland flush")?;
        wait_readable(&conn, &quit_pipe_r, POLL_SLICE)?;
        drain(&quit_pipe_r);
        queue.dispatch_pending(&mut st).context("dispatch")?;
        if let Some(reason) = st.failed.take() {
            if let Some((f, idx)) = frame.take() {
                f.destroy();
                free.put(idx);
            }
            bail!("compositor failed the capture frame (reason {reason})");
        }
        if st.resized {
            bail!("capture source changed size — rebuilding the capture");
        }
        if st.ready {
            st.ready = false;
            let Some((f, idx)) = frame.take() else {
                continue;
            };
            f.destroy();
            let bo = &pool.bos[idx];
            let modifier = if bo.modifier == pf_zerocopy::gbm::DRM_FORMAT_MOD_INVALID {
                0
            } else {
                bo.modifier
            };
            let payload = if let Some(imp) = importer.as_mut().filter(|_| !raw_lane) {
                // The import reads the buffer synchronously here, so the buffer goes
                // straight back to the pool: the CUDA payload owns whatever it needed.
                let plane = pf_zerocopy::DmabufPlane {
                    fd: bo.fd.as_raw_fd(),
                    offset: bo.offset,
                    stride: bo.stride,
                };
                let (kind, modifier) = match modifier {
                    0 => (pf_zerocopy::ImportKind::Linear, None),
                    m => (pf_zerocopy::ImportKind::Tiled, Some(m)),
                };
                let imported = imp.import(kind, &plane, w, h, fourcc, modifier);
                free.put(idx);
                match imported {
                    Ok(buf) => FramePayload::Cuda(buf),
                    Err(e) => bail!("GPU import of the captured dmabuf failed: {e:#}"),
                }
            } else {
                // The frame owns and closes the dup; the pool keeps its own fd.
                let fd = bo
                    .fd
                    .try_clone()
                    .context("dup the capture dmabuf (raise the host's NOFILE)")?;
                let hold: pf_frame::FrameHold = Arc::new(BufHold {
                    list: free.clone(),
                    idx,
                });
                FramePayload::Dmabuf(DmabufFrame {
                    fd,
                    fourcc,
                    modifier,
                    offset: bo.offset,
                    stride: bo.stride,
                    plane1: None,
                    hold: Some(hold),
                    health: signals.health.clone(),
                    rebuild: signals.broken.clone(),
                })
            };
            // The compositor stamps `presentation_time` on CLOCK_MONOTONIC; the wire
            // speaks realtime-since-epoch. `wire_pts` rebases it and falls back to the
            // delivery stamp when the result is implausible, exactly as the portal path
            // does for `SPA_META_Header`.
            let delivery_ns = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let pts_ns = crate::pts_provenance::wire_pts(
                st.presented_ns.take().map(|p| p as i64),
                delivery_ns,
                rt_minus_mono_ns,
            )
            .pts_ns;
            let captured = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns,
                format,
                payload,
                cursor: signals.cursor_live.lock().ok().and_then(|c| c.clone()),
            };
            if let Ok(mut s) = slot.lock() {
                *s = Some(captured);
            }
            let _ = wake.try_send(());
            delivered += 1;
            if delivered == 1 {
                tracing::info!("direct wayland capture: first frame delivered");
            }
        }
    }
    if let Some((f, idx)) = frame.take() {
        f.destroy();
        free.put(idx);
    }
    session.destroy();
    source.destroy();
    Ok(())
}

/// Intersect each constrained fourcc with its consumer list, then allocate that pool.
fn build_pool(
    st: &State,
    importer: Option<&mut pf_zerocopy::Importer>,
    policy: &crate::ZeroCopyPolicy,
) -> Result<(GbmPool, u32, PixelFormat, (u32, u32))> {
    let (w, h) = st
        .size
        .ok_or_else(|| anyhow!("session sent no buffer size"))?;
    let dev = st
        .dmabuf_dev
        .ok_or_else(|| anyhow!("session offered no dmabuf device (shm-only capture)"))?;
    // Every format gets its own consumer-proved list. LINEAR is always supported by
    // the CUDA Vulkan bridge and remains the safe fallback for direct encoders.
    let want = [u32::from_le_bytes(*b"XR24"), u32::from_le_bytes(*b"AR24")];
    let mut importer = importer;
    let importable: Vec<(u32, Vec<u64>)> = want
        .iter()
        .copied()
        .map(|fourcc| {
            let mut modifiers = if let Some(i) = importer.as_deref_mut() {
                i.supported_modifiers(fourcc)
            } else {
                policy
                    .encoder_modifiers
                    .iter()
                    .find(|(f, _)| *f == fourcc)
                    .map(|(_, m)| m.clone())
                    .unwrap_or_default()
            };
            modifiers.retain(|m| *m != 0);
            modifiers.dedup();
            modifiers.push(0);
            (fourcc, modifiers)
        })
        .collect();
    let (fourcc, mods) =
        choose_format(&st.dmabuf_formats, &want, &importable).ok_or_else(|| {
            anyhow!(
                "no usable dmabuf format: compositor offered {:?}, consumer takes {:?}",
                st.dmabuf_formats
                    .iter()
                    .map(|(f, m)| (format!("{:#010x}", f), m.len()))
                    .collect::<Vec<_>>(),
                want.iter()
                    .map(|f| format!("{f:#010x}"))
                    .collect::<Vec<_>>()
            )
        })?;
    let format = fourcc_to_pixel(fourcc)
        .ok_or_else(|| anyhow!("negotiated fourcc {fourcc:#010x} has no pixel format"))?;
    let node = render_node_for(dev)?;
    let pool = GbmPool::new(node, w, h, fourcc, &mods, POOL)?;
    Ok((pool, fourcc, format, (w, h)))
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: `pipe2` writes exactly two ints into the live local array and returns 0 on
    // success (checked). CLOEXEC keeps the fds out of any child.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    if rc != 0 {
        bail!("pipe2 failed: {}", std::io::Error::last_os_error());
    }
    // SAFETY: both fds come from the successful `pipe2` above and are owned solely here.
    unsafe {
        use std::os::fd::FromRawFd;
        Ok((OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])))
    }
}

fn drain(fd: &OwnedFd) {
    let mut buf = [0u8; 64];
    // SAFETY: a non-blocking read into a live local buffer; a short read or EAGAIN is the
    // expected end and needs no handling.
    while unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) } > 0 {}
}

/// Block until the compositor has events or a buffer came back, whichever first.
///
/// `prepare_read` before the poll is what makes this race-free: events queued between the
/// dispatch and the poll are read here rather than waited on.
fn wait_readable(conn: &Connection, wake: &OwnedFd, timeout: std::time::Duration) -> Result<()> {
    let guard = conn.prepare_read();
    let mut fds = [
        libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: wake.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    // SAFETY: `poll` reads and writes exactly the two live `pollfd`s in this local array.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout.as_millis() as i32) };
    // `None` means events were already pending, and `dispatch_pending` takes those.
    if let Some(g) = guard {
        if rc > 0 && fds[0].revents != 0 {
            let _ = g.read();
        }
    }
    Ok(())
}

use std::os::fd::AsFd;

#[cfg(test)]
mod tests {
    use super::{choose_format, fourcc_to_pixel};
    use pf_frame::PixelFormat;

    const XR24: u32 = u32::from_le_bytes(*b"XR24");
    const AR24: u32 = u32::from_le_bytes(*b"AR24");

    #[test]
    fn the_wanted_format_wins_over_the_order_the_compositor_offered() {
        let offered = vec![(AR24, vec![1, 2]), (XR24, vec![5])];
        let importable = vec![(XR24, vec![5]), (AR24, vec![1, 2])];
        let (f, m) = choose_format(&offered, &[XR24, AR24], &importable).unwrap();
        assert_eq!(
            f, XR24,
            "preference is the consumer's, not the compositor's"
        );
        assert_eq!(m, vec![5]);
    }

    #[test]
    fn modifier_lists_are_scoped_to_their_exact_fourcc() {
        let offered = vec![(XR24, vec![7]), (AR24, vec![3])];
        let importable = vec![(XR24, vec![9]), (AR24, vec![3])];
        assert_eq!(
            choose_format(&offered, &[XR24, AR24], &importable),
            Some((AR24, vec![3]))
        );
    }

    #[test]
    fn missing_or_empty_consumer_lists_reject_the_format() {
        let offered = vec![(XR24, vec![7])];
        assert!(choose_format(&offered, &[XR24], &[]).is_none());
        assert!(choose_format(&offered, &[XR24], &[(XR24, Vec::new())]).is_none());
    }

    #[test]
    fn linear_is_offered_first_because_every_consumer_imports_it() {
        let offered = vec![(XR24, vec![0x0100_0000_0000_0002, 0, 5])];
        let importable = vec![(XR24, vec![5, 0x0100_0000_0000_0002, 0])];
        let (_, m) = choose_format(&offered, &[XR24], &importable).unwrap();
        assert_eq!(m[0], 0, "LINEAR must lead the allocation attempt");
    }

    #[test]
    fn only_the_packed_rgb_fourccs_the_encoders_ingest_map_to_a_pixel_format() {
        assert_eq!(fourcc_to_pixel(XR24), Some(PixelFormat::Bgrx));
        assert_eq!(fourcc_to_pixel(AR24), Some(PixelFormat::Bgra));
        assert_eq!(fourcc_to_pixel(u32::from_le_bytes(*b"NV12")), None);
    }
}
