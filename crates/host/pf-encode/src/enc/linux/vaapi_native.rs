//! The VAAPI encoder behind [`Encoder`]: `pf_libva`'s session, and the only one.
//!
//! A loss is answered by predicting from a slot the client still has
//! (`invalidate_ref_frames`), or by an intra refresh wave when none survives
//! (`design/vulkan-intra-refresh.md` §10), an ABR step retargets in place
//! (`reconfigure_bitrate`), and HEVC Main 10 carries the HDR10 SEI.
//! `submit` enqueues and `poll` collects — blocking until `set_pipelined` turns
//! the wait into a `vaQuerySurfaceStatus` probe under GPU contention.
//!
//! H.264 and HEVC on AMD and Intel. AV1 there is Vulkan Video's.

use std::collections::VecDeque;
use std::os::fd::AsRawFd as _;

use anyhow::{anyhow, bail, ensure, Context, Result};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use pf_libva::encode::{CodecParams, Encoder as Session, Stripe};
use pf_libva::{Display, DmabufSource, Libva};
use pf_vaapi::drm::ExportedPlane;
use pf_vaapi::enc_params::SessionParams;
use pf_vaapi::hevc::{HdrStatic, COLOUR_BT2020_PQ, COLOUR_BT709};
use pf_vaapi::vpp;
use pf_zerocopy::gbm::{GbmBo, GbmDevice, GBM_BO_USE_RENDERING};

use super::{ChromaFormat, Codec, EncodedFrame, Encoder, EncoderCaps};
use crate::cpu_ingest::{CpuIngest, Target};
use crate::env_knobs::Knobs;
use crate::rfi::{
    self, plan_slot_recovery, FrameRef, MidWave, Pending, Recovery, RecoveryPolicy, WaveMark,
};

/// Slots a session keeps: how far back a recovery anchor may reach. A report
/// names frames the client missed two frames ago and spends a round trip
/// arriving, so the ring must still hold the picture before the loss — eight is
/// 80 ms at 100 fps, past a Wi-Fi report. The level's DPB may allow fewer.
const SLOTS: u8 = 8;

/// The wave knobs, read once when the encoder opens so a frame does not scan the environment.
#[derive(Clone, Copy)]
struct WaveKnobs {
    /// [`rfi::wave_enabled`].
    enabled: bool,
    /// [`rfi::pinned_cycle`].
    pinned: Option<u32>,
}

pub struct NativeVaapiEncoder {
    /// `None` between a [`Encoder::reset`] and the submit that reopens.
    session: Option<Session>,
    params: SessionParams,
    waves: WaveKnobs,
    codec: CodecParams,
    hdr: Option<HdrStatic>,
    force_kf: bool,
    /// A loss plan's anchor, consumed by the next picture that encodes.
    anchor: Option<usize>,
    /// The newest frame the client confirmed, while the host holds confirmed references.
    reference_floor: Option<crate::Acked>,
    /// What the client confirmed it decoded clean, whatever the link.
    confirmed: Option<crate::Acked>,
    /// The two pictures the session never replaces, in host wires: a late loss's anchor.
    kept: rfi::Kept,
    /// Intra refresh wave in flight: the rung a loss with no anchor takes instead of the
    /// IDR ([`recovery_policy`]).
    recovery: Recovery,
    /// The host's wire index minus the session's own picture count.
    wire_offset: i64,
    frames: u64,
    /// One [`PendingMeta`] per picture the session holds, in encode order —
    /// `poll` pairs a collected picture back with its meta.
    meta: VecDeque<PendingMeta>,
    /// `set_pipelined`: `poll` probes the oldest pending encode instead of
    /// waiting the driver's surface out.
    pipelined: bool,
    /// Lays out CPU frames as 4-byte pixels; its scratch is reused across frames.
    ingest: CpuIngest,
    /// The part of each picture this session encodes ([`Encoder::set_input_crop`]).
    crop: Option<[u32; 4]>,
    /// `PUNKTFUNK_VAAPI_DUMP=<file>`: every access unit, appended, for a decoder to look at.
    dump: Option<std::fs::File>,
}

impl NativeVaapiEncoder {
    /// A session at `full_range` (`false` is studio): the VPP writes it and the VUI states it.
    /// A producer's NV12 or P010 encoded as imported arrives at it from capture's pod.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        codec: Codec,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        bit_depth: u8,
        chroma: ChromaFormat,
        // BT.2020 PQ vs BT.709. Independent of depth: 10-bit SDR is Main10 under BT.709.
        hdr: bool,
        full_range: bool,
        knobs: &Knobs,
    ) -> Result<Self> {
        ensure!(!chroma.is_444(), "the native VAAPI encoder is 4:2:0 only");
        let ten_bit = bit_depth == 10;
        let codec = match codec {
            Codec::H264 => {
                ensure!(
                    !ten_bit,
                    "ten-bit H.264 is not a path here; HEVC Main 10 is"
                );
                CodecParams::H264
            }
            Codec::H265 => CodecParams::Hevc {
                ten_bit,
                colour: if hdr { COLOUR_BT2020_PQ } else { COLOUR_BT709 },
            },
            Codec::Av1 | Codec::PyroWave => {
                bail!("the native VAAPI encoder does not encode {codec:?}")
            }
        };
        let mut params = SessionParams {
            width,
            height,
            fps_num: fps,
            fps_den: 1,
            bitrate_bps: bitrate_bps.min(u64::from(u32::MAX)) as u32,
            slots: SLOTS,
            max_num_reorder_frames: 0,
            initial_qp: 26,
            vbv_frames: super::vbv_frames_env() as f32,
            full_range,
        };
        params.slots = SLOTS.min(match codec {
            CodecParams::H264 => params.h264_max_slots(),
            CodecParams::Hevc { .. } => params.hevc_max_slots(),
        });
        let mut this = Self {
            session: None,
            params,
            waves: WaveKnobs {
                enabled: rfi::wave_enabled(),
                pinned: rfi::pinned_cycle(),
            },
            codec,
            hdr: None,
            force_kf: true,
            anchor: None,
            reference_floor: None,
            confirmed: None,
            kept: rfi::Kept::default(),
            recovery: Recovery::default(),
            wire_offset: 0,
            frames: 0,
            meta: VecDeque::new(),
            pipelined: false,
            ingest: CpuIngest::default(),
            crop: None,
            dump: knobs
                .vaapi_dump
                .as_deref()
                .and_then(|p| std::fs::File::create(p).ok()),
        };
        this.open_session()?;
        tracing::info!(
            ?codec,
            width,
            height,
            fps,
            bitrate_bps,
            slots = params.slots,
            "native VAAPI encode session open"
        );
        Ok(this)
    }

    /// Open on the GPU the host chose, never the first node that initialises.
    fn open_session(&mut self) -> Result<()> {
        let va = Libva::load().context("libva")?;
        let node = pf_gpu::linux_render_node();
        let display = Display::open_path(va, &node.to_string_lossy())
            .with_context(|| format!("VAAPI display on {}", node.display()))?;
        let mut session =
            Session::new(display, self.params, self.codec).map_err(|e| anyhow!("{e:#}"))?;
        session.set_hdr(self.hdr);
        session.set_source_crop(self.crop);
        self.session = Some(session);
        self.force_kf = true;
        Ok(())
    }
}

/// Probe picture size. iHD binds a Tile4 import compressed only above 64×64, so a smaller probe
/// never reads a compressed candidate the way a capture frame is read.
const PROBE_DIM: u32 = 128;
/// Edge of one probe cell, and the gray level each cell is painted at.
const PROBE_CELL: u32 = 32;
const PROBE_LEVELS: [u8; 4] = [32, 96, 160, 224];
/// Luma a cell may stray from its level and still read as written.
const PROBE_SLACK: u8 = 16;

/// Allocate a `PROBE_DIM`² BO tiled exactly as `modifier` on `node`'s GBM device, one plane.
/// `None` is a refused candidate, never a host failure.
fn alloc_probe_bo(node: &std::path::Path, fourcc: u32, modifier: u64) -> Option<GbmBo> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .ok()?;
    let device = std::rc::Rc::new(GbmDevice::open(file).ok()?);
    let bo = GbmBo::alloc(
        &device,
        PROBE_DIM,
        PROBE_DIM,
        fourcc,
        &[modifier],
        GBM_BO_USE_RENDERING,
    )
    .ok()?;
    (bo.planes == 1 && bo.modifier == modifier && bo.stride != 0).then_some(bo)
}

/// Prove one candidate on the session's own render node: the GPU paints gray cells
/// into a BO of that modifier, a real H.264/8-bit dmabuf submit goes through the
/// native session, then the same import is converted to NV12 and every cell must
/// read back at its level. An import libva accepts but reads wrong, such as a
/// compressed layout read raw, fails here. A probe the driver cannot paint or read
/// back keeps the submit's verdict.
fn probe_capture_modifier(node: &std::path::Path, fourcc: u32, modifier: u64) -> bool {
    use pf_frame::DmabufFrame;
    let Some(bo) = alloc_probe_bo(node, fourcc, modifier) else {
        return false;
    };
    let painted = match paint_probe(&bo, fourcc) {
        Ok(painted) => painted,
        Err(e) => {
            tracing::info!(
                modifier = format_args!("{modifier:#x}"),
                error = %format!("{e:#}"),
                "VAAPI capture modifier kept unchecked: the probe was not painted"
            );
            false
        }
    };
    let Ok(mut enc) = NativeVaapiEncoder::open(
        Codec::H264,
        PROBE_DIM,
        PROBE_DIM,
        60,
        1_000_000,
        8,
        ChromaFormat::Yuv420,
        false,
        true,
        &Knobs::from_env(),
    ) else {
        return false;
    };
    let Ok(fd) = bo.fd.try_clone() else {
        return false;
    };
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: PROBE_DIM,
        height: PROBE_DIM,
        pts_ns: 0,
        format: PixelFormat::Bgrx,
        payload: FramePayload::Dmabuf(DmabufFrame {
            // A dup, owned by the frame: closed once on drop. `bo.fd` stays open
            // through the submit and is closed by the guard after.
            fd,
            fourcc,
            modifier,
            offset: bo.offset,
            stride: bo.stride,
            plane1: None,
            hold: None,
            health: pf_zerocopy::zero_copy_health(modifier ^ u64::from(fourcc)),
            rebuild: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }),
        cursor: None,
    };
    if enc.submit(&frame).is_err() {
        return false;
    }
    if !painted {
        return true;
    }
    let plane = ExportedPlane {
        fd: bo.fd.as_raw_fd(),
        offset: bo.offset,
        stride: bo.stride,
    };
    match probe_read_back(node, fourcc, modifier, &[plane]) {
        Ok(None) => true,
        Ok(Some(OffLevel { x, y, got, want })) => {
            tracing::info!(
                fourcc = format_args!("{fourcc:#010x}"),
                modifier = format_args!("{modifier:#x}"),
                x,
                y,
                got,
                want,
                "VAAPI capture modifier refused: the picture reads back wrong"
            );
            false
        }
        Err(e) => {
            tracing::info!(
                modifier = format_args!("{modifier:#x}"),
                error = %format!("{e:#}"),
                "VAAPI capture modifier kept unchecked: the read-back did not run"
            );
            true
        }
    }
}

/// Paint the probe's gray cells through the GPU. `false` for a fourcc it has no gray for.
fn paint_probe(bo: &GbmBo, fourcc: u32) -> Result<bool> {
    let Some(format) = PixelFormat::from_drm_fourcc(fourcc) else {
        return Ok(false);
    };
    if probe_gray(format, 0).is_none() {
        return Ok(false);
    }
    bo.write(PROBE_DIM, PROBE_DIM, |rows, pitch| {
        for y in 0..PROBE_DIM {
            for x in 0..PROBE_DIM {
                let at = y as usize * pitch + x as usize * 4;
                if let Some(px) = probe_gray(format, probe_level(x, y)) {
                    rows[at..at + 4].copy_from_slice(&px);
                }
            }
        }
    })?;
    Ok(true)
}

/// The level of the cell holding (`x`, `y`): every level on every row of cells.
fn probe_level(x: u32, y: u32) -> u8 {
    PROBE_LEVELS[((x / PROBE_CELL + 2 * (y / PROBE_CELL)) % 4) as usize]
}

/// One gray pixel of luma `v` in `format`'s little-endian bytes.
fn probe_gray(format: PixelFormat, v: u8) -> Option<[u8; 4]> {
    match format {
        PixelFormat::Bgrx | PixelFormat::Bgra | PixelFormat::Rgbx | PixelFormat::Rgba => {
            Some([v, v, v, 0xff])
        }
        PixelFormat::X2Rgb10 | PixelFormat::X2Bgr10 => {
            let c = u32::from(v) << 2 | u32::from(v) >> 6;
            Some((3 << 30 | c << 20 | c << 10 | c).to_le_bytes())
        }
        _ => None,
    }
}

/// A probe cell read back off its level: the luma at its centre (`x`, `y`) and the level.
#[derive(Debug, PartialEq, Eq)]
struct OffLevel {
    x: u32,
    y: u32,
    got: u8,
    want: u8,
}

/// Import the painted probe again, convert it to full-range NV12 and return the first cell
/// off its level; `None` when every cell reads as painted. One plane is packed RGB, two a
/// producer's own NV12 or P010.
fn probe_read_back(
    node: &std::path::Path,
    fourcc: u32,
    modifier: u64,
    planes: &[ExportedPlane],
) -> Result<Option<OffLevel>> {
    use pf_vaapi::drm::VA_FOURCC_NV12;
    use pf_vaapi::vpp::VA_RT_FORMAT_YUV420;
    let display = Display::open_path(Libva::load()?, &node.to_string_lossy())?;
    let converter = pf_libva::vpp::Vpp::new(&display, PROBE_DIM, PROBE_DIM, true)?;
    let mut surfaces = Vec::new();
    let found = (|| {
        let source = display.import_dmabuf(&DmabufSource {
            width: PROBE_DIM,
            height: PROBE_DIM,
            drm_fourcc: fourcc,
            modifier,
            planes,
        })?;
        surfaces.push(source);
        let target = display.create_surface(
            VA_RT_FORMAT_YUV420,
            Some(VA_FOURCC_NV12),
            PROBE_DIM,
            PROBE_DIM,
        )?;
        surfaces.push(target);
        converter.convert(
            &display,
            source,
            (PROBE_DIM, PROBE_DIM),
            planes.len() == 1,
            COLOUR_BT709,
            target,
        )?;
        display.map_image(target, |image, ptr| {
            let pitch = image.pitches[0] as usize;
            let len = pitch * (PROBE_DIM as usize - 1) + PROBE_DIM as usize;
            // SAFETY: the mapped image holds its luma plane at `offsets[0]`, `PROBE_DIM` rows
            // `pitch` apart, until `map_image` unmaps it after this closure.
            let luma =
                unsafe { std::slice::from_raw_parts(ptr.add(image.offsets[0] as usize), len) };
            Ok(first_off_level(luma, pitch))
        })
    })();
    for surface in surfaces {
        display.destroy_surface(surface);
    }
    converter.destroy(&display);
    found
}

/// The first cell whose centre luma strays more than [`PROBE_SLACK`] from its level.
fn first_off_level(luma: &[u8], pitch: usize) -> Option<OffLevel> {
    let half = PROBE_CELL / 2;
    (0..PROBE_DIM / PROBE_CELL)
        .flat_map(|cy| (0..PROBE_DIM / PROBE_CELL).map(move |cx| (cx, cy)))
        .map(|(cx, cy)| (cx * PROBE_CELL + half, cy * PROBE_CELL + half))
        .find_map(|(x, y)| {
            let (got, want) = (luma[y as usize * pitch + x as usize], probe_level(x, y));
            (got.abs_diff(want) > PROBE_SLACK).then_some(OffLevel { x, y, got, want })
        })
}

/// Intel's single-plane layouts, newest first: 4-tiled, Y-tiled, X-tiled.
const INTEL_TILED: [u64; 3] = [
    0x0100_0000_0000_0009,
    0x0100_0000_0000_0002,
    0x0100_0000_0000_0001,
];

/// The layouts [`vaapi_capture_modifiers`] tries on the host's render node: what the Vulkan
/// device owning that node samples. A host can run VA with no Vulkan driver for the GPU; an
/// Intel node then tries its known layouts. The proof decides either way.
pub(crate) fn capture_candidates(fourcc: u32) -> Vec<u64> {
    let node = pf_gpu::linux_render_node();
    if let Some(list) = super::vk_util::node_capture_modifiers(fourcc, &node) {
        return list;
    }
    let intel = pf_gpu::enumerate()
        .iter()
        .find(|g| g.handle.render_node.as_deref() == Some(node.as_path()))
        .is_some_and(|g| g.vendor_id == pf_gpu::VENDOR_INTEL);
    if intel {
        INTEL_TILED.to_vec()
    } else {
        Vec::new()
    }
}

/// The `candidates` subset libva imports and reads right for `fourcc` on this render
/// node, each proved by [`probe_capture_modifier`]. Verdicts are cached per
/// (node rdev, fourcc, modifier) so each is proved once per device.
pub(crate) fn vaapi_capture_modifiers(fourcc: u32, candidates: &[u64]) -> Vec<u64> {
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Mutex, OnceLock};
    #[allow(clippy::type_complexity)]
    static CACHE: OnceLock<Mutex<HashMap<(u64, u32, u64), bool>>> = OnceLock::new();
    let node = pf_gpu::linux_render_node();
    let rdev = std::fs::metadata(&node).map(|m| m.rdev()).unwrap_or(0);
    let mut accepted: Vec<u64> = Vec::new();
    for &m in candidates {
        if m == 0 || accepted.contains(&m) {
            continue;
        }
        let key = (rdev, fourcc, m);
        // The lock guards the map, not the probe: a concurrent first probe of the
        // same candidate may run twice; the verdict is identical and last-write wins.
        let cached = CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .copied();
        let ok = match cached {
            Some(v) => v,
            None => {
                let v = probe_capture_modifier(&node, fourcc, m);
                CACHE
                    .get_or_init(|| Mutex::new(HashMap::new()))
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, v);
                v
            }
        };
        if ok {
            accepted.push(m);
        }
    }
    tracing::info!(
        fourcc = format_args!("{fourcc:#010x}"),
        ?accepted,
        "VAAPI: encoder-proved tiled capture modifiers (LINEAR is always appended)"
    );
    accepted
}

/// Intel's layouts a two-plane picture is read from where it lies: 4-tiled, Y-tiled. Mesa's
/// Intel driver allows a video surface no other tiling.
pub(crate) const INTEL_PLANAR_TILED: [u64; 2] = [0x0100_0000_0000_0009, 0x0100_0000_0000_0002];

/// What a producer's own NV12 or P010 may arrive as for the session that opens for `codec`
/// at this depth: the whole offer, LINEAR (0) included.
///
/// Intel's full encode entrypoint reads its source as tiled: the driver copies a LINEAR NV12
/// first and reads a LINEAR P010 as it lies. There the proved tiled layouts lead and a
/// ten-bit offer has no LINEAR. The low-power entrypoint reads LINEAR in place, and so does
/// every other vendor, so those keep LINEAR alone. Cached per (node, codec, depth).
pub(crate) fn planar_capture_modifiers(codec: Codec, ten_bit: bool) -> Vec<u64> {
    use std::collections::HashMap;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Mutex, OnceLock};
    #[allow(clippy::type_complexity)]
    static CACHE: OnceLock<Mutex<HashMap<(u64, bool, bool), Vec<u64>>>> = OnceLock::new();
    let node = pf_gpu::linux_render_node();
    let rdev = std::fs::metadata(&node).map(|m| m.rdev()).unwrap_or(0);
    let key = (rdev, codec == Codec::H264, ten_bit);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    // The lock guards the map, not the probe, as in `vaapi_capture_modifiers`.
    let cached = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .cloned();
    if let Some(offer) = cached {
        return offer;
    }
    let intel = pf_gpu::enumerate()
        .iter()
        .find(|g| g.handle.render_node.as_deref() == Some(node.as_path()))
        .is_some_and(|g| g.vendor_id == pf_gpu::VENDOR_INTEL);
    let offer = if intel && reads_tiled_only(codec, ten_bit) {
        let mut offer: Vec<u64> = INTEL_PLANAR_TILED
            .into_iter()
            .filter(|&m| probe_planar_modifier(&node, codec, ten_bit, m))
            .collect();
        if !ten_bit {
            offer.push(0);
        }
        tracing::info!(
            ?codec,
            ten_bit,
            ?offer,
            "VAAPI: the full encode entrypoint reads a tiled planar capture in place"
        );
        offer
    } else {
        vec![0]
    };
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(key, offer.clone());
    offer
}

/// Whether the session for `codec` at this depth runs on the full entrypoint. A session that
/// does not open answers `false`: nothing reads the capture then.
fn reads_tiled_only(codec: Codec, ten_bit: bool) -> bool {
    probe_session(codec, ten_bit)
        .ok()
        .and_then(|enc| enc.session.as_ref().map(|s| !s.low_power()))
        .unwrap_or(false)
}

/// A `PROBE_DIM`² session of the codec and depth a real one opens with.
fn probe_session(codec: Codec, ten_bit: bool) -> Result<NativeVaapiEncoder> {
    NativeVaapiEncoder::open(
        codec,
        PROBE_DIM,
        PROBE_DIM,
        60,
        1_000_000,
        if ten_bit { 10 } else { 8 },
        ChromaFormat::Yuv420,
        false,
        true,
        &Knobs::from_env(),
    )
}

/// The probe's gray cells as a two-plane picture in tight rows, chroma neutral. A ten-bit
/// sample is the level in the high byte of a little-endian word.
fn planar_probe_picture(ten_bit: bool) -> Vec<u8> {
    let sample = |v: u8| if ten_bit { vec![0, v] } else { vec![v] };
    let luma = (0..PROBE_DIM)
        .flat_map(|y| (0..PROBE_DIM).map(move |x| probe_level(x, y)))
        .flat_map(sample);
    let chroma = (0..PROBE_DIM * PROBE_DIM / 2).flat_map(|_| sample(128));
    luma.chain(chroma).collect()
}

/// [`probe_capture_modifier`] for a producer's own NV12 or P010: the Vulkan device that owns
/// the node paints the cells into an image of that modifier, the session of the real codec
/// and depth takes it as its picture, and the import must read back at every level. Any step
/// that does not run refuses the candidate; LINEAR or packed RGB carries the stream then.
fn probe_planar_modifier(
    node: &std::path::Path,
    codec: Codec,
    ten_bit: bool,
    modifier: u64,
) -> bool {
    use super::planar_probe;
    let (format, fourcc) = if ten_bit {
        (PixelFormat::P010, u32::from_le_bytes(*b"P010"))
    } else {
        (PixelFormat::Nv12, u32::from_le_bytes(*b"NV12"))
    };
    let dim = (PROBE_DIM, PROBE_DIM);
    let picture = planar_probe_picture(ten_bit);
    let Some(buf) = planar_probe::paint(node, modifier, ten_bit, dim, &picture) else {
        return false;
    };
    let (Ok(mut enc), Ok(fd)) = (probe_session(codec, ten_bit), buf.fd.try_clone()) else {
        return false;
    };
    let [luma, chroma] = buf.planes;
    let frame = CapturedFrame {
        provenance: Default::default(),
        width: PROBE_DIM,
        height: PROBE_DIM,
        pts_ns: 0,
        format,
        payload: FramePayload::Dmabuf(pf_frame::DmabufFrame {
            fd,
            fourcc,
            modifier,
            offset: luma.0,
            stride: luma.1,
            plane1: Some(chroma),
            hold: None,
            health: pf_zerocopy::zero_copy_health(modifier ^ u64::from(fourcc)),
            rebuild: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }),
        cursor: None,
    };
    if enc.submit(&frame).is_err() {
        return false;
    }
    let plane = |(offset, stride)| ExportedPlane {
        fd: buf.fd.as_raw_fd(),
        offset,
        stride,
    };
    match probe_read_back(node, fourcc, modifier, &[plane(luma), plane(chroma)]) {
        Ok(None) => true,
        Ok(Some(OffLevel { x, y, got, want })) => {
            tracing::info!(
                fourcc = format_args!("{fourcc:#010x}"),
                modifier = format_args!("{modifier:#x}"),
                x,
                y,
                got,
                want,
                "VAAPI capture modifier refused: the planar picture reads back wrong"
            );
            false
        }
        Err(e) => {
            tracing::info!(
                modifier = format_args!("{modifier:#x}"),
                error = %format!("{e:#}"),
                "VAAPI capture modifier refused: the planar read-back did not run"
            );
            false
        }
    }
}

/// Whether the host's render node offers an encode entrypoint for `codec`
/// at this depth — what a native open needs. AV1 is not a native path.
pub fn probe_can_encode(codec: Codec, ten_bit: bool) -> bool {
    probe_entrypoint(codec, ten_bit).unwrap_or(false)
}

/// [`probe_can_encode`] with a probe that could not ask told apart: `None` when no VAAPI
/// display opened, `Some(false)` when libva answered without an encode entrypoint.
pub(crate) fn probe_entrypoint(codec: Codec, ten_bit: bool) -> Option<bool> {
    use pf_vaapi::config::{VA_PROFILE_H264_HIGH, VA_PROFILE_HEVC_MAIN, VA_PROFILE_HEVC_MAIN10};
    use pf_vaapi::enc_h264::{VA_ENTRYPOINT_ENC_SLICE, VA_ENTRYPOINT_ENC_SLICE_LP};
    let profile = match (codec, ten_bit) {
        (Codec::H264, false) => VA_PROFILE_H264_HIGH,
        (Codec::H265, false) => VA_PROFILE_HEVC_MAIN,
        (Codec::H265, true) => VA_PROFILE_HEVC_MAIN10,
        _ => return Some(false),
    };
    let node = pf_gpu::linux_render_node();
    let display = match Libva::load().and_then(|va| Display::open_path(va, &node.to_string_lossy()))
    {
        Ok(d) => d,
        Err(e) => {
            // The resolution holds off asking again and warns once per entry, so this line
            // stays at debug.
            tracing::debug!(error = %format!("{e:#}"), "no VAAPI display to probe");
            return None;
        }
    };
    Some(
        display
            .entrypoints(profile)
            .map(|e| {
                e.iter()
                    .any(|&p| p == VA_ENTRYPOINT_ENC_SLICE || p == VA_ENTRYPOINT_ENC_SLICE_LP)
            })
            .unwrap_or(false),
    )
}

/// How a session recovers, on the knobs read at open. iHD tracks each reference's refreshed
/// rows itself and a stripe that jumps back to the top never heals there, so a loss mid-wave
/// queues a fresh wave behind the one in flight. There is no wave before the first picture.
fn recovery_policy(session: &Session, params: &SessionParams, waves: WaveKnobs) -> RecoveryPolicy {
    let fps = (params.fps_num / params.fps_den.max(1)).max(1);
    RecoveryPolicy {
        cycle: (waves.enabled && session.next_wire() > 0)
            .then(|| rfi::wave_cycle(session.wave_rows(), fps, u32::MAX, waves.pinned)),
        mid_wave: MidWave::Queue,
        abandons: true,
    }
}

/// What `poll` pairs back to a collected picture. `source_hold` is the deferred-requeue
/// hold of a direct-ingested dmabuf: kept in the queue until `collect` synced the encode,
/// it makes the producer's rewrite wait out the GPU read.
struct PendingMeta {
    pts_ns: u64,
    mark: WaveMark,
    source_hold: Option<pf_frame::FrameHold>,
}

/// Describe one captured dmabuf for libva and route tiled rejection back to capture.
/// `true` = the import was retained as the encode source (VPP was skipped), so the
/// caller must hold the producer off the buffer until collect.
fn submit_captured_dmabuf(
    session: &mut Session,
    frame: &CapturedFrame,
    d: &pf_frame::DmabufFrame,
) -> Result<bool> {
    let fd = d.fd.as_raw_fd();
    let mut planes = vec![ExportedPlane {
        fd,
        offset: d.offset,
        stride: d.stride,
    }];
    if let Some((offset, stride)) = d.plane1 {
        planes.push(ExportedPlane { fd, offset, stride });
    } else if d.fourcc == vpp::DRM_FORMAT_NV12 || d.fourcc == vpp::DRM_FORMAT_P010 {
        // Producer-reported numbers: a wrapped sum would import plane 1 from the wrong place.
        let offset = d
            .stride
            .checked_mul(frame.height)
            .and_then(|n| n.checked_add(d.offset))
            .context("contiguous chroma plane offset overflows u32")?;
        planes.push(ExportedPlane {
            fd,
            offset,
            stride: d.stride,
        });
    }
    let imported = session.submit_dmabuf(&DmabufSource {
        width: frame.width,
        height: frame.height,
        drm_fourcc: d.fourcc,
        modifier: d.modifier,
        planes: &planes,
    });
    let direct = match imported {
        Ok(direct) => direct,
        Err(e) => {
            if d.modifier != 0 {
                super::vk_util::reject_dmabuf(d, &format!("{e:#}"));
            }
            return Err(e);
        }
    };
    if d.modifier != 0 {
        d.health.note_raw_import_ok();
    }
    Ok(direct)
}

impl Encoder for NativeVaapiEncoder {
    /// A mirrored head or crop may scale down, but cannot be smaller or change shape.
    /// Tiled dmabuf rejection marks its capture for a LINEAR rebuild before returning.
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        let [cx, cy, cw, ch] = self
            .crop
            .unwrap_or([0, 0, frame.width, frame.height])
            .map(u64::from);
        let (fw, fh) = (cw, ch);
        let (ew, eh) = (u64::from(self.params.width), u64::from(self.params.height));
        ensure!(
            cx + cw <= u64::from(frame.width)
                && cy + ch <= u64::from(frame.height)
                && fw >= ew
                && fh >= eh
                && (fw * eh).abs_diff(fh * ew) < 2 * fw.max(fh),
            "captured frame {}x{} (encoding {cw}x{ch} at {cx},{cy}) does not fit encoder {}x{}",
            frame.width,
            frame.height,
            self.params.width,
            self.params.height
        );
        if self.session.is_none() {
            self.open_session()?;
        }
        let session = self.session.as_mut().expect("opened above");
        let source_hold = match &frame.payload {
            FramePayload::Cpu(_) => {
                let staged = self.ingest.stage(frame, Target::AsCaptured)?;
                let fourcc = packed_rgb(staged.fmt)?;
                session.submit_packed(
                    staged.bytes,
                    fourcc,
                    frame.width,
                    frame.height,
                    staged.row,
                )?;
                None
            }
            // The hold matters only when the import itself is the encode source: a
            // VPP conversion has already read the dmabuf by the time it returns.
            FramePayload::Dmabuf(d) => {
                if submit_captured_dmabuf(session, frame, d)? {
                    d.hold.clone()
                } else {
                    None
                }
            }
            FramePayload::Cuda(_) => bail!(
                "a CUDA frame reached the VAAPI encoder — that payload is NVENC-only; set \
                 PUNKTFUNK_ZEROCOPY=0 or don't pin PUNKTFUNK_ENCODER=vaapi-native on an NVIDIA host"
            ),
            FramePayload::Pixelless => {
                bail!("the VAAPI encoder needs pixels; got a pixel-less frame")
            }
        };
        // The link loses packets: the newest picture the client confirmed, as an anchor.
        // None resident keeps the chain.
        let confirmed = self
            .reference_floor
            .filter(|_| self.anchor.is_none())
            .and_then(|acked| {
                let refs: Vec<(usize, i64)> = session
                    .slots()
                    .into_iter()
                    .map(|(slot, wire)| (slot, wire + self.wire_offset))
                    .collect();
                rfi::pick_acked(&refs, &acked).map(|(slot, _)| slot)
            });
        let pending = Pending {
            idr: self.force_kf,
            anchor: self.anchor.is_some(),
            acked: confirmed.is_some(),
            loss: false,
        };
        let policy = recovery_policy(session, &self.params, self.waves);
        let plan = rfi::plan_frame(self.recovery, pending, &policy);
        let slot = self.anchor.or(confirmed);
        // An owed IDR outranks a recovery anchor, as on NVENC and Vulkan.
        match (slot.filter(|_| plan.refs != FrameRef::Idr), plan.wave) {
            (Some(slot), _) => session.encode_anchored(slot)?,
            (None, Some(w)) => {
                let (first_row, rows) = w.stripe(session.wave_rows());
                let stripe = Stripe {
                    first_row: first_row as u16,
                    rows: rows as u16,
                };
                // A spoiled close still leans on the lost frame: never an anchor.
                session.encode_wave(stripe, plan.dirty)?
            }
            (None, None) => session.encode(plan.refs == FrameRef::Idr)?,
        }
        // The picture is in: spend the plan. A failed encode above leaves the anchor, the
        // owed IDR and the wave as they were, and the retry plans the same frame.
        self.anchor = None;
        self.force_kf = false;
        self.recovery = plan.next;
        let offset = self.wire_offset;
        let held: Vec<i64> = session
            .slots()
            .into_iter()
            .map(|(_, w)| w + offset)
            .collect();
        let wire = session.next_wire() - 1 + offset;
        self.kept
            .step(wire, !plan.dirty, self.confirmed.as_ref(), |w| {
                held.contains(&w)
            });
        session.keep([self.kept.pin, self.kept.candidate].map(|w| w.map(|w| w - offset)));
        let pts_ns = self.frames * 1_000_000_000 / u64::from(self.params.fps_num.max(1));
        self.frames += 1;
        self.meta.push_back(PendingMeta {
            pts_ns,
            mark: plan.mark,
            source_hold,
        });
        Ok(())
    }

    /// The session numbers pictures from zero; the host's index is that plus an
    /// offset, re-learnt on every submit so a rebuild cannot desync the two.
    fn submit_indexed(&mut self, frame: &CapturedFrame, wire_index: u32) -> Result<()> {
        if let Some(s) = &self.session {
            self.wire_offset = i64::from(wire_index) - s.next_wire();
        }
        self.submit(frame)
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            supports_rfi: true,
            downscales_input: true,
            crops_input: true,
            ..Default::default()
        }
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn set_hdr_meta(&mut self, meta: Option<pf_frame::HdrMeta>) {
        self.hdr = meta.map(|m| HdrStatic {
            display_primaries: m.display_primaries,
            white_point: m.white_point,
            max_display_mastering_luminance: m.max_display_mastering_luminance,
            min_display_mastering_luminance: m.min_display_mastering_luminance,
            max_cll: m.max_cll,
            max_fall: m.max_fall,
        });
        if let Some(s) = &mut self.session {
            s.set_hdr(self.hdr);
        }
    }

    /// The slot plan on the session's trusted slots, in the host's wire domain:
    /// taint what the loss touched, anchor the next picture on the newest older
    /// one. With none, a wave answers. `false` when the session cannot wave either —
    /// the caller keyframes.
    fn invalidate_ref_frames(&mut self, first: i64, last: i64) -> bool {
        if first < 0 || last < first {
            return false;
        }
        let Some(session) = &mut self.session else {
            return false;
        };
        let refs: Vec<(usize, i64)> = session
            .slots()
            .into_iter()
            .map(|(slot, wire)| (slot, wire + self.wire_offset))
            .collect();
        let plan = plan_slot_recovery(&refs, first, self.reference_floor.as_ref());
        session.distrust(plan.tainted);
        self.anchor = plan.anchor.map(|(slot, _)| slot);
        if self.anchor.is_some() {
            return true;
        }
        // A wave in flight sweeps away a loss before its start; one inside the sweep spoils
        // its close. The start is `index` pictures back: a skipped wire index only moves it
        // earlier, which spoils more, never less.
        let next_ts = session.next_wire() + self.wire_offset;
        let spoils = self
            .recovery
            .wave
            .is_some_and(|w| w.spoiled_by(Some(next_ts - i64::from(w.index)), next_ts, last));
        let policy = recovery_policy(session, &self.params, self.waves);
        if let Some(next) = self.recovery.lose(&policy, spoils) {
            tracing::debug!(
                first,
                last,
                mid_wave = self.recovery.wave.is_some(),
                cycle = ?policy.cycle,
                "vaapi-native RFI: no reference older than the loss — an intra refresh wave \
                 instead of an IDR"
            );
            self.recovery = next;
            return true;
        }
        tracing::debug!(
            first,
            last,
            slots = refs.len(),
            "vaapi-native RFI declined: the ring holds no reference older than the loss — \
             caller falls back to its (coalesced) keyframe path"
        );
        false
    }

    fn set_reference_floor(&mut self, acked: Option<crate::Acked>) {
        self.reference_floor = acked;
    }

    fn note_confirmed(&mut self, acked: Option<crate::Acked>) {
        self.confirmed = acked;
    }

    fn distrust_references(&mut self) {
        // A pending anchor names a slot that is no longer trusted.
        self.anchor = None;
        if let Some(s) = &mut self.session {
            s.distrust_all();
        }
    }

    /// Collect the oldest finished picture. Sync mode waits the driver's surface
    /// out; `pipelined` probes it, so an encode still running yields `None` and
    /// the AU rides a tick behind instead of holding the loop.
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let Some(session) = &mut self.session else {
            return Ok(None);
        };
        let Some(pic) = session.collect(!self.pipelined)? else {
            return Ok(None);
        };
        // The hold rides `meta` through `collect`: the producer stays locked out of
        // the source dmabuf until the encode that read it has synced.
        let PendingMeta {
            pts_ns,
            mark,
            source_hold: _hold,
        } = self
            .meta
            .pop_front()
            .expect("every enqueued picture queued its meta");
        if let Some(f) = &mut self.dump {
            use std::io::Write as _;
            let _ = f.write_all(&pic.bytes);
        }
        Ok(Some(EncodedFrame {
            data: pic.bytes,
            pts_ns,
            keyframe: pic.is_idr,
            recovery_anchor: pic.recovery_anchor,
            recovery_point: mark.point() && !pic.is_idr,
            recovery_close: mark.close() && !pic.is_idr,
            chunk_aligned: false,
        }))
    }

    /// Probe instead of wait: under contention the AU rides a tick behind rather
    /// than holding the loop on the driver's surface.
    fn set_pipelined(&mut self, on: bool) -> bool {
        self.pipelined = on;
        self.pipelined
    }

    /// Drop the session; the next submit reopens it with an IDR.
    fn reset(&mut self) -> bool {
        self.session = None;
        self.meta.clear();
        self.anchor = None;
        self.recovery = Recovery::default();
        self.force_kf = true;
        true
    }

    /// Retarget in place: the next picture is rate-controlled to `bps`.
    ///
    /// VA-API has no query for what the driver settled on — the rate goes out
    /// with every picture in `VAEncMiscParameterRateControl` and nothing comes
    /// back — so [`Encoder::applied_bitrate_bps`] reports what this session will
    /// ask for, and [`va_rate_bps`] is the one clamp it can know about.
    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        let bps = va_rate_bps(bps);
        self.params.bitrate_bps = bps;
        if let Some(s) = &mut self.session {
            s.set_bitrate(bps);
        }
        true
    }

    fn applied_bitrate_bps(&self) -> Option<u64> {
        Some(u64::from(self.params.bitrate_bps))
    }

    fn set_input_crop(&mut self, rect: [u32; 4]) -> Result<()> {
        self.crop = Some(rect);
        if let Some(session) = self.session.as_mut() {
            session.set_source_crop(self.crop);
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The rate a VA-API session can actually ask for.
///
/// `VAEncMiscParameterRateControl::bits_per_second` is a `u32`, so a higher ask
/// is truncated. Reported as applied rather than swallowed: the caller reads the
/// truncation as a short apply and stops promising the client a rate no VA-API
/// driver can be told about.
fn va_rate_bps(asked_bps: u64) -> u32 {
    u32::try_from(asked_bps).unwrap_or_else(|_| {
        tracing::warn!(
            asked_bps,
            applied_bps = u32::MAX,
            "VA-API rate control carries a 32-bit rate — the ask is truncated"
        );
        u32::MAX
    })
}

/// The packed-RGB fourcc a CPU frame staged in its own order ([`CpuIngest`]) uploads
/// as. `Bgrx` uploads as `BGRA`: same bytes, and iHD allocates no `BGRX`.
fn packed_rgb(staged: PixelFormat) -> Result<u32> {
    Ok(match staged {
        PixelFormat::Bgrx | PixelFormat::Bgra => vpp::VA_FOURCC_BGRA,
        PixelFormat::Rgbx | PixelFormat::Rgba => vpp::VA_FOURCC_RGBA,
        PixelFormat::X2Rgb10 => vpp::VA_FOURCC_X2R10G10B10,
        PixelFormat::X2Bgr10 => vpp::VA_FOURCC_X2B10G10R10,
        other => bail!("no native VAAPI ingest for CPU {other:?} frames"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe read back as painted passes; one cell off its level by more than the slack
    /// names that cell.
    #[test]
    fn the_probe_check_finds_a_cell_off_its_level() {
        let pitch = PROBE_DIM as usize + 64;
        let mut luma = vec![0u8; pitch * PROBE_DIM as usize];
        for y in 0..PROBE_DIM {
            for x in 0..PROBE_DIM {
                luma[y as usize * pitch + x as usize] = probe_level(x, y);
            }
        }
        assert_eq!(first_off_level(&luma, pitch), None);
        luma[48 * pitch + 80] = probe_level(80, 48) + PROBE_SLACK;
        assert_eq!(first_off_level(&luma, pitch), None, "within the slack");
        luma[48 * pitch + 80] = probe_level(80, 48) + PROBE_SLACK + 1;
        let want = probe_level(80, 48);
        assert_eq!(
            first_off_level(&luma, pitch),
            Some(OffLevel {
                x: 80,
                y: 48,
                got: want + PROBE_SLACK + 1,
                want
            })
        );
    }

    /// Every row of cells holds every level, and gray packs the same in each 10-bit order.
    #[test]
    fn the_probe_paints_every_level_in_each_fourcc() {
        let mut seen: Vec<u8> = (0..PROBE_DIM / PROBE_CELL)
            .map(|cx| probe_level(cx * PROBE_CELL, 0))
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, PROBE_LEVELS);
        assert_eq!(probe_gray(PixelFormat::Bgrx, 96), Some([96, 96, 96, 0xff]));
        let ten = u32::from_le_bytes(probe_gray(PixelFormat::X2Rgb10, 255).unwrap());
        assert_eq!(ten, 0xffff_ffff);
        assert_eq!(
            probe_gray(PixelFormat::X2Bgr10, 160),
            probe_gray(PixelFormat::X2Rgb10, 160)
        );
        assert_eq!(probe_gray(PixelFormat::Nv12, 96), None);
    }

    /// Each packed-RGB capture candidate of this box's render node through the full proof,
    /// with refusals logged. Ignored: needs a VAAPI device; run with `--nocapture`.
    #[test]
    #[ignore = "needs a VAAPI device"]
    fn capture_candidates_prove_on_this_device() {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_env_filter("pf_encode=info")
            .try_init();
        for fourcc in [vpp::DRM_FORMAT_XRGB8888, vpp::DRM_FORMAT_ARGB8888] {
            let candidates = capture_candidates(fourcc);
            let accepted = vaapi_capture_modifiers(fourcc, &candidates);
            println!("{fourcc:#010x}: candidates {candidates:#x?} accepted {accepted:#x?}");
            assert!(accepted.iter().all(|m| candidates.contains(m)));
        }
    }

    /// The read-back catches a wrong read on real hardware: a probe painted in the first tiled
    /// candidate, imported as the last, must leave a cell off its level. Ignored: needs a VAAPI
    /// device with two tiled candidates.
    #[test]
    #[ignore = "needs a VAAPI device"]
    fn the_read_back_catches_a_layout_read_as_another() {
        let fourcc = vpp::DRM_FORMAT_XRGB8888;
        let tiled: Vec<u64> = capture_candidates(fourcc)
            .into_iter()
            .filter(|&m| m != 0)
            .collect();
        let (Some(&painted), Some(&declared)) = (tiled.first(), tiled.last()) else {
            panic!("no tiled candidate on this device");
        };
        assert_ne!(painted, declared, "needs two tiled candidates");
        let node = pf_gpu::linux_render_node();
        let bo = alloc_probe_bo(&node, fourcc, painted).expect("allocate the probe");
        assert!(paint_probe(&bo, fourcc).expect("paint the probe"));
        let plane = [ExportedPlane {
            fd: bo.fd.as_raw_fd(),
            offset: bo.offset,
            stride: bo.stride,
        }];
        assert_eq!(
            probe_read_back(&node, fourcc, painted, &plane).unwrap(),
            None
        );
        let wrong = probe_read_back(&node, fourcc, declared, &plane).unwrap();
        println!("{painted:#x} read as {declared:#x}: {wrong:?}");
        assert!(wrong.is_some(), "a layout read as another passed");
    }

    /// 24-bit CPU frames become 32-bit with an opaque alpha; 32-bit ones upload
    /// as they are.
    #[test]
    fn packed_rgb_repacks_24_bit_only() {
        let frame = |format, width, bytes: &[u8]| CapturedFrame {
            width,
            height: 1,
            pts_ns: 0,
            format,
            payload: FramePayload::Cpu(bytes.to_vec()),
            cursor: None,
            provenance: Default::default(),
        };
        let mut ingest = CpuIngest::default();
        let bgr = frame(PixelFormat::Bgr, 2, &[1, 2, 3, 4, 5, 6]);
        let out = ingest.stage(&bgr, Target::AsCaptured).unwrap();
        assert_eq!(packed_rgb(out.fmt).unwrap(), vpp::VA_FOURCC_BGRA);
        assert_eq!(out.bytes, &[1, 2, 3, 255, 4, 5, 6, 255]);
        let bgrx = frame(PixelFormat::Bgrx, 2, &[9; 8]);
        let out = ingest.stage(&bgrx, Target::AsCaptured).unwrap();
        assert_eq!(packed_rgb(out.fmt).unwrap(), vpp::VA_FOURCC_BGRA);
        let FramePayload::Cpu(payload) = &bgrx.payload else {
            unreachable!()
        };
        assert_eq!(out.bytes.as_ptr(), payload.as_ptr(), "no copy");
        assert!(packed_rgb(PixelFormat::Nv12).is_err());
    }

    /// The rate control buffer is 32 bits wide, so an ask past it is truncated
    /// and reported as truncated — never echoed back as if the driver took it.
    #[test]
    fn a_rate_past_the_buffers_width_is_reported_truncated() {
        assert_eq!(va_rate_bps(2_000_000), 2_000_000);
        assert_eq!(va_rate_bps(u64::from(u32::MAX)), u32::MAX);
        assert_eq!(va_rate_bps(u64::from(u32::MAX) + 1), u32::MAX);
        assert_eq!(va_rate_bps(u64::MAX), u32::MAX);
    }

    /// The trait contract on real silicon: an IDR first, P after, a loss answered
    /// by a recovery anchor and not an IDR, and a bitrate step accepted in place.
    ///
    /// `cargo test -p pf-encode native_vaapi_smoke -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn native_vaapi_smoke() {
        let (w, h) = (320u32, 240u32);
        let mut enc = NativeVaapiEncoder::open(
            Codec::H264,
            w,
            h,
            60,
            4_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            true,
            &Knobs::from_env(),
        )
        .expect("open");
        assert!(enc.caps().supports_rfi);
        let frame = |i: u32| {
            let mut buf = vec![0u8; (w * h * 4) as usize];
            for px in buf.chunks_exact_mut(4) {
                px.copy_from_slice(&[(i * 8) as u8, 0x40, 0xC0, 0xFF]);
            }
            CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: u64::from(i) * 16_666_666,
                format: PixelFormat::Bgrx,
                payload: FramePayload::Cpu(buf),
                cursor: None,
            }
        };
        let mut aus = Vec::new();
        for i in 0..10 {
            enc.submit_indexed(&frame(i), 100 + i).expect("submit");
            aus.push(enc.poll().expect("poll").expect("an AU per submit"));
        }
        assert!(aus[0].keyframe);
        assert!(aus[1..]
            .iter()
            .all(|au| !au.keyframe && !au.recovery_anchor));
        // Wire 108 and 109 were lost; the anchor must be 107.
        assert!(
            enc.invalidate_ref_frames(108, 109),
            "a slot older than the loss survives"
        );
        assert!(enc.reconfigure_bitrate(2_000_000));
        assert_eq!(enc.applied_bitrate_bps(), Some(2_000_000));
        enc.submit_indexed(&frame(10), 110).expect("submit");
        let recovery = enc.poll().expect("poll").expect("an AU");
        assert!(recovery.recovery_anchor && !recovery.keyframe);
        // Everything is tainted: no anchor, so a wave answers instead of the IDR.
        assert!(enc.invalidate_ref_frames(100, 110));
        assert!(
            enc.recovery.wave.is_some(),
            "the wave starts where the IDR used to"
        );
        assert!(enc.reset());
        enc.submit(&frame(11)).expect("submit after reset");
        assert!(
            enc.poll().unwrap().unwrap().keyframe,
            "a rebuild starts with an IDR"
        );
    }
    /// BGRX frame `i` of `smoke_pattern::scroll_pattern`: rows band by luma and move `i`
    /// frames' worth (down by default), so the encoder must reach for rows above to predict it.
    fn scroll_frame(w: u32, h: u32, i: u32) -> CapturedFrame {
        let buf = crate::smoke_pattern::scroll_pattern(w as usize, h as usize, i as usize);
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: u64::from(i) * 16_666_666,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(buf),
            cursor: None,
        }
    }

    /// The wave replaces the IDR: an RFI with every reference tainted starts one; its start
    /// and close AU carry the mark, nothing in between does, no IDR follows frame 0, and a
    /// later loss of the plain P after the wave re-anchors on the close (a fully swept
    /// picture is trusted). `PF_WAVE_DUMP=<dir>` writes the full stream and the client's
    /// view with the pre-wave P frames lost: decoded side by side, the close must match
    /// the full decode.
    ///
    /// `cargo test -p pf-encode native_vaapi_wave -- --ignored --nocapture`
    fn run_wave_smoke(codec: Codec, ext: &str) {
        let (w, h) = (256u32, 256u32);
        let mut enc = NativeVaapiEncoder::open(
            codec,
            w,
            h,
            60,
            4_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            true,
            &Knobs::from_env(),
        )
        .expect("open");
        const WAVE_START: usize = 3;
        // `PF_WAVE_SPOIL=1`: two frames into the wave a frame inside its sweep is lost, so
        // it closes unmarked and the wave queued behind it carries the start and close.
        let restart = std::env::var("PF_WAVE_SPOIL").is_ok_and(|v| v == "1");
        let mut start2 = WAVE_START;
        let mut aus = Vec::new();
        let mut cycle = 0usize;
        let mut i = 0usize;
        loop {
            if i == WAVE_START {
                assert!(
                    enc.invalidate_ref_frames(0, WAVE_START as i64 - 1),
                    "a wave-capable encoder answers an RFI with no anchor"
                );
                let w = enc.recovery.wave.expect("the wave is armed");
                assert_eq!(w.index, 0);
                cycle = w.cycle as usize;
                if restart {
                    start2 = WAVE_START + cycle;
                }
                eprintln!(
                    "run_wave_smoke: {} rows, cycle {cycle}, low_power={}",
                    enc.session.as_ref().unwrap().wave_rows(),
                    enc.session.as_ref().unwrap().low_power()
                );
            }
            if restart && i == WAVE_START + 2 {
                let lost = (i - 1) as i64;
                assert!(
                    enc.invalidate_ref_frames(lost, lost),
                    "a loss inside the sweep"
                );
                assert!(
                    enc.recovery.spoiled && enc.recovery.queued,
                    "spoiled, one queued"
                );
                assert_eq!(
                    enc.recovery.wave.map(|w| w.index),
                    Some(2),
                    "the sweep runs on"
                );
            }
            let after_wave = start2 + cycle; // the plain P after the close
            let anchor_p = after_wave + 1;
            if cycle > 0 && i == anchor_p {
                assert!(
                    enc.invalidate_ref_frames(after_wave as i64, after_wave as i64),
                    "the wave close is a trusted anchor"
                );
            }
            enc.submit_indexed(&scroll_frame(w, h, i as u32), i as u32)
                .expect("submit");
            aus.push(enc.poll().expect("poll").expect("an AU per submit"));
            if cycle > 0 && i == anchor_p {
                break;
            }
            i += 1;
        }
        let close = start2 + cycle - 1;
        let after_wave = close + 1;
        let anchor_p = after_wave + 1;
        assert!(enc.recovery.wave.is_none(), "the wave closed");
        assert!(aus[0].keyframe, "frame 0 is the IDR");
        for (i, au) in aus.iter().enumerate().skip(1) {
            assert!(!au.data.is_empty(), "AU {i} empty");
            assert!(
                !au.keyframe,
                "AU {i}: no IDR after frame 0 — the wave replaced it"
            );
            let start = i == WAVE_START || i == start2;
            assert_eq!(
                au.recovery_point,
                start || i == close,
                "AU {i}: recovery_point marks every start and the close"
            );
            assert_eq!(au.recovery_close, i == close, "AU {i}: the close bit");
            assert_eq!(
                au.recovery_anchor,
                i == anchor_p,
                "AU {i}: the only anchor P answers the post-wave loss"
            );
        }
        if let Ok(dir) = std::env::var("PF_WAVE_DUMP") {
            let full: Vec<u8> = aus.iter().flat_map(|a| a.data.iter().copied()).collect();
            let p = format!("{dir}/vaenc-wave-smoke.{ext}");
            std::fs::write(&p, &full).unwrap_or_else(|e| panic!("write {p}: {e}"));
            // The client's view: the pre-wave P frames lost, and the restart's frame too.
            let dropped: Vec<u8> = aus
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    *i == 0 || (*i >= WAVE_START && !(restart && *i == WAVE_START + 1))
                })
                .flat_map(|(_, a)| a.data.iter().copied())
                .collect();
            let p2 = format!("{dir}/vaenc-wave-smoke-dropped.{ext}");
            std::fs::write(&p2, &dropped).unwrap_or_else(|e| panic!("write {p2}: {e}"));
            eprintln!(
                "run_wave_smoke: wrote {p} ({} bytes, {} AUs) and {p2} (frames 1..{} dropped; \
                 the close at {close} must decode identical to the full stream)",
                full.len(),
                aus.len(),
                WAVE_START,
            );
        }
    }

    /// The anchor soak ([`crate::smoke_pattern::Soak`], its `PF_WAVE_*` knobs). With
    /// `PF_WAVE_CONFIRM` and a `PF_WAVE_LAG` past the slot ring, every ask anchors on the kept
    /// picture instead of starting a wave.
    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn native_vaapi_anchor_soak() {
        let soak = crate::smoke_pattern::Soak::from_env();
        let (w, h) = (soak.w, soak.h);
        let bps = soak.mbps * 1_000_000;
        let knobs = Knobs::from_env();
        let mut enc = NativeVaapiEncoder::open(
            soak.codec,
            w,
            h,
            soak.fps,
            bps,
            8,
            ChromaFormat::Yuv420,
            false,
            true,
            &knobs,
        )
        .expect("open");
        soak.run("vaapi", &mut enc, |i| scroll_frame(w, h, i as u32));
    }

    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn native_vaapi_wave_h264() {
        run_wave_smoke(Codec::H264, "h264");
    }

    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn native_vaapi_wave_hevc() {
        run_wave_smoke(Codec::H265, "h265");
    }

    /// A direct-ingest hold lives exactly as long as its `PendingMeta`: once the
    /// frame and the original Arc are gone the meta still pins it, and popping the
    /// meta is what lets the producer back in.
    #[test]
    fn pending_meta_keeps_the_source_hold_alive() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        struct Probe(Arc<AtomicBool>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let probe = Arc::new(Probe(dropped.clone()));
        let meta = PendingMeta {
            pts_ns: 0,
            mark: WaveMark::None,
            source_hold: Some(probe),
        };
        assert!(
            !dropped.load(Ordering::SeqCst),
            "PendingMeta keeps the hold alive"
        );
        drop(meta);
        assert!(dropped.load(Ordering::SeqCst));
    }

    /// Coded-buffer exhaustion plus `ready`: five submits ahead of any collect must
    /// not drop a picture — the fifth submit's `acquire_coded` drains the oldest into
    /// `ready`, and every AU comes back in encode order.
    ///
    /// `cargo test -p pf-encode native_vaapi_collect_depth -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn native_vaapi_collect_depth() {
        let (w, h) = (320u32, 240u32);
        let mut enc = NativeVaapiEncoder::open(
            Codec::H264,
            w,
            h,
            60,
            4_000_000,
            8,
            ChromaFormat::Yuv420,
            false,
            true,
            &Knobs::from_env(),
        )
        .expect("open");
        for i in 0..5u32 {
            enc.submit_indexed(&scroll_frame(w, h, i), i)
                .expect("submit");
        }
        let mut aus = Vec::new();
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
        assert_eq!(aus.len(), 5, "every queued picture drains");
        assert!(aus[0].keyframe);
        assert!(aus[1..].iter().all(|au| !au.keyframe));
        assert!(
            aus.windows(2).all(|w| w[0].pts_ns < w[1].pts_ns),
            "AUs come back in encode order"
        );
    }

    /// The probe agrees with an open: H.264 and both HEVC depths yes, AV1 and
    /// ten-bit H.264 no.
    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn native_probe_matches_open() {
        assert!(probe_can_encode(Codec::H264, false));
        assert!(probe_can_encode(Codec::H265, false));
        assert!(probe_can_encode(Codec::H265, true));
        assert!(!probe_can_encode(Codec::Av1, false));
        assert!(!probe_can_encode(Codec::H264, true));
    }

    /// A producer's planar picture in tight rows: luma, then chroma. Dark left half, bright
    /// right half, and a coloured band over picture rows `h/4..h/2`.
    #[cfg(any(feature = "nvenc", feature = "vulkan-encode"))]
    fn planar_picture(w: u32, h: u32, ten_bit: bool) -> Vec<u8> {
        let sample = |v: u8| if ten_bit { vec![0, v] } else { vec![v] };
        let luma = (0..h)
            .flat_map(|_| (0..w).map(move |x| if x < w / 2 { 60 } else { 200 }))
            .flat_map(sample);
        let chroma = (0..h / 2).flat_map(move |cy| {
            let band = (h / 8..h / 4).contains(&cy);
            let pair = if band { [90, 180] } else { [128, 128] };
            (0..w).map(move |i| pair[(i % 2) as usize])
        });
        luma.chain(chroma.flat_map(sample)).collect()
    }

    /// [`planar_picture`] as `modifier` lays it out, through the lane that hands it to the
    /// encoder as imported: the first decoded picture's Y, U and V planes at 8 bits. LINEAR
    /// puts chroma on the 16-row boundary gamescope puts it on.
    #[cfg(any(feature = "nvenc", feature = "vulkan-encode"))]
    fn direct_planar(
        codec: Codec,
        (w, h): (u32, u32),
        ten_bit: bool,
        modifier: u64,
    ) -> [Vec<u8>; 3] {
        use crate::test_dmabuf::{decode_first, LinearDmabuf};
        let node = pf_gpu::linux_render_node();
        let tight = planar_picture(w, h, ten_bit);
        let (fmt, fourcc, depth) = if ten_bit {
            (PixelFormat::P010, b"P010", 10u8)
        } else {
            (PixelFormat::Nv12, b"NV12", 8)
        };
        let stride = w * u32::from(depth.div_ceil(8));
        let luma_len = (stride * h) as usize;
        // Held to the end: each frame is a dup of one of these.
        let (linear, tiled);
        let frame = if modifier == 0 {
            let chroma_row = h.next_multiple_of(16);
            let mut bytes = tight[..luma_len].to_vec();
            bytes.resize((stride * chroma_row) as usize, 0);
            bytes.extend_from_slice(&tight[luma_len..]);
            let vendor = pf_gpu::enumerate()
                .iter()
                .find(|g| g.handle.render_node.as_deref() == Some(node.as_path()))
                .map(|g| g.vendor_id)
                .expect("the render node's GPU");
            linear = LinearDmabuf::on(vendor, &bytes);
            linear.planar_frame(fmt, fourcc, (w, h), stride, chroma_row * stride)
        } else {
            tiled = crate::planar_probe::paint(&node, modifier, ten_bit, (w, h), &tight)
                .expect("the node's Vulkan device has that planar image");
            let mut frame = linear_frame_shell(fmt, fourcc, (w, h), &tiled.fd);
            if let FramePayload::Dmabuf(d) = &mut frame.payload {
                d.modifier = modifier;
                (d.offset, d.stride) = tiled.planes[0];
                d.plane1 = Some(tiled.planes[1]);
            }
            frame
        };
        let mut enc = NativeVaapiEncoder::open(
            codec,
            w,
            h,
            60,
            20_000_000,
            depth,
            ChromaFormat::Yuv420,
            false,
            false,
            &Knobs::from_env(),
        )
        .expect("open");
        enc.submit(&frame).expect("submit");
        let stream = enc.poll().expect("poll").expect("an AU").data;
        let name = format!("pf-direct-{codec:?}-{depth}-{modifier:x}");
        let raw = decode_first(
            &stream,
            &name,
            if ten_bit { "yuv420p10le" } else { "yuv420p" },
        );
        let eight: Vec<u8> = if ten_bit {
            raw.chunks_exact(2)
                .map(|p| (u16::from_le_bytes([p[0], p[1]]) >> 2) as u8)
                .collect()
        } else {
            raw
        };
        let (luma, chroma) = ((w * h) as usize, (w * h / 4) as usize);
        assert_eq!(eight.len(), luma + 2 * chroma, "{name}: one 4:2:0 picture");
        [
            eight[..luma].to_vec(),
            eight[luma..luma + chroma].to_vec(),
            eight[luma + chroma..].to_vec(),
        ]
    }

    /// A held dmabuf frame over `fd`, its layout left for the caller to fill.
    #[cfg(any(feature = "nvenc", feature = "vulkan-encode"))]
    fn linear_frame_shell(
        fmt: PixelFormat,
        fourcc: &[u8; 4],
        (w, h): (u32, u32),
        fd: &std::os::fd::OwnedFd,
    ) -> CapturedFrame {
        CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 0,
            format: fmt,
            payload: FramePayload::Dmabuf(pf_frame::DmabufFrame {
                fd: fd.try_clone().expect("dup"),
                fourcc: u32::from_le_bytes(*fourcc),
                modifier: 0,
                plane1: None,
                offset: 0,
                stride: 0,
                hold: Some(std::sync::Arc::new(())),
                health: pf_zerocopy::zero_copy_health(0x5003),
                rebuild: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
            cursor: None,
        }
    }

    /// The decoded [`planar_picture`]: its luma edge in one column on every row, its colour
    /// on the band's rows and on no others. A misread pitch walks the edge; chroma read from
    /// another row moves the band.
    #[cfg(any(feature = "nvenc", feature = "vulkan-encode"))]
    fn assert_planar_picture(name: &str, [y, u, v]: &[Vec<u8>; 3], w: u32, h: u32) {
        let (w, h) = (w as usize, h as usize);
        for row in [8, h / 2, h - 8] {
            let edge = (0..w).find(|&x| y[row * w + x] > 128);
            assert_eq!(edge, Some(w / 2), "{name}: luma edge on row {row}");
        }
        let (cw, ch) = (w / 2, h / 2);
        let near = |got: u8, want: i32| (i32::from(got) - want).abs() <= 16;
        for (row, (want_u, want_v)) in [
            (ch / 4 - 6, (128, 128)),
            (ch / 4 + 6, (90, 180)),
            (ch / 2 - 6, (90, 180)),
            (ch / 2 + 6, (128, 128)),
            (ch - 1, (128, 128)),
        ] {
            let (got_u, got_v) = (u[row * cw + cw / 4], v[row * cw + cw / 4]);
            assert!(
                near(got_u, want_u) && near(got_v, want_v),
                "{name}: chroma row {row} is ({got_u}, {got_v}), want ({want_u}, {want_v})"
            );
        }
    }

    /// Every layout the host offers a producer's own picture in decodes as the picture: at
    /// both depths, at a height off the 16-row grid and one on it. On Intel's full entrypoint
    /// that is the tiled layouts, and a ten-bit offer without LINEAR.
    #[cfg(any(feature = "nvenc", feature = "vulkan-encode"))]
    #[test]
    #[ignore = "needs a real VAAPI device and ffmpeg"]
    fn every_offered_planar_layout_decodes_as_the_picture() {
        for (codec, ten_bit) in [
            (Codec::H264, false),
            (Codec::H265, false),
            (Codec::H265, true),
        ] {
            let offer = planar_capture_modifiers(codec, ten_bit);
            eprintln!("{codec:?} ten_bit={ten_bit}: offer {offer:x?}");
            for modifier in offer {
                for (w, h) in [(1920, 1080), (1280, 720)] {
                    let name = format!("{codec:?} ten_bit={ten_bit} {modifier:#x} {w}x{h}");
                    let planes = direct_planar(codec, (w, h), ten_bit, modifier);
                    assert_planar_picture(&name, &planes, w, h);
                }
            }
        }
    }

    /// The full entrypoint reads a LINEAR P010 as if it were tiled, so the offer leaves it
    /// out there; the picture it would have made is not the picture.
    #[cfg(any(feature = "nvenc", feature = "vulkan-encode"))]
    #[test]
    #[ignore = "needs an Intel VAAPI device on its full entrypoint, and ffmpeg"]
    fn the_full_entrypoint_misreads_a_linear_p010() {
        assert!(
            reads_tiled_only(Codec::H265, true),
            "not the full entrypoint"
        );
        assert!(!planar_capture_modifiers(Codec::H265, true).contains(&0));
        let (w, h) = (1280, 720);
        let [y, _, _] = direct_planar(Codec::H265, (w, h), true, 0);
        let row = (h / 2 * w) as usize;
        let edge = (0..w as usize).find(|&x| y[row + x] > 128);
        assert_ne!(edge, Some(w as usize / 2), "the driver reads it right now");
    }
}
