//! V4L2 decode: the hardware rung on SoCs whose decoder is a memory-to-memory
//! video node. No ARM Mesa driver has Vulkan Video or VA-API, so this is
//! their only rung.
//!
//! Two kinds of node. A stateful one (Qualcomm `iris`/`venus`, MediaTek,
//! Amlogic) parses the stream itself: [`StatefulRung`] here feeds it access
//! units. A stateless one (Raspberry Pi 5, Rockchip) takes parsed slices:
//! `video_v4l2_hevc`. Either way the pump's facts — keyframe, colour, clean
//! references, damage — come from the shared `pf-bitstream` planners, and the
//! queue flows and ioctls live in `pf-v4l2dec` and `pf-v4l2`.
//!
//! The driver's enumeration is the only truth: [`caps`] lists a codec only
//! where a node takes it and offers a picture format this client can show.
//! Pin with `PUNKTFUNK_DECODER=native-v4l2`; `PUNKTFUNK_V4L2_DEVICE` names
//! the node.

use std::collections::VecDeque;
use std::os::fd::AsRawFd as _;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use pf_v4l2::Node;
use pf_v4l2::RequestNode;
use pf_v4l2dec::stateful::fourcc_name;
use pf_v4l2dec::stateful::Device;
use pf_v4l2dec::stateful::Picture;
use pf_v4l2dec::stateful::Queue;
use pf_v4l2dec::stateful::Stateful;
use pf_v4l2dec::uapi;
use pf_v4l2dec::uapi_stateless;

use crate::video::DecodeHealth;
use crate::video::DecodedImage;
use crate::video::DmabufFrame;
use crate::video::DmabufPlane;
use crate::video::DrmFrameGuard;
use crate::video::FrameGuard;
use crate::video::StreamFormat;
use crate::video::V4l2Summary;
use crate::video_color::ColorDesc;

/// `PUNKTFUNK_DECODER=native-v4l2`.
pub(crate) const DECODER_PIN: &str = "native-v4l2";

/// `DRM_FORMAT_MOD_LINEAR`. V4L2 buffers carry no modifier; the linear
/// formats this rung accepts are, by definition, this one.
pub(crate) const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Wait for a picture when none is in hand. A working decoder answers in a
/// few milliseconds; past this the pump moves on and takes it next call.
const WAIT_PICTURE: Duration = Duration::from_millis(30);

/// Extra wait for this access unit's own picture when an older one is in
/// hand. Short: it only exists to catch up after one late picture.
const WAIT_CATCH_UP: Duration = Duration::from_millis(5);

/// Access units outstanding with no picture before the decoder counts as
/// stalled. About a quarter second at 60 Hz.
const STALLED_AFTER: usize = 16;

/// What a node decodes, from its own format enumeration.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CodecNode {
    path: PathBuf,
    /// A linear 10-bit picture format is offered for this codec.
    ten_bit: bool,
}

/// V4L2 decode on this machine: per wire codec, the node that takes it.
#[derive(Debug, Default)]
pub(crate) struct Caps {
    h264: Option<CodecNode>,
    hevc: Option<CodecNode>,
    av1: Option<CodecNode>,
    /// A node that decodes HEVC from parsed slices, frame by frame.
    hevc_stateless: Option<PathBuf>,
}

impl Caps {
    fn slot(&self, wire: u8) -> Option<&CodecNode> {
        match wire {
            punktfunk_core::quic::CODEC_H264 => self.h264.as_ref(),
            punktfunk_core::quic::CODEC_HEVC => self.hevc.as_ref(),
            punktfunk_core::quic::CODEC_AV1 => self.av1.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn summary(&self) -> V4l2Summary {
        let mut s = V4l2Summary::default();
        for wire in [
            punktfunk_core::quic::CODEC_H264,
            punktfunk_core::quic::CODEC_HEVC,
            punktfunk_core::quic::CODEC_AV1,
        ] {
            if let Some(node) = self.slot(wire) {
                s.codecs |= wire;
                if node.ten_bit {
                    s.ten_bit |= wire;
                }
            }
        }
        // 8-bit only: no stateless decoder here has a 10-bit format we show.
        if self.hevc_stateless.is_some() {
            s.codecs |= punktfunk_core::quic::CODEC_HEVC;
        }
        s
    }
}

/// The machine's V4L2 decoders, probed once. Opens each video node briefly.
pub(crate) fn caps() -> &'static Caps {
    static CAPS: OnceLock<Caps> = OnceLock::new();
    CAPS.get_or_init(probe)
}

fn wire_fourcc(wire: u8) -> Option<u32> {
    match wire {
        punktfunk_core::quic::CODEC_H264 => Some(uapi::V4L2_PIX_FMT_H264),
        punktfunk_core::quic::CODEC_HEVC => Some(uapi::V4L2_PIX_FMT_HEVC),
        punktfunk_core::quic::CODEC_AV1 => Some(uapi::V4L2_PIX_FMT_AV1),
        _ => None,
    }
}

fn probe() -> Caps {
    let mut caps = Caps::default();
    // The struct layouts in `pf_v4l2dec::uapi` are the 64-bit little-endian ABI.
    if !cfg!(all(target_pointer_width = "64", target_endian = "little")) {
        return caps;
    }
    for path in candidate_nodes() {
        let Ok(mut node) = Node::open(&path) else {
            continue;
        };
        let Ok(inputs) = node.formats(Queue::Output) else {
            continue;
        };
        for (wire, slot) in [
            (punktfunk_core::quic::CODEC_H264, &mut caps.h264),
            (punktfunk_core::quic::CODEC_HEVC, &mut caps.hevc),
            (punktfunk_core::quic::CODEC_AV1, &mut caps.av1),
        ] {
            let fourcc = wire_fourcc(wire).expect("the three listed codecs map");
            if slot.is_some() || !inputs.contains(&fourcc) {
                continue;
            }
            // Picture formats depend on the codec, so ask with it selected.
            if node.set_output_format(fourcc, 1920, 1080, 2 << 20).is_err() {
                continue;
            }
            let Ok(pictures) = node.formats(Queue::Capture) else {
                continue;
            };
            if !pictures.contains(&uapi::V4L2_PIX_FMT_NV12) {
                tracing::info!(
                    node = %path.display(),
                    codec = crate::video::wire_codec_name(wire),
                    offered = ?pictures.iter().map(|f| fourcc_name(*f)).collect::<Vec<_>>(),
                    "V4L2 decoder offers no linear NV12 for this codec — not used"
                );
                continue;
            }
            *slot = Some(CodecNode {
                path: path.clone(),
                ten_bit: pictures.contains(&uapi::V4L2_PIX_FMT_P010),
            });
        }
        if caps.hevc_stateless.is_none()
            && inputs.contains(&uapi_stateless::V4L2_PIX_FMT_HEVC_SLICE)
            && frame_based(&path)
        {
            caps.hevc_stateless = Some(path.clone());
        }
    }
    let s = caps.summary();
    if s.codecs != 0 {
        tracing::info!(
            h264 = ?caps.h264.as_ref().map(|n| n.path.display().to_string()),
            hevc = ?caps.hevc.as_ref().map(|n| n.path.display().to_string()),
            av1 = ?caps.av1.as_ref().map(|n| n.path.display().to_string()),
            hevc_stateless = ?caps.hevc_stateless.as_ref().map(|n| n.display().to_string()),
            ten_bit = format_args!("{:#04x}", s.ten_bit),
            "V4L2 decoders found"
        );
    }
    caps
}

/// Can the stateless node at `path` take a whole picture per request? It
/// also has to open: a node no media device claims has no requests.
fn frame_based(path: &Path) -> bool {
    use pf_v4l2dec::stateless::Device as _;
    let Ok(mut node) = RequestNode::open(path) else {
        return false;
    };
    match node.control(uapi_stateless::V4L2_CID_STATELESS_HEVC_DECODE_MODE) {
        Ok(Some(range)) => {
            range.maximum >= uapi_stateless::V4L2_STATELESS_HEVC_DECODE_MODE_FRAME_BASED
        }
        // No mode control: the driver has one mode, and it is per picture.
        Ok(None) => true,
        Err(_) => false,
    }
}

/// `PUNKTFUNK_V4L2_DEVICE`, else every `/dev/video*` in numeric order.
fn candidate_nodes() -> Vec<PathBuf> {
    if let Some(forced) = std::env::var_os("PUNKTFUNK_V4L2_DEVICE").filter(|v| !v.is_empty()) {
        return vec![PathBuf::from(forced)];
    }
    let Ok(dir) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut nodes: Vec<(u32, PathBuf)> = dir
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let n = name.to_str()?.strip_prefix("video")?.parse().ok()?;
            Some((n, e.path()))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().map(|(_, p)| p).collect()
}

/// What the stateful rung needs of a decoder beyond the queue flow.
pub(crate) trait Opened: Device + Sized {
    fn open(path: &Path) -> std::io::Result<Self>;
    /// Export CAPTURE buffer `index` as a dma-buf.
    fn export(&mut self, index: u32) -> std::io::Result<OwnedFd>;
}

impl Opened for Node {
    fn open(path: &Path) -> std::io::Result<Node> {
        Node::open(path)
    }

    fn export(&mut self, index: u32) -> std::io::Result<OwnedFd> {
        Node::export(self, index)
    }
}

/// Which buffer a shipped frame gives back, and to which pool.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Release {
    pub(crate) pool: u32,
    pub(crate) generation: u64,
    pub(crate) index: u32,
}

/// Holds one shipped picture's buffer until the presenter is done reading it.
/// The fds stay open past a pool rebuild so an imported frame never dangles.
pub struct V4l2FrameGuard {
    pub(crate) _fds: Arc<Vec<OwnedFd>>,
    pub(crate) tx: mpsc::Sender<Release>,
    pub(crate) release: Release,
}

impl Drop for V4l2FrameGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(self.release);
    }
}

/// Facts of the access unit a picture decodes, recorded when it was queued.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Facts {
    pub(crate) keyframe: bool,
    pub(crate) references_clean: bool,
    pub(crate) color: ColorDesc,
    pub(crate) display: (u32, u32),
    /// The planner saw damage: decode it to keep the references in step, but
    /// do not show it.
    pub(crate) damaged: bool,
}

/// What decides the input buffer size and the picture format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    coded: (u32, u32),
    bit_depth: u8,
}

enum Planner {
    H264(Box<pf_bitstream::h264::H264Planner>),
    H265(Box<pf_bitstream::h265::H265Planner>),
    Av1(Box<pf_bitstream::av1::Av1Planner>),
}

/// The planner's answer for one access unit.
enum Planned {
    /// Queue it; `Some` when a picture is expected back.
    Feed(Shape, Option<Facts>),
    /// Nothing decodable yet: wait for the keyframe.
    AwaitKeyframe,
    /// Dropped by rule (an HEVC RASL picture after a random access).
    Skip,
}

struct Session<D: Opened> {
    decoder: Stateful<D>,
    shape: Shape,
    /// This session's pools are `(pool, generation)`; `pool` is the session.
    pool: u32,
    /// dma-bufs of the current CAPTURE generation, by buffer index.
    exports: Arc<Vec<OwnedFd>>,
    exported_generation: u64,
    /// Queued access units still owed a picture, oldest first.
    pending: VecDeque<(u64, Facts)>,
}

/// The V4L2 rung: whichever kind of node takes the session's codec.
pub(crate) enum NativeV4l2Decoder {
    Stateful(Box<StatefulRung>),
    StatelessHevc(Box<crate::video_v4l2_hevc::StatelessHevc>),
}

impl NativeV4l2Decoder {
    /// Refuses when no node takes this codec and shape, so the ladder falls
    /// through at construction instead of on the first access unit.
    pub(crate) fn new(wire: u8, stream: StreamFormat) -> Result<NativeV4l2Decoder> {
        if stream.chroma_format_idc != punktfunk_core::quic::CHROMA_IDC_420 {
            bail!("V4L2 decode is 4:2:0 only");
        }
        let caps = caps();
        if let Some(node) = caps.slot(wire) {
            if stream.bit_depth > 8 && !node.ten_bit {
                bail!("the V4L2 decoder offers no linear 10-bit picture format");
            }
            return Ok(NativeV4l2Decoder::Stateful(Box::new(
                StatefulRung::on_node(wire, node.path.clone()),
            )));
        }
        match &caps.hevc_stateless {
            Some(path) if wire == punktfunk_core::quic::CODEC_HEVC => {
                if stream.bit_depth > 8 {
                    bail!("the stateless V4L2 decoder has no 10-bit picture format we show");
                }
                Ok(NativeV4l2Decoder::StatelessHevc(Box::new(
                    crate::video_v4l2_hevc::StatelessHevc::on_node(path.clone()),
                )))
            }
            _ => bail!("no V4L2 decoder node takes this codec"),
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            NativeV4l2Decoder::Stateful(d) => d.name(),
            NativeV4l2Decoder::StatelessHevc(_) => "native-v4l2 h265 (stateless)",
        }
    }

    pub(crate) fn health(&self) -> DecodeHealth {
        match self {
            NativeV4l2Decoder::Stateful(d) => d.health(),
            NativeV4l2Decoder::StatelessHevc(d) => d.health(),
        }
    }

    pub(crate) fn take_recovery_request(&mut self) -> bool {
        match self {
            NativeV4l2Decoder::Stateful(d) => d.take_recovery_request(),
            NativeV4l2Decoder::StatelessHevc(d) => d.take_recovery_request(),
        }
    }

    pub(crate) fn forgive_unclean(&mut self) {
        match self {
            NativeV4l2Decoder::Stateful(d) => d.forgive_unclean(),
            NativeV4l2Decoder::StatelessHevc(d) => d.forgive_unclean(),
        }
    }

    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<DecodedImage>> {
        match self {
            NativeV4l2Decoder::Stateful(d) => Ok(d.decode(au)?.map(DecodedImage::NativeDmabuf)),
            NativeV4l2Decoder::StatelessHevc(d) => d.decode(au),
        }
    }
}

/// A stateful decoder node: access units in, pictures out, its own parser.
pub(crate) struct StatefulRung<D: Opened = Node> {
    wire: u8,
    node: PathBuf,
    planner: Planner,
    session: Option<Session<D>>,
    next_stamp: u64,
    health: DecodeHealth,
    recovery_request: bool,
    /// An access unit the planner counted never reached the driver, so the
    /// driver's references are wrong until the next keyframe.
    out_of_step: bool,
    release_tx: mpsc::Sender<Release>,
    release_rx: mpsc::Receiver<Release>,
}

pub(crate) fn colour_of(c: &pf_bitstream::h264::ColourDescription) -> ColorDesc {
    ColorDesc {
        primaries: c.colour_primaries,
        transfer: c.transfer_characteristics,
        matrix: c.matrix_coefficients,
        full_range: c.video_full_range,
    }
}

/// The visible size of a cropped picture. Planes are sampled from (0,0), so
/// a crop with another origin is refused rather than shown shifted.
pub(crate) fn display_of(crop: pf_bitstream::h264::DisplayCrop) -> Result<(u32, u32)> {
    if crop.x != 0 || crop.y != 0 {
        bail!(
            "conformance window at ({}, {}) — this rung hands the buffer over uncropped",
            crop.x,
            crop.y
        );
    }
    Ok((crop.width, crop.height))
}

impl<D: Opened> StatefulRung<D> {
    /// The rung over the decoder at `node`, opened on the first access unit.
    fn on_node(wire: u8, node: PathBuf) -> StatefulRung<D> {
        let planner = match wire {
            punktfunk_core::quic::CODEC_H264 => {
                Planner::H264(Box::new(pf_bitstream::h264::H264Planner::new()))
            }
            punktfunk_core::quic::CODEC_HEVC => {
                Planner::H265(Box::new(pf_bitstream::h265::H265Planner::new()))
            }
            _ => Planner::Av1(Box::new(pf_bitstream::av1::Av1Planner::new())),
        };
        let (release_tx, release_rx) = mpsc::channel();
        StatefulRung {
            wire,
            node,
            planner,
            session: None,
            next_stamp: 0,
            health: DecodeHealth {
                // The driver has no per-picture status query this rung reads.
                status_queries: false,
                ..DecodeHealth::default()
            },
            recovery_request: false,
            out_of_step: false,
            release_tx,
            release_rx,
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        match self.planner {
            Planner::H264(_) => "native-v4l2 h264",
            Planner::H265(_) => "native-v4l2 h265",
            Planner::Av1(_) => "native-v4l2 av1",
        }
    }

    pub(crate) fn health(&self) -> DecodeHealth {
        self.health
    }

    pub(crate) fn take_recovery_request(&mut self) -> bool {
        std::mem::take(&mut self.recovery_request)
    }

    pub(crate) fn forgive_unclean(&mut self) {
        match &mut self.planner {
            Planner::H264(p) => p.forgive_unclean(),
            Planner::H265(p) => p.forgive_unclean(),
            Planner::Av1(p) => p.forgive_unclean(),
        }
    }

    /// One access unit in, at most one picture out. `Ok(None)` is the decoder
    /// still working, a withheld damaged picture, or the wait for a keyframe.
    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<DmabufFrame>> {
        self.drain_releases();
        let result = self.decode_inner(au);
        match &result {
            Ok(Some((_, damaged))) => self.health.note(*damaged, false, 0),
            Ok(None) => {}
            Err(_) => self.health.note(false, true, 0),
        }
        Ok(result?.and_then(|(frame, _)| frame))
    }

    fn decode_inner(&mut self, au: &[u8]) -> Result<Option<(Option<DmabufFrame>, bool)>> {
        let (shape, mut facts) = match self.plan(au)? {
            Planned::Feed(shape, facts) => (shape, facts),
            Planned::AwaitKeyframe => {
                self.recovery_request = true;
                return Ok(None);
            }
            Planned::Skip => return Ok(None),
        };
        let keyframe = facts.is_some_and(|f| f.keyframe);
        if let Some(f) = facts.as_mut() {
            f.damaged |= self.out_of_step && !keyframe;
        }
        let damaged = facts.is_some_and(|f| f.damaged);
        if damaged {
            self.recovery_request = true;
        }
        let stamp = self.next_stamp;
        self.next_stamp += 1;
        let queued = self
            .ensure_session(shape)
            .and_then(|s| s.decoder.submit(au, stamp).map_err(|e| anyhow!("{e}")));
        if let Err(e) = queued {
            // The planner has moved past this unit; the driver never saw it.
            self.out_of_step = true;
            return Err(e);
        }
        if keyframe {
            self.out_of_step = false;
        }
        let s = self.session.as_mut().expect("just queued into it");
        if let Some(facts) = facts {
            s.pending.push_back((stamp, facts));
        }
        if s.pending.len() > STALLED_AFTER {
            let owed = s.pending.len();
            // Start over at the next keyframe instead of failing every unit.
            s.pending.clear();
            bail!("the V4L2 decoder returned no picture for {owed} access units");
        }
        let Some(picture) = self.newest_picture(stamp)? else {
            return Ok(Some((None, damaged)));
        };
        let frame = self.ship(picture)?;
        Ok(Some((frame, damaged)))
    }

    /// Picture facts for `au` from the shared planner. The driver decodes on
    /// its own parse; this only tells the pump what the picture is.
    fn plan(&mut self, au: &[u8]) -> Result<Planned> {
        match &mut self.planner {
            Planner::H264(p) => {
                let plan = match p.plan_au(au) {
                    Ok(plan) => plan,
                    Err(e) if e.awaits_idr() => return Ok(Planned::AwaitKeyframe),
                    Err(e) => return Err(anyhow!("{e:?}")),
                };
                let pic = &plan.picture;
                Ok(Planned::Feed(
                    Shape {
                        coded: (pic.coded_width, pic.coded_height),
                        bit_depth: 8 + pic.bit_depth_luma_minus8,
                    },
                    Some(Facts {
                        keyframe: pic.is_idr,
                        references_clean: pic.references_clean,
                        color: colour_of(&pic.colour),
                        display: display_of(pic.display_crop)?,
                        damaged: plan
                            .warnings
                            .iter()
                            .any(pf_bitstream::h264::PlanWarning::is_integrity),
                    }),
                ))
            }
            Planner::H265(p) => {
                let plan = match p.plan_au(au) {
                    Ok(plan) => plan,
                    Err(pf_bitstream::h265::PlanError::RaslSkipped { .. }) => {
                        return Ok(Planned::Skip)
                    }
                    Err(e) if e.awaits_idr() => return Ok(Planned::AwaitKeyframe),
                    Err(e) => return Err(anyhow!("{e:?}")),
                };
                let pic = &plan.picture;
                Ok(Planned::Feed(
                    Shape {
                        coded: (pic.coded_width, pic.coded_height),
                        bit_depth: 8 + pic.bit_depth_luma_minus8,
                    },
                    Some(Facts {
                        keyframe: pic.is_idr,
                        references_clean: pic.references_clean,
                        color: colour_of(&pic.colour),
                        display: display_of(pic.display_crop)?,
                        damaged: plan
                            .warnings
                            .iter()
                            .any(pf_bitstream::h265::PlanWarning::is_integrity),
                    }),
                ))
            }
            Planner::Av1(p) => {
                let plans = p.plan_au(au).map_err(|e| anyhow!("{e}"))?;
                let Some(first) = plans.first() else {
                    return Ok(Planned::Skip);
                };
                let shape = Shape {
                    coded: (
                        u32::from(first.sequence.max_frame_width_minus_1) + 1,
                        u32::from(first.sequence.max_frame_height_minus_1) + 1,
                    ),
                    bit_depth: first.picture.bit_depth,
                };
                let damaged = plans.iter().any(|plan| {
                    plan.warnings
                        .iter()
                        .any(pf_bitstream::av1::PlanWarning::is_integrity)
                });
                // A temporal unit shows at most one frame; hidden ones are
                // references the driver keeps to itself.
                let shown = plans.iter().rev().find(|plan| !plan.dpb.outputs.is_empty());
                let facts = shown.map(|plan| Facts {
                    keyframe: plan.picture.is_key,
                    references_clean: plan.picture.references_clean,
                    color: colour_of(&plan.picture.colour),
                    display: (
                        plan.picture.render_width.min(plan.picture.upscaled_width),
                        plan.picture.render_height.min(plan.picture.frame_height),
                    ),
                    damaged,
                });
                Ok(Planned::Feed(shape, facts))
            }
        }
    }

    /// A new stream shape gets a new node session: the input buffers are
    /// sized for it and the picture format follows its bit depth.
    fn ensure_session(&mut self, shape: Shape) -> Result<&mut Session<D>> {
        if self.session.as_ref().is_some_and(|s| s.shape == shape) {
            return Ok(self.session.as_mut().expect("just matched"));
        }
        if let Some(old) = self.session.take() {
            tracing::info!(from = ?old.shape, to = ?shape,
                "V4L2 stream renegotiated — reopening the decoder");
        }
        let wanted = if shape.bit_depth > 8 {
            uapi::V4L2_PIX_FMT_P010
        } else {
            uapi::V4L2_PIX_FMT_NV12
        };
        let fourcc = wire_fourcc(self.wire).expect("construction checked the codec");
        let node = D::open(&self.node).with_context(|| format!("open {}", self.node.display()))?;
        let decoder = Stateful::open(node, fourcc, shape.coded.0, shape.coded.1, &[wanted])
            .map_err(|e| anyhow!("{e}"))
            .context("start the V4L2 decoder")?;
        Ok(self.session.insert(Session {
            decoder,
            shape,
            pool: crate::video::next_pool_generation(),
            exports: Arc::new(Vec::new()),
            exported_generation: 0,
            pending: VecDeque::new(),
        }))
    }

    fn drain_releases(&mut self) {
        while let Ok(r) = self.release_rx.try_recv() {
            let Some(s) = self.session.as_mut().filter(|s| s.pool == r.pool) else {
                continue;
            };
            if let Err(e) = s.decoder.release(r.index, r.generation) {
                tracing::debug!(error = %e, "V4L2: a picture buffer did not requeue");
            }
        }
    }

    /// The newest ready picture. Waits for one, then briefly for `stamp`'s
    /// own; an older picture it overtakes goes straight back to the decoder.
    fn newest_picture(&mut self, stamp: u64) -> Result<Option<Picture>> {
        let s = self.session.as_mut().expect("decode built the session");
        let mut deadline = Instant::now() + WAIT_PICTURE;
        let mut best: Option<Picture> = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(p) = s.decoder.pump(left).map_err(|e| anyhow!("{e}"))? else {
                break;
            };
            if let Some(older) = best.replace(p) {
                s.decoder
                    .release(older.index, older.generation)
                    .map_err(|e| anyhow!("{e}"))?;
                if self.health.note_dropped() {
                    tracing::warn!(
                        dropped_total = self.health.dropped,
                        "native V4L2: the decoder is running behind — dropping the older picture"
                    );
                }
            }
            if p.stamp >= stamp {
                break;
            }
            deadline = deadline.min(Instant::now() + WAIT_CATCH_UP);
        }
        Ok(best)
    }

    /// Build the presenter's frame, or give the buffer back when the picture
    /// must not be shown.
    fn ship(&mut self, picture: Picture) -> Result<Option<DmabufFrame>> {
        let s = self.session.as_mut().expect("decode built the session");
        // Drivers copy the input stamp to its picture. Older entries are
        // units the decoder produced nothing for.
        while s.pending.front().is_some_and(|(st, _)| *st < picture.stamp) {
            s.pending.pop_front();
        }
        // No entry: a unit the stall reset forgot, or one that shows nothing.
        let facts = match s.pending.front() {
            Some((st, _)) if *st == picture.stamp => s.pending.pop_front().map(|(_, f)| f),
            _ => None,
        };
        let give_back = |s: &mut Session<D>| {
            s.decoder
                .release(picture.index, picture.generation)
                .map_err(|e| anyhow!("{e}"))
        };
        let Some(facts) = facts else {
            give_back(s)?;
            return Ok(None);
        };
        if facts.damaged || picture.corrupt {
            self.recovery_request = true;
            give_back(s)?;
            return Ok(None);
        }
        let capture = s
            .decoder
            .capture()
            .expect("a picture implies a configured queue");
        let format = capture.format;
        let buffers = capture.buffers;
        if s.exported_generation != picture.generation {
            let mut fds = Vec::with_capacity(buffers as usize);
            for index in 0..buffers {
                fds.push(
                    s.decoder
                        .device()
                        .export(index)
                        .context("export a V4L2 picture buffer")?,
                );
            }
            s.exports = Arc::new(fds);
            s.exported_generation = picture.generation;
            tracing::info!(
                format = %fourcc_name(format.fourcc),
                coded = format_args!("{}x{}", format.width, format.height),
                stride = format.stride,
                buffers,
                "native V4L2 picture pool ready"
            );
        }
        let fd = s.exports[picture.index as usize].as_raw_fd();
        // One memory plane: chroma follows luma at the coded height.
        let chroma_offset = format
            .stride
            .checked_mul(format.height)
            .ok_or_else(|| anyhow!("V4L2 picture geometry overflows"))?;
        Ok(Some(DmabufFrame {
            width: facts.display.0,
            height: facts.display.1,
            coded_width: format.width,
            coded_height: format.height,
            // The V4L2 codes for NV12 and P010 are the DRM ones.
            fourcc: format.fourcc,
            modifier: DRM_FORMAT_MOD_LINEAR,
            planes: vec![
                DmabufPlane {
                    fd,
                    offset: 0,
                    stride: format.stride,
                },
                DmabufPlane {
                    fd,
                    offset: chroma_offset,
                    stride: format.stride,
                },
            ],
            color: facts.color,
            keyframe: facts.keyframe,
            references_clean: facts.references_clean,
            // A dequeued buffer is finished: there is no fence to wait.
            sync_fds: Vec::new(),
            // Pool and generation both name a distinct set of buffers.
            pool_key: (u64::from(s.pool) << 48)
                | ((picture.generation & 0xffff) << 32)
                | u64::from(picture.index),
            path: DECODER_PIN,
            guard: DrmFrameGuard(FrameGuard::V4l2(V4l2FrameGuard {
                _fds: s.exports.clone(),
                tx: self.release_tx.clone(),
                release: Release {
                    pool: s.pool,
                    generation: picture.generation,
                    index: picture.index,
                },
            })),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_bitstream::testing::split_h264_aus;
    use pf_bitstream::testing::H264_25FPS;
    use pf_v4l2dec::testing::FakeDecoder;

    impl Opened for FakeDecoder {
        fn open(_: &Path) -> std::io::Result<FakeDecoder> {
            // Larger than the vendored stream: the rung takes the coded size
            // from the decoder and the visible one from the bitstream.
            Ok(FakeDecoder::new(384, 256, &[uapi::V4L2_PIX_FMT_NV12]))
        }

        fn export(&mut self, _: u32) -> std::io::Result<OwnedFd> {
            Ok(std::fs::File::open("/dev/null")?.into())
        }
    }

    fn rung() -> StatefulRung<FakeDecoder> {
        StatefulRung::on_node(punktfunk_core::quic::CODEC_H264, PathBuf::from("fake"))
    }

    fn fake(d: &mut StatefulRung<FakeDecoder>) -> &mut FakeDecoder {
        d.session.as_mut().expect("a session").decoder.device()
    }

    /// The whole path short of a device: planner facts, queueing, the stamp
    /// pairing, the dma-buf layout, and the guard that returns each buffer.
    #[test]
    fn the_vendored_stream_decodes_one_picture_per_unit() {
        let mut d = rung();
        let aus = split_h264_aus(H264_25FPS);
        assert!(aus.len() > 8, "more units than the pool has buffers");
        let mut keys = Vec::new();
        for (n, au) in aus.iter().enumerate() {
            let f = d
                .decode(au)
                .expect("decode")
                .unwrap_or_else(|| panic!("unit {n} produced no picture"));
            assert_eq!(f.path, "native-v4l2");
            assert_eq!((f.coded_width, f.coded_height), (384, 256));
            assert!(f.width <= f.coded_width && f.height <= f.coded_height);
            assert_eq!(f.fourcc, uapi::V4L2_PIX_FMT_NV12);
            assert_eq!(f.modifier, DRM_FORMAT_MOD_LINEAR);
            assert_eq!(f.planes.len(), 2);
            assert_eq!((f.planes[0].offset, f.planes[0].stride), (0, 384));
            assert_eq!(
                f.planes[1].offset,
                384 * 256,
                "chroma follows the coded luma"
            );
            assert!(f.sync_fds.is_empty());
            assert!(f.keyframe || n > 0, "the stream opens on an IDR");
            keys.push(f.pool_key);
        }
        // Dropping each frame returned its buffer: six buffers carried them all.
        keys.sort_unstable();
        keys.dedup();
        assert!(keys.len() <= 6, "{} distinct buffers", keys.len());
        assert_eq!(fake(&mut d).log, ["allocate"]);
        assert!(!d.take_recovery_request());
    }

    /// A picture the presenter still holds is never decoded into, and a full
    /// pool waits instead of failing.
    #[test]
    fn held_frames_keep_their_buffers_until_dropped() {
        let mut d = rung();
        let aus = split_h264_aus(H264_25FPS);
        let mut held = Vec::new();
        let mut next = aus.iter();
        while held.len() < 6 {
            let f = d
                .decode(next.next().unwrap())
                .unwrap()
                .expect("a free buffer");
            assert!(held.iter().all(|h: &DmabufFrame| h.pool_key != f.pool_key));
            held.push(f);
        }
        assert!(d.decode(next.next().unwrap()).unwrap().is_none());
        held.remove(0);
        assert!(d.decode(next.next().unwrap()).unwrap().is_some());
    }

    /// A unit the driver never received breaks its reference chain: nothing
    /// is shown, and a keyframe is asked for, until one arrives.
    #[test]
    fn a_lost_unit_withholds_pictures_until_the_next_keyframe() {
        let mut d = rung();
        let aus = split_h264_aus(H264_25FPS);
        assert!(d.decode(aus[0]).unwrap().is_some());
        fake(&mut d).frozen = true;
        let lost = aus[1..]
            .iter()
            .position(|au| d.decode(au).is_err())
            .expect("a frozen decoder runs out of input buffers");
        fake(&mut d).thaw();
        let after = aus[lost + 2];
        assert!(d.decode(after).unwrap().is_none(), "references are wrong");
        assert!(d.take_recovery_request());
        // The host answers with an IDR; the stream's first unit is one.
        let f = d.decode(aus[0]).unwrap().expect("the keyframe shows");
        assert!(f.keyframe);
        assert!(!d.take_recovery_request());
    }
}
