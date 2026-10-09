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
//!
//! HDR rides `wp_color_management_v1`: the output's image description says whether it is
//! lit in BT.2020 PQ, and its mastering volume becomes the stream's HDR10 metadata. An HDR
//! session captures the output's own packed 10-bit buffer; the compositor never tone-maps.

use super::gbm_pool::{render_node_for, GbmPool};
use super::{CaptureSignals, FrameSlot};
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, DmabufFrame, FramePayload, HdrMeta, PixelFormat};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};

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

pub mod cm {
    #![allow(clippy::too_many_arguments, missing_docs)]
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/color-management-v1.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/color-management-v1.xml");
}

use cm::wp_color_management_output_v1::{
    Event as CmOutputEvent, WpColorManagementOutputV1 as CmOutput,
};
use cm::wp_color_manager_v1::{Primaries, TransferFunction, WpColorManagerV1 as ColorManager};
use cm::wp_image_description_info_v1::{Event as InfoEvent, WpImageDescriptionInfoV1 as ImageInfo};
use cm::wp_image_description_v1::{Event as DescEvent, WpImageDescriptionV1 as ImageDesc};
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
    color_mgr: Option<ColorManager>,
    outputs: Vec<(wl_output::WlOutput, Option<String>)>,

    // Session constraints, valid once `done` has landed.
    size: Option<(u32, u32)>,
    dmabuf_dev: Option<u64>,
    /// `(fourcc, modifiers)` in the order advertised.
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
    constraints_done: bool,
    stopped: bool,

    // The output's image description, gathered event by event until `done`.
    desc_ready: bool,
    desc_failed: Option<String>,
    color_pending: OutputColor,
    color_done: bool,
    /// The output was re-lit in another encoding: the pool's depth may be wrong now.
    color_changed: bool,

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

/// An output's colour encoding as `wp_image_description_info_v1` reports it. Named
/// enums are kept as their wire values so a compositor's unknown entry still compares.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct OutputColor {
    /// `wp_color_manager_v1.primaries`; 0 = not named.
    pub primaries: u32,
    /// `wp_color_manager_v1.transfer_function`; 0 = not named.
    pub tf: u32,
    /// Mastering primaries, CIE xy × 1e6: red, green, blue, white.
    pub target_primaries: Option<[(i32, i32); 4]>,
    /// Mastering luminance: min cd/m² × 10000, max cd/m².
    pub target_luminance: Option<(u32, u32)>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
}

/// BT.2020 primaries and D65 white, CIE xy × 1e6.
const BT2020_XY: [(i32, i32); 4] = [
    (708_000, 292_000),
    (170_000, 797_000),
    (131_000, 46_000),
    (312_700, 329_000),
];

impl OutputColor {
    /// BT.2020 under the PQ curve — the one encoding the stream's HDR path carries.
    pub(super) fn is_hdr10(&self) -> bool {
        self.primaries == Primaries::Bt2020 as u32 && self.tf == TransferFunction::St2084Pq as u32
    }

    /// ST.2086 + CLL block for this description. Mastering values when the compositor sent
    /// them (Hyprland forwards the panel's EDID); otherwise the generic 1000-nit HDR10 block
    /// the PipeWire path also claims. CIE xy × 1e6 → 1/50000 units is a divide by 20.
    pub(super) fn hdr_meta(&self) -> HdrMeta {
        let xy = |(x, y): (i32, i32)| {
            [
                (x / 20).clamp(0, 50_000) as u16,
                (y / 20).clamp(0, 50_000) as u16,
            ]
        };
        let [r, g, b, w] = self.target_primaries.unwrap_or(BT2020_XY);
        let (min, max) = self.target_luminance.unwrap_or((50, 1000));
        let nits = |v: Option<u32>| v.unwrap_or(0).min(u32::from(u16::MAX)) as u16;
        HdrMeta {
            display_primaries: [xy(g), xy(b), xy(r)],
            white_point: xy(w),
            max_display_mastering_luminance: max.saturating_mul(10_000),
            min_display_mastering_luminance: min,
            max_cll: nits(self.max_cll),
            max_fall: nits(self.max_fall),
        }
    }
}

fn wenum_raw<T: Into<u32>>(v: WEnum<T>) -> u32 {
    match v {
        WEnum::Value(v) => v.into(),
        WEnum::Unknown(v) => v,
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
    ColorManager,
    wl_buffer::WlBuffer,
    wl_shm::WlShm,
);

impl Dispatch<CmOutput, ()> for State {
    fn event(
        st: &mut Self,
        _: &CmOutput,
        event: CmOutputEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // The interface's only event.
        let CmOutputEvent::ImageDescriptionChanged = event;
        st.color_changed = true;
    }
}

impl Dispatch<ImageDesc, ()> for State {
    fn event(
        st: &mut Self,
        _: &ImageDesc,
        event: DescEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            DescEvent::Ready { .. } => st.desc_ready = true,
            DescEvent::Failed { msg, .. } => st.desc_failed = Some(msg),
            _ => {}
        }
    }
}

impl Dispatch<ImageInfo, ()> for State {
    fn event(
        st: &mut Self,
        _: &ImageInfo,
        event: InfoEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let c = &mut st.color_pending;
        match event {
            InfoEvent::PrimariesNamed { primaries } => c.primaries = wenum_raw(primaries),
            InfoEvent::TfNamed { tf } => c.tf = wenum_raw(tf),
            InfoEvent::TargetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => c.target_primaries = Some([(r_x, r_y), (g_x, g_y), (b_x, b_y), (w_x, w_y)]),
            InfoEvent::TargetLuminance { min_lum, max_lum } => {
                c.target_luminance = Some((min_lum, max_lum));
            }
            InfoEvent::TargetMaxCll { max_cll } => c.max_cll = Some(max_cll),
            InfoEvent::TargetMaxFall { max_fall } => c.max_fall = Some(max_fall),
            InfoEvent::Done => st.color_done = true,
            _ => {}
        }
    }
}

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
            // v1 carries everything an output description needs; v2 only renames `ready`.
            "wp_color_manager_v1" => {
                st.color_mgr = Some(registry.bind(name, 1, qh, ()));
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
        color_mgr: None,
        outputs: Vec::new(),
        size: None,
        dmabuf_dev: None,
        dmabuf_formats: Vec::new(),
        constraints_done: false,
        stopped: false,
        desc_ready: false,
        desc_failed: None,
        color_pending: OutputColor::default(),
        color_done: false,
        color_changed: false,
        ready: false,
        failed: None,
        presented_ns: None,
        resized: false,
    })
}

/// Dispatch until `done` holds or `timeout` passes. Bounded: a `blocking_dispatch` never
/// returns for a compositor that answers nothing, and `spawn`'s startup timeout joins the
/// capture thread, so a hang here would hang the caller too.
fn pump_until(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    st: &mut State,
    wake: &OwnedFd,
    timeout: Duration,
    done: impl Fn(&State) -> bool,
) -> Result<bool> {
    let until = Instant::now() + timeout;
    while !done(st) && !st.stopped && Instant::now() < until {
        conn.flush().context("wayland flush")?;
        wait_readable(conn, wake, Duration::from_millis(50))?;
        queue.dispatch_pending(st).context("wayland dispatch")?;
    }
    Ok(done(st))
}

/// The output's current image description, read through `get_information`.
///
/// Returns the `wp_color_management_output_v1` to keep for its `image_description_changed`
/// event, and the description. `Ok(None)` when the compositor cannot describe the output
/// (gone, or an older interface); the caller treats that as SDR.
fn fetch_output_color(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    st: &mut State,
    qh: &QueueHandle<State>,
    mgr: &ColorManager,
    output: &wl_output::WlOutput,
    wake: &OwnedFd,
) -> Result<Option<(CmOutput, OutputColor)>> {
    let cm_out = mgr.get_output(output, qh, ());
    let desc = cm_out.get_image_description(qh, ());
    st.desc_ready = false;
    st.desc_failed = None;
    // `ready` or `failed` lands at once; `get_information` before `ready` is a protocol error.
    if !pump_until(conn, queue, st, wake, Duration::from_secs(2), |s| {
        s.desc_ready || s.desc_failed.is_some()
    })? {
        desc.destroy();
        cm_out.destroy();
        bail!("compositor did not answer the output's image description");
    }
    if let Some(msg) = st.desc_failed.take() {
        desc.destroy();
        cm_out.destroy();
        tracing::debug!(reason = %msg, "output has no colour description — taking it as SDR");
        return Ok(None);
    }
    st.color_pending = OutputColor::default();
    st.color_done = false;
    // `done` is a destructor event: the info object is gone once it lands.
    let _info = desc.get_information(qh, ());
    let done = pump_until(conn, queue, st, wake, Duration::from_secs(2), |s| {
        s.color_done
    })?;
    desc.destroy();
    if !done {
        cm_out.destroy();
        bail!("compositor did not finish the output's colour information");
    }
    Ok(Some((cm_out, st.color_pending)))
}

/// Whether `output_name` is lit in HDR (BT.2020 PQ), by its `wp_color_management_v1`
/// description. `None`: no such output, or a compositor without colour management.
pub(crate) fn output_is_hdr10(output_name: &str) -> Option<bool> {
    let (wake_r, _wake_w) = pipe().ok()?;
    let conn = Connection::connect_to_env().ok()?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = new_state().ok()?;
    queue.roundtrip(&mut st).ok()?;
    queue.roundtrip(&mut st).ok()?;
    let mgr = st.color_mgr.clone()?;
    let output = st.output_named(output_name)?.clone();
    let (cm_out, color) =
        fetch_output_color(&conn, &mut queue, &mut st, &qh, &mgr, &output, &wake_r).ok()??;
    cm_out.destroy();
    let _ = conn.flush();
    Some(color.is_hdr10())
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
    // `XR24`/`AR24` are little-endian BGRx/BGRA, which is what the encoders ingest; the
    // 10-bit pair is the HDR capture, PQ by the output's description.
    match &fourcc.to_le_bytes() {
        b"XR24" => Some(PixelFormat::Bgrx),
        b"AR24" => Some(PixelFormat::Bgra),
        b"XB24" => Some(PixelFormat::Rgbx),
        b"AB24" => Some(PixelFormat::Rgba),
        b"XB30" => Some(PixelFormat::X2Bgr10),
        b"XR30" => Some(PixelFormat::X2Rgb10),
        _ => None,
    }
}

/// Fourccs to allocate, by preference. SDR is the packed-RGB pair every encoder ingests.
/// HDR is the packed 10-bit pair, `XB30` first: NVIDIA has no linear `A2R10G10B10`, and
/// Hyprland hands out `XBGR2101010` for a 10-bit head anyway.
fn wanted_fourccs(hdr: bool) -> [u32; 2] {
    if hdr {
        [u32::from_le_bytes(*b"XB30"), u32::from_le_bytes(*b"XR30")]
    } else {
        [u32::from_le_bytes(*b"XR24"), u32::from_le_bytes(*b"AR24")]
    }
}

// The encode loop's end of the thread `spawn` starts.
mod capturer;
pub(crate) use capturer::WlCapturer;

struct WlHandles {
    slot: FrameSlot,
    wake: std::sync::mpsc::Receiver<()>,
    signals: CaptureSignals,
    quit: Arc<AtomicBool>,
    join: std::thread::JoinHandle<()>,
    /// The output's mastering volume once a 10-bit PQ pool is up; `None` on an SDR capture.
    hdr_meta: Option<HdrMeta>,
}

/// Spawn the capture thread for `output_name`. `want_hdr` asks for the output's packed
/// 10-bit buffer and fails unless its description is BT.2020 PQ.
fn spawn(
    output_name: String,
    policy: crate::ZeroCopyPolicy,
    want_hdr: bool,
    slot: FrameSlot,
    signals: CaptureSignals,
) -> Result<WlHandles> {
    let (wake_tx, wake_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let quit = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Option<HdrMeta>>>();
    let join = std::thread::Builder::new()
        .name("punktfunk-wl-capture".into())
        .spawn({
            let (slot, signals, quit) = (slot.clone(), signals.clone(), quit.clone());
            move || {
                if let Err(e) = run(
                    &output_name,
                    policy,
                    want_hdr,
                    slot,
                    wake_tx,
                    &signals,
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
                        signals.broken.store(true, Ordering::Relaxed);
                    }
                }
                signals.streaming.store(false, Ordering::Relaxed);
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
        signals,
        quit,
        join,
        hdr_meta,
    })
}

/// The capture thread's body. A setup failure goes to the opener through `started`, which
/// falls back to the portal; a failure once streaming ends the loop and marks it broken.
#[allow(clippy::too_many_arguments)]
fn run(
    output_name: &str,
    policy: crate::ZeroCopyPolicy,
    want_hdr: bool,
    slot: FrameSlot,
    wake: SyncSender<()>,
    signals: &CaptureSignals,
    quit: &AtomicBool,
    started: std::sync::mpsc::Sender<Result<Option<HdrMeta>>>,
) -> Result<()> {
    match open(output_name, &policy, want_hdr, signals) {
        Ok(opened) => {
            let _ = started.send(Ok(opened.hdr_meta));
            pump(opened, &slot, &wake, signals, quit)
        }
        Err(e) => {
            let _ = started.send(Err(e));
            Ok(())
        }
    }
}

/// A capture session ready to stream: what [`open`] set up and [`pump`] drives. Fields drop
/// in declaration order: the buffers and the pool before the importer, every proxy before
/// the connection.
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

/// Bind the globals, read the output's colour, start the session and build the pool, up to
/// each buffer wrapped as a `wl_buffer`. Every failure keeps the portal path.
///
/// The streaming and negotiated signals are stored before this returns: the opener must not
/// see a capturer that is not streaming yet.
fn open(
    output_name: &str,
    policy: &crate::ZeroCopyPolicy,
    want_hdr: bool,
    signals: &CaptureSignals,
) -> Result<Opened> {
    // One pipe for both waits below: the bounded constraints wait, and the frame loop's
    // "a buffer came back" wakeup.
    let (wake_r, wake_w) = pipe().context("wakeup pipe")?;
    let conn = Connection::connect_to_env().context("wayland connect")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = new_state()?;
    queue.roundtrip(&mut st).context("registry roundtrip")?;
    queue.roundtrip(&mut st).context("output-name roundtrip")?;

    let source_mgr = st
        .source_mgr
        .clone()
        .ok_or_else(|| anyhow!("compositor has no ext_output_image_capture_source_manager_v1"))?;
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

    // The output's colour comes first: an HDR session has nothing to capture on an SDR head,
    // and the `wp_color_management_output_v1` stays bound to see the head re-lit.
    let color = match st.color_mgr.clone() {
        Some(mgr) => fetch_output_color(&conn, &mut queue, &mut st, &qh, &mgr, &output, &wake_r)?,
        None => None,
    };
    let _cm_out = color.as_ref().map(|(o, _)| o.clone());
    let color = color.map(|(_, c)| c);
    let hdr = match (want_hdr, color) {
        (false, _) => false,
        (true, Some(c)) if c.is_hdr10() => true,
        (true, Some(c)) => bail!(
            "output {output_name} is not lit in HDR (primaries {} / transfer {}) — the \
             session negotiated BT.2020 PQ",
            c.primaries,
            c.tf
        ),
        (true, None) => bail!(
            "compositor describes no colour for output {output_name} (no \
             wp_color_management_v1) — HDR capture needs it"
        ),
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
    // Constraints arrive as a batch ending in `done`.
    if !pump_until(
        &conn,
        &mut queue,
        &mut st,
        &wake_r,
        Duration::from_secs(3),
        |s| s.constraints_done,
    )? {
        bail!("capture session sent no buffer constraints");
    }

    // NVENC's raw lane converts the held dmabuf into its slot in one pass, as libva and
    // PyroWave import it themselves. Once this identity's raw latch or tiled refusal trips,
    // NVENC takes CUDA from the EGL/CUDA import instead. The importer is built before the
    // pool either way: it names the allocatable modifiers. One importer, one worker process.
    let raw_lane = policy.nvenc_raw_dmabuf
        && !signals.health.raw_disabled()
        && !signals.health.passthrough_tiled_refused();
    let mut importer = if needs_cuda_import(policy) {
        let importer = pf_zerocopy::Importer::new_for_capture().map_err(|e| {
            anyhow!("this session's encoder needs the GPU importer, which did not start: {e:#}")
        })?;
        Some(importer)
    } else {
        None
    };
    // Only direct-SDK NVENC reads packed 10-bit PQ off a CUDA import; any other arm would
    // encode the words as garbage.
    if hdr && importer.is_some() && !raw_lane && !policy.hdr_cuda_ok {
        bail!("this session's encoder takes no 10-bit PQ through the GPU importer");
    }
    let (pool, fourcc, format, (w, h)) = build_pool(&st, importer.as_mut(), policy, hdr)?;

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
    signals.hdr_negotiated.store(hdr, Ordering::Relaxed);
    signals
        .frame_size
        .store((u64::from(w) << 32) | u64::from(h), Ordering::Relaxed);
    let hdr_meta = color.filter(|_| hdr).map(|c| c.hdr_meta());
    tracing::info!(
        output = output_name,
        w,
        h,
        fourcc = format_args!("{:#010x}", fourcc),
        modifier = pool.bos.first().map(|b| b.modifier).unwrap_or(0),
        pool = pool.bos.len(),
        raw_lane,
        hdr,
        "direct wayland capture: the compositor fills our dmabufs, no portal in the path"
    );
    Ok(Opened {
        free,
        buffers,
        pool,
        importer,
        raw_lane,
        fourcc,
        format,
        size: (w, h),
        hdr_meta,
        session,
        source,
        _cm_out,
        st,
        _registry,
        qh,
        queue,
        conn,
        wake_r,
    })
}

/// The frame loop: arm a capture whenever none is outstanding and a buffer is free, and hand
/// each ready one to `slot`. Ends on `quit` or a stopped session; a failed frame, a resize
/// or a re-lit head ends it with an error, and the capture rebuilds.
fn pump(
    mut o: Opened,
    slot: &FrameSlot,
    wake: &SyncSender<()>,
    signals: &CaptureSignals,
    quit: &AtomicBool,
) -> Result<()> {
    let (w, h) = o.size;
    let mut frame: Option<(CaptureFrame, usize)> = None;
    let mut delivered: u64 = 0;
    // Clocks drift by microseconds over a long session; re-paired on the same cadence as
    // the portal path's provenance window.
    let mut rt_minus_mono_ns = super::pipewire::realtime_minus_monotonic_ns();
    let mut repaired = std::time::Instant::now();
    while !quit.load(Ordering::Relaxed) && !o.st.stopped {
        // Arm whenever nothing is outstanding and a buffer is free. A returned buffer
        // wakes the poll below, so this runs the moment the encoder lets go.
        if frame.is_none() {
            if let Some(idx) = o.free.take() {
                let f = o.session.create_frame(&o.qh, ());
                f.attach_buffer(&o.buffers[idx]);
                f.damage_buffer(0, 0, w as i32, h as i32);
                f.capture();
                o.st.ready = false;
                o.st.failed = None;
                o.st.presented_ns = None;
                frame = Some((f, idx));
            }
        }
        if repaired.elapsed() >= std::time::Duration::from_secs(30) {
            rt_minus_mono_ns = super::pipewire::realtime_minus_monotonic_ns();
            repaired = std::time::Instant::now();
        }
        o.conn.flush().context("wayland flush")?;
        wait_readable(&o.conn, &o.wake_r, POLL_SLICE)?;
        drain(&o.wake_r);
        o.queue.dispatch_pending(&mut o.st).context("dispatch")?;
        if let Some(reason) = o.st.failed.take() {
            if let Some((f, idx)) = frame.take() {
                f.destroy();
                o.free.put(idx);
            }
            bail!("compositor failed the capture frame (reason {reason})");
        }
        if o.st.resized {
            bail!("capture source changed size — rebuilding the capture");
        }
        if o.st.color_changed {
            bail!("output colour description changed — rebuilding the capture");
        }
        if !o.st.ready {
            continue;
        }
        o.st.ready = false;
        let Some((f, idx)) = frame.take() else {
            continue;
        };
        f.destroy();
        let payload = o.deliver(idx, signals)?;
        // The compositor stamps `presentation_time` on CLOCK_MONOTONIC; the wire
        // speaks realtime-since-epoch. `wire_pts` rebases it and falls back to the
        // delivery stamp when the result is implausible, exactly as the portal path
        // does for `SPA_META_Header`.
        let delivery_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let pts_ns = crate::pts_provenance::wire_pts(
            o.st.presented_ns.take().map(|p| p as i64),
            delivery_ns,
            rt_minus_mono_ns,
        )
        .pts_ns;
        let captured = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns,
            format: o.format,
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
    if let Some((f, idx)) = frame.take() {
        f.destroy();
        o.free.put(idx);
    }
    o.session.destroy();
    o.source.destroy();
    Ok(())
}

impl Opened {
    /// The payload for buffer `idx`, which the compositor just filled. A CUDA import reads
    /// it here and hands it straight back to the pool; the raw lane passes the dmabuf on
    /// under a [`BufHold`] that returns it when the encoder lets go.
    fn deliver(&mut self, idx: usize, signals: &CaptureSignals) -> Result<FramePayload> {
        let bo = &self.pool.bos[idx];
        let modifier = if bo.modifier == pf_zerocopy::gbm::DRM_FORMAT_MOD_INVALID {
            0
        } else {
            bo.modifier
        };
        let (w, h) = self.size;
        if let Some(imp) = self.importer.as_mut().filter(|_| !self.raw_lane) {
            let plane = pf_zerocopy::DmabufPlane {
                fd: bo.fd.as_raw_fd(),
                offset: bo.offset,
                stride: bo.stride,
            };
            let (kind, modifier) = match modifier {
                0 => (pf_zerocopy::ImportKind::Linear, None),
                m => (pf_zerocopy::ImportKind::Tiled, Some(m)),
            };
            let imported = imp.import(kind, &plane, w, h, self.fourcc, modifier);
            self.free.put(idx);
            return match imported {
                Ok(buf) => Ok(FramePayload::Cuda(buf)),
                Err(e) => bail!("GPU import of the captured dmabuf failed: {e:#}"),
            };
        }
        // The frame owns and closes the dup; the pool keeps its own fd.
        let fd = bo
            .fd
            .try_clone()
            .context("dup the capture dmabuf (raise the host's NOFILE)")?;
        let hold: pf_frame::FrameHold = Arc::new(BufHold {
            list: self.free.clone(),
            idx,
        });
        Ok(FramePayload::Dmabuf(DmabufFrame {
            fd,
            fourcc: self.fourcc,
            modifier,
            offset: bo.offset,
            stride: bo.stride,
            plane1: None,
            hold: Some(hold),
            health: signals.health.clone(),
            rebuild: signals.broken.clone(),
        }))
    }
}

/// Intersect each constrained fourcc with its consumer list, then allocate that pool.
/// `hdr` asks the packed 10-bit pair instead of 8-bit RGB.
fn build_pool(
    st: &State,
    importer: Option<&mut pf_zerocopy::Importer>,
    policy: &crate::ZeroCopyPolicy,
    hdr: bool,
) -> Result<(GbmPool, u32, PixelFormat, (u32, u32))> {
    let (w, h) = st
        .size
        .ok_or_else(|| anyhow!("session sent no buffer size"))?;
    let dev = st
        .dmabuf_dev
        .ok_or_else(|| anyhow!("session offered no dmabuf device (shm-only capture)"))?;
    // Every format gets its own consumer-proved list. LINEAR is always supported by
    // the CUDA Vulkan bridge and remains the safe fallback for direct encoders.
    let want = wanted_fourccs(hdr);
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
    use super::{choose_format, fourcc_to_pixel, wanted_fourccs, OutputColor};
    use super::{Primaries, TransferFunction};
    use pf_frame::PixelFormat;

    const XR24: u32 = u32::from_le_bytes(*b"XR24");
    const AR24: u32 = u32::from_le_bytes(*b"AR24");
    const XB30: u32 = u32::from_le_bytes(*b"XB30");
    const XR30: u32 = u32::from_le_bytes(*b"XR30");

    fn pq() -> OutputColor {
        OutputColor {
            primaries: Primaries::Bt2020 as u32,
            tf: TransferFunction::St2084Pq as u32,
            ..OutputColor::default()
        }
    }

    #[test]
    fn only_bt2020_under_pq_counts_as_hdr() {
        assert!(pq().is_hdr10());
        let wide_sdr = OutputColor {
            tf: TransferFunction::Gamma22 as u32,
            ..pq()
        };
        assert!(!wide_sdr.is_hdr10(), "BT.2020 gamma 2.2 is wide SDR");
        let hlg = OutputColor {
            tf: TransferFunction::Hlg as u32,
            ..pq()
        };
        assert!(!hlg.is_hdr10(), "the stream carries PQ only");
        assert!(!OutputColor::default().is_hdr10());
    }

    #[test]
    fn a_description_without_mastering_data_yields_the_generic_hdr10_block() {
        let m = pq().hdr_meta();
        // The same block `PortalCapturer::hdr_meta` claims: BT.2020, D65, 1000 / 0.005 nits.
        assert_eq!(
            m.display_primaries,
            [[8500, 39850], [6550, 2300], [35400, 14600]]
        );
        assert_eq!(m.white_point, [15635, 16450]);
        assert_eq!(m.max_display_mastering_luminance, 10_000_000);
        assert_eq!(m.min_display_mastering_luminance, 50);
        assert_eq!((m.max_cll, m.max_fall), (0, 0));
    }

    #[test]
    fn mastering_data_converts_to_st2086_units() {
        let c = OutputColor {
            // A DCI-P3 panel: x/y × 1e6 → 1/50000 is ÷20.
            target_primaries: Some([
                (680_000, 320_000),
                (265_000, 690_000),
                (150_000, 60_000),
                (312_700, 329_000),
            ]),
            // 0.0001 cd/m² min, 800 cd/m² max.
            target_luminance: Some((1, 800)),
            max_cll: Some(700),
            max_fall: Some(70_000),
            ..pq()
        };
        let m = c.hdr_meta();
        assert_eq!(
            m.display_primaries,
            [[13250, 34500], [7500, 3000], [34000, 16000]]
        );
        assert_eq!(m.max_display_mastering_luminance, 8_000_000);
        assert_eq!(m.min_display_mastering_luminance, 1);
        assert_eq!(m.max_cll, 700);
        assert_eq!(
            m.max_fall,
            u16::MAX,
            "an out-of-range FALL saturates, never wraps"
        );
    }

    #[test]
    fn hdr_asks_the_packed_ten_bit_pair_with_xb30_first() {
        assert_eq!(wanted_fourccs(true), [XB30, XR30]);
        assert_eq!(wanted_fourccs(false), [XR24, AR24]);
        assert_eq!(fourcc_to_pixel(XB30), Some(PixelFormat::X2Bgr10));
        assert_eq!(fourcc_to_pixel(XR30), Some(PixelFormat::X2Rgb10));
    }

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
