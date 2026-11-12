//! What the host opens a source with: the request structs, the `open_*` entry points, and
//! what each platform's sources can deliver before one is open.

#[cfg(target_os = "linux")]
use crate::knobs::Knobs;
#[cfg(any(target_os = "linux", target_os = "windows"))]
use crate::Capturer;
#[cfg(target_os = "linux")]
use crate::{hdr_capture_failed, HdrSource};
#[cfg(target_os = "windows")]
use crate::{CursorChannelSender, CursorForwardSender, EncodeCtlSender, SetEncodeSender};
#[cfg(target_os = "linux")]
use anyhow::Result;

/// Least zero-copy pool depth asked of a compositor. 2 is what every producer already
/// serves; the negotiated depth minus the hold reserve is the deferred-requeue budget.
pub const POOL_MIN: i32 = 2;
/// KWin ≥ 6.2 offers `Range(3, 2, 4)` as a driver stream, so its default 3 wins any
/// intersection that contains it, and a pool of 3 spares one hold while the encoder keeps
/// two frames in flight. A minimum of 4 is the only ask that moves it.
pub const KWIN_POOL_MIN: i32 = 4;
/// The deepest pool KWin serves. A minimum above it fails negotiation outright
/// (`error alloc buffers: Invalid argument`).
pub const KWIN_POOL_MAX: i32 = 4;

/// Encode-backend facts the Linux zero-copy negotiation needs, resolved once
/// by the host facade and passed in so capture never calls back into encode.
#[cfg(target_os = "linux")]
#[derive(Clone, Default, PartialEq)]
pub struct ZeroCopyPolicy {
    /// VAAPI (AMD/Intel): hand raw dmabufs through instead of the EGL→CUDA
    /// import (`encode::linux_zero_copy_is_vaapi`).
    pub backend_is_vaapi: bool,
    /// GPU-resident frames (everything but software). Phrases the CPU-fallback
    /// warning (`encode::resolved_backend_is_gpu`).
    pub backend_is_gpu: bool,
    /// This session encodes PyroWave: the wavelet encoder's Vulkan device
    /// imports raw dmabufs on any vendor, so take raw-dmabuf passthrough.
    /// Per-session, unlike `backend_is_vaapi`.
    pub pyrowave_session: bool,
    /// Encoder can ingest producer-native NV12 (`pf_encode::linux_native_nv12_ok`:
    /// the AMD/Intel passthrough or NVENC's raw lane). Every other arm takes
    /// packed RGB; PyroWave must never see NV12.
    pub native_nv12_session: bool,
    /// Encoder can ingest packed 10-bit PQ CUDA (`pf_encode::linux_hdr_cuda_ok`,
    /// direct-SDK NVENC only). No other arm reads those 2:10:10:10 words as
    /// anything but garbage, so do not produce them unless this holds.
    pub hdr_cuda_ok: bool,
    /// The NVENC encoder takes held dmabufs and lets its zero-copy worker convert them
    /// straight into its input slots (`pf_encode::linux_nvenc_raw_dmabuf_ok`). The capture
    /// then imports nothing; a producer that cannot be held keeps the import path.
    pub nvenc_raw_dmabuf: bool,
    /// The gamescope producer fixates a tiled modifier (`pf_vdisplay::gamescope_tiled_capture`).
    /// Off, its offer stays LINEAR-only.
    pub gamescope_tiled: bool,
    /// Per-fourcc modifiers the session's direct encoder import proved. Empty keeps LINEAR.
    pub encoder_modifiers: Vec<(u32, Vec<u64>)>,
}

#[cfg(target_os = "linux")]
pub fn capturer_supports_444(_encoder_ingests_rgb_444: bool) -> bool {
    true
}

/// Whether a native-plane capturer (compositor virtual output) can deliver HDR
/// (10-bit PQ/BT.2020) on this platform alone — the platform half of the
/// handshake's 10-bit gate, without knowing which compositor will be driven.
///
/// Linux is `false`: Mutter `RecordVirtual` and KWin/wlroots advertise 8-bit
/// BGRx/BGRA. gamescope can be 10-bit with the carried `pipewire-hdr` patch;
/// the host resolves that in `capture::capturer_supports_hdr_for`. The GNOME
/// portal monitor mirror (`open_portal_monitor` + `want_hdr`) is a separate gate.
#[cfg(target_os = "linux")]
pub fn capturer_supports_hdr() -> bool {
    false
}
/// Windows: IDD-push enables advanced colour and delivers P010/Rgb10a2.
#[cfg(target_os = "windows")]
pub fn capturer_supports_hdr() -> bool {
    true
}
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capturer_supports_hdr() -> bool {
    false
}

/// Whether a 10-bit SDR session's IDD-push capture carries the desktop's own bits past 8:
/// Windows composes the virtual display in SDR wide colour (FP16) for it. Windows 11 24H2,
/// until a session on this host finds wide colour refused.
#[cfg(target_os = "windows")]
pub fn capturer_delivers_sdr10() -> bool {
    crate::windows::idd_push::wcg_available()
}
#[cfg(not(target_os = "windows"))]
pub fn capturer_delivers_sdr10() -> bool {
    false
}

#[cfg(target_os = "windows")]
pub fn capturer_supports_444(encoder_ingests_rgb_444: bool) -> bool {
    // IDD-push is full-chroma RGB (BGRA SDR, Rgb10a2 HDR). Only a backend that
    // CSCs RGB to 4:4:4 itself can use that (direct-NVENC). Both depths are
    // full-chroma, so Welcome chroma is real regardless of HDR.
    encoder_ingests_rgb_444
}
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn capturer_supports_444(_encoder_ingests_rgb_444: bool) -> bool {
    false
}

/// Linux xdg-ScreenCast portal capturer for a client-sized monitor. `anchored`
/// inherits a RemoteDesktop grant headlessly. Pass `want_hdr` only when the
/// mirrored monitor is in HDR mode, or the 10 s negotiation latches SDR.
/// Pass `want_metadata_cursor` only when encode composites `CapturedFrame::cursor`;
/// otherwise the portal embeds the pointer so it is never silently lost. `full_range` is the
/// range the YUV converts write.
#[cfg(target_os = "linux")]
pub fn open_portal_monitor(
    anchored: bool,
    want_hdr: bool,
    want_metadata_cursor: bool,
    full_range: bool,
    policy: ZeroCopyPolicy,
) -> Result<Box<dyn Capturer>> {
    crate::linux::PortalCapturer::open(
        anchored,
        want_hdr && !hdr_capture_failed(HdrSource::PortalMonitor),
        want_metadata_cursor,
        full_range,
        policy,
        Knobs::from_env(),
    )
    .map(|c| Box::new(c) as Box<dyn Capturer>)
}

/// The compositor behind a virtual output's PipeWire node. Node ids and remote fds do not
/// name it, so the host does.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Producer {
    /// KWin: rewrites `SPA_META_Cursor` every buffer (`id == 0` hides the pointer), serves
    /// pools of [`KWIN_POOL_MIN`] to [`KWIN_POOL_MAX`], and records unpaced unless
    /// `PUNKTFUNK_KWIN_PACED` asks for the stream rate.
    Kwin,
    /// gamescope: no cursor metadata, a gated tiled modifier offer, and the only HDR producer.
    Gamescope,
    /// Mutter and every other producer.
    #[default]
    Other,
}

/// A compositor output [`open_virtual_output`] captures with `ext-image-copy-capture-v1`
/// before its PipeWire node.
#[cfg(target_os = "linux")]
pub struct DirectOutput {
    /// The output's `wl_output.name`.
    pub output_name: String,
    /// This path's zero-copy facts, resolved only when its open runs: the encoder's modifier
    /// probe is not free. Not the PipeWire path's policy, whose offer narrows by producer.
    pub policy: Box<dyn FnOnce() -> ZeroCopyPolicy>,
}

/// What [`open_virtual_output`] negotiates. Named fields: adjacent bools transpose silently
/// and negotiate the wrong pod family (black screen).
#[cfg(target_os = "linux")]
#[derive(Default)]
pub struct VirtualOutputOpts {
    /// `false` forces CPU mmap even when `PUNKTFUNK_ZEROCOPY` is set.
    pub allow_zerocopy: bool,
    /// Tiled dmabufs convert to planar YUV444.
    pub want_444: bool,
    /// Offer 10-bit PQ/BT.2020. Holds on a [`Producer::Gamescope`] node only: every other
    /// virtual output is SDR, and a desktop that refuses the offer would latch gamescope's SDR.
    pub want_hdr: bool,
    /// 10-bit SDR: keep packed RGB so direct NVENC widens 8→10.
    pub ten_bit_sdr: bool,
    /// The producer composites 10-bit SDR itself (gamescope from `+pfhdr26`): offer its P010
    /// and packed 10-bit formats under BT.709 ahead of the 8-bit ones. Implies `ten_bit_sdr`.
    pub sdr10_native: bool,
    /// Every YUV this capture asks a producer for or converts itself, PQ included, is studio
    /// range: the session's `!OutputFormat::full_range`. Off (the default) is full.
    pub studio_range: bool,
    /// Skip buffers until the negotiated size matches the preferred mode (KWin's
    /// sacrificial birth mode).
    pub expect_exact_dims: bool,
    pub producer: Producer,
    /// The PipeWire path's zero-copy facts. [`open_virtual_output`] fills it from its `policy`
    /// only when the node opens, so a caller leaves it default.
    pub policy: ZeroCopyPolicy,
    /// The wire rate; the output may run at a multiple of it. `0` derives it from the output's
    /// refresh and the configured multiple.
    pub stream_hz: u32,
    /// Read the pointer from gamescope's nested Xwaylands: its node carries no
    /// `SPA_META_Cursor`. `None` everywhere else.
    pub gamescope_cursor: Option<crate::GamescopeCursorTargets>,
    /// Sessions may share this node's screen: an open takes its frames from the stream
    /// already on the node, since a reliable producer feeds one PipeWire link only.
    pub share: bool,
    /// Capture this output directly first. Skipped without zero-copy (it delivers dmabufs
    /// only) or under `PUNKTFUNK_DIRECT_CAPTURE=0`.
    pub direct: Option<DirectOutput>,
}

/// A virtual output with no PipeWire node (`node_id` 0, no remote) that direct capture could
/// not open. Retrying cannot help: the backend has nothing else to offer.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct NoPipeWireNode;

#[cfg(target_os = "linux")]
impl std::fmt::Display for NoPipeWireNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("direct capture refused and the backend has no PipeWire node")
    }
}

/// The node's one stream is held in a format this open's encoder does not take, and its own
/// stream got no frame: a reliable producer feeds one link. Retrying cannot help.
#[cfg(target_os = "linux")]
#[derive(Debug)]
pub struct SharedCaptureTaken;

#[cfg(target_os = "linux")]
impl std::fmt::Display for SharedCaptureTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("shared capture is taken in another format")
    }
}

/// Linux capturer for an existing virtual output, which `keepalive` owns. Direct capture goes
/// first where it may run: the portal's re-request timer halves the rate above ~140 Hz. It
/// fails for every reason to use the PipeWire node instead (no protocol, no output, nothing the
/// encoder imports) and hands the keepalive back for it. `want_hdr` there needs the output lit
/// in BT.2020 PQ; on the node, a [`Producer::Gamescope`]. `policy` is the node's zero-copy
/// facts, resolved only when it opens: the encoder's modifier probe is not free. An output
/// with no node fails here as [`NoPipeWireNode`].
#[cfg(target_os = "linux")]
pub fn open_virtual_output(
    remote_fd: Option<std::os::fd::OwnedFd>,
    node_id: u32,
    preferred_mode: Option<(u32, u32, u32)>,
    mut keepalive: Box<dyn Send>,
    mut opts: VirtualOutputOpts,
    policy: impl FnOnce() -> ZeroCopyPolicy,
) -> Result<Box<dyn Capturer>> {
    let knobs = Knobs::from_env();
    let try_direct = opts.allow_zerocopy && knobs.direct_capture;
    let mut refused =
        anyhow::anyhow!("direct capture off (software encoder or PUNKTFUNK_DIRECT_CAPTURE=0)");
    if let Some(direct) = opts.direct.take().filter(|_| try_direct) {
        let name = direct.output_name;
        let policy = (direct.policy)();
        match crate::linux::WlCapturer::open(
            name.clone(),
            keepalive,
            policy,
            opts.want_hdr,
            knobs.clone(),
        ) {
            Ok(c) => {
                tracing::info!(output = %name, "capturing the compositor output directly");
                return Ok(Box::new(c));
            }
            Err((e, handed_back)) => {
                keepalive = handed_back;
                tracing::info!(
                    output = %name,
                    reason = %format!("{e:#}"),
                    "no direct capture on this compositor — using the ScreenCast portal"
                );
                refused = e;
            }
        }
    }
    if node_id == 0 && remote_fd.is_none() {
        return Err(refused.context(NoPipeWireNode));
    }
    let want_hdr = opts.want_hdr
        && opts.producer == Producer::Gamescope
        && !hdr_capture_failed(HdrSource::VirtualOutput);
    crate::linux::PortalCapturer::from_virtual_output(
        remote_fd,
        node_id,
        preferred_mode,
        keepalive,
        VirtualOutputOpts {
            want_hdr,
            policy: policy(),
            ..opts
        },
        knobs,
    )
    .map(|c| Box::new(c) as Box<dyn Capturer>)
}

/// Whether the compositor output named `output_name` is lit in HDR (BT.2020 PQ), read
/// from its `wp_color_management_v1` description. `None` when the compositor has no colour
/// management or no such output — direct capture then has no HDR to offer.
#[cfg(target_os = "linux")]
pub fn output_hdr(output_name: &str) -> Option<bool> {
    crate::linux::output_is_hdr10(output_name)
}

/// Whether the compositor advertises every global a [`DirectOutput`] binds for an HDR
/// capture: the capture protocol, dmabuf, and `wp_color_management_v1`. Answerable before
/// the output exists, so the handshake can ask.
#[cfg(target_os = "linux")]
pub fn direct_hdr_globals() -> bool {
    crate::linux::hdr_capture_globals()
}

/// What an IDD-push capturer opens for. Named fields: four adjacent bools transpose silently,
/// and a transposed pair composes the wrong depth or runs the wrong cursor model.
#[cfg(target_os = "windows")]
pub struct IddPushOpts {
    /// The handshake advertised HDR, not merely 10-bit. Off pins advanced colour off, so an
    /// SDR client never gets in-band PQ.
    pub want_hdr: bool,
    /// 10-bit SDR: composed in SDR wide colour where Windows allows it, else the driver's
    /// encoder widens BGRA itself. Never set with `want_hdr`.
    pub ten_bit_sdr: bool,
    /// Full-chroma 4:4:4.
    pub want_444: bool,
    /// A wavelet session.
    pub pyrowave: bool,
    /// `Some` opts the session into the cursor channel: the capturer creates the cursor
    /// section and the driver declares the hardware cursor. `None` keeps DWM's pointer.
    pub cursor_sender: Option<CursorChannelSender>,
    pub cursor_forward: Option<CursorForwardSender>,
    /// The session negotiated a pointer the client draws (`HOST_CAP_CURSOR`). Off, the
    /// pointer reaches the wire only through the driver pool's blend.
    pub forwards_to_client: bool,
    /// `IOCTL_SET_ENCODE` and `IOCTL_ENCODE_CTL`, which the source's
    /// [`RemoteEncoder`](crate::RemoteEncoder) opens and drives the driver's encoder through.
    pub set_encode: SetEncodeSender,
    pub encode_ctl: EncodeCtlSender,
}

/// Windows IDD direct-push capturer on a pf-vdisplay target. On failure `keepalive` is handed
/// back so the caller can retire the display.
#[cfg(target_os = "windows")]
pub fn open_idd_push(
    target: pf_frame::dxgi::WinCaptureTarget,
    preferred: Option<(u32, u32, u32)>,
    keepalive: Box<dyn Send>,
    opts: IddPushOpts,
) -> std::result::Result<Box<dyn Capturer>, (anyhow::Error, Box<dyn Send>)> {
    crate::windows::idd_push::IddPushCapturer::open(target, preferred, keepalive, opts)
        .map(|c| Box::new(c) as Box<dyn Capturer>)
}
