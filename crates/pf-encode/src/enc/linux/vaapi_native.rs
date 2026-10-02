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
use crate::rfi::{self, plan_slot_recovery, Wave, WaveMark};

/// Slots a session keeps: how far back a recovery anchor may reach. A report
/// names frames the client missed two frames ago and spends a round trip
/// arriving, so the ring must still hold the picture before the loss — eight is
/// 80 ms at 100 fps, past a Wi-Fi report. The level's DPB may allow fewer.
const SLOTS: u8 = 8;

pub struct NativeVaapiEncoder {
    /// `None` between a [`Encoder::reset`] and the submit that reopens.
    session: Option<Session>,
    params: SessionParams,
    codec: CodecParams,
    hdr: Option<HdrStatic>,
    force_kf: bool,
    /// A loss plan's anchor, consumed by the next submit.
    anchor: Option<usize>,
    /// Intra refresh wave in flight: the rung a loss with no anchor takes instead of the
    /// IDR. An anchor or a forced IDR abandons it; a loss reported while it runs spoils it
    /// and queues a fresh one behind it, never a restart: iHD tracks each reference's
    /// refreshed rows itself and a stripe that jumps back to the top never heals there.
    wave: Option<Wave>,
    wave_spoiled: bool,
    wave_queued: bool,
    /// The host's wire index minus the session's own picture count.
    wire_offset: i64,
    frames: u64,
    /// One [`PendingMeta`] per picture the session holds, in encode order —
    /// `poll` pairs a collected picture back with its meta.
    meta: VecDeque<PendingMeta>,
    /// `set_pipelined`: `poll` probes the oldest pending encode instead of
    /// waiting the driver's surface out.
    pipelined: bool,
    /// 24-bit CPU frames are repacked to 32 here.
    repack: Vec<u8>,
    /// The part of each picture this session encodes ([`Encoder::set_input_crop`]).
    crop: Option<[u32; 4]>,
    /// `PUNKTFUNK_VAAPI_DUMP=<file>`: every access unit, appended, for a decoder to look at.
    dump: Option<std::fs::File>,
}

impl NativeVaapiEncoder {
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
        };
        params.slots = SLOTS.min(match codec {
            CodecParams::H264 => params.h264_max_slots(),
            CodecParams::Hevc { .. } => params.hevc_max_slots(),
        });
        let mut this = Self {
            session: None,
            params,
            codec,
            hdr: None,
            force_kf: true,
            anchor: None,
            wave: None,
            wave_spoiled: false,
            wave_queued: false,
            wire_offset: 0,
            frames: 0,
            meta: VecDeque::new(),
            pipelined: false,
            repack: Vec::new(),
            crop: None,
            dump: std::env::var("PUNKTFUNK_VAAPI_DUMP")
                .ok()
                .and_then(|p| std::fs::File::create(&p).ok()),
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

/// Probe picture size: large enough to exercise the real import + VPP path.
const PROBE_DIM: u32 = 64;

/// Allocate a 64x64 BO tiled exactly as `modifier` on `node`'s GBM device, one plane.
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

/// Prove one candidate: allocate the modifier on the session's own render node
/// and run a real 64x64 H.264/8-bit dmabuf submit through the native session —
/// `Display::import_dmabuf` plus the source VPP into the NV12 target is the
/// import contract being proved (the source VPP also serves a 10-bit source).
fn probe_capture_modifier(node: &std::path::Path, fourcc: u32, modifier: u64) -> bool {
    use pf_frame::DmabufFrame;
    let Some(bo) = alloc_probe_bo(node, fourcc, modifier) else {
        return false;
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
    enc.submit(&frame).is_ok()
}

/// The `candidates` subset libva actually imports for `fourcc` on this render
/// node, proved by a real GBM alloc + encoder submit per candidate. Verdicts are
/// cached per (node rdev, fourcc, modifier) so each is proved once per device.
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

/// Whether the host's render node offers an encode entrypoint for `codec`
/// at this depth — what a native open needs. AV1 is not a native path.
pub fn probe_can_encode(codec: Codec, ten_bit: bool) -> bool {
    use pf_vaapi::config::{VA_PROFILE_H264_HIGH, VA_PROFILE_HEVC_MAIN, VA_PROFILE_HEVC_MAIN10};
    use pf_vaapi::enc_h264::{VA_ENTRYPOINT_ENC_SLICE, VA_ENTRYPOINT_ENC_SLICE_LP};
    let profile = match (codec, ten_bit) {
        (Codec::H264, false) => VA_PROFILE_H264_HIGH,
        (Codec::H265, false) => VA_PROFILE_HEVC_MAIN,
        (Codec::H265, true) => VA_PROFILE_HEVC_MAIN10,
        _ => return false,
    };
    let node = pf_gpu::linux_render_node();
    let display = match Libva::load().and_then(|va| Display::open_path(va, &node.to_string_lossy()))
    {
        Ok(d) => d,
        Err(e) => {
            tracing::info!(error = %format!("{e:#}"), "no VAAPI display to probe");
            return false;
        }
    };
    display
        .entrypoints(profile)
        .map(|e| {
            e.iter()
                .any(|&p| p == VA_ENTRYPOINT_ENC_SLICE || p == VA_ENTRYPOINT_ENC_SLICE_LP)
        })
        .unwrap_or(false)
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
        planes.push(ExportedPlane {
            fd,
            offset: d.offset + d.stride * frame.height,
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
            FramePayload::Cpu(bytes) => {
                let (fourcc, bytes) = packed_rgb(frame.format, bytes, &mut self.repack)?;
                session.submit_packed(
                    bytes,
                    fourcc,
                    frame.width,
                    frame.height,
                    frame.width as usize * 4,
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
                "a CUDA frame reached the VAAPI encoder — that payload is NVENC-only; unset \
                 PUNKTFUNK_ZEROCOPY or do not pin PUNKTFUNK_ENCODER=vaapi-native on an NVIDIA host"
            ),
        };
        if self.force_kf || self.anchor.is_some() {
            self.wave = None;
            self.wave_spoiled = false;
            self.wave_queued = false;
        }
        let wave = self.wave;
        let mark = wave.map_or(WaveMark::None, |w| w.mark(self.wave_spoiled));
        // An owed IDR outranks a recovery anchor, as on NVENC and Vulkan.
        let anchor = self.anchor.take().filter(|_| !self.force_kf);
        match (anchor, wave) {
            (Some(slot), _) => session.encode_anchored(slot)?,
            (None, Some(w)) => {
                let (first_row, rows) = w.stripe(session.wave_rows());
                let stripe = Stripe {
                    first_row: first_row as u16,
                    rows: rows as u16,
                };
                // A spoiled close still leans on the lost frame: never an anchor.
                session.encode_wave(stripe, !w.closes() || self.wave_spoiled)?
            }
            (None, None) => session.encode(self.force_kf)?,
        }
        self.force_kf = false;
        if let Some(w) = wave {
            self.wave = w.next();
            if self.wave.is_none() {
                self.wave_spoiled = false;
                if std::mem::take(&mut self.wave_queued) {
                    self.wave = Some(Wave::start(w.cycle));
                }
            }
        }
        let pts_ns = self.frames * 1_000_000_000 / u64::from(self.params.fps_num.max(1));
        self.frames += 1;
        self.meta.push_back(PendingMeta {
            pts_ns,
            mark,
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
    /// one. `false` when nothing older survives — the caller keyframes.
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
        let plan = plan_slot_recovery(&refs, first);
        session.distrust(plan.tainted);
        self.anchor = plan.anchor.map(|(slot, _)| slot);
        if self.anchor.is_some() {
            return true;
        }
        if rfi::wave_enabled() && session.next_wire() > 0 {
            if self.wave.is_some() {
                // The client re-armed at this loss: this wave runs out unmarked and a
                // fresh one, whose start and close it counts, starts behind it.
                self.wave_spoiled = true;
                self.wave_queued = true;
                tracing::debug!(first, last, "vaapi-native RFI: loss mid-wave — wave queued");
                return true;
            }
            // No anchor, but a wave heals without one.
            let cycle = rfi::wave_cycle(
                session.wave_rows(),
                (self.params.fps_num / self.params.fps_den.max(1)).max(1),
                u32::MAX,
                rfi::pinned_cycle(),
            );
            self.wave = Some(Wave::start(cycle));
            tracing::debug!(
                first,
                last,
                cycle,
                "vaapi-native RFI: no reference older than the loss — starting an intra \
                 refresh wave instead of an IDR"
            );
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
        self.wave = None;
        self.wave_spoiled = false;
        self.wave_queued = false;
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

/// The packed-RGB fourcc a CPU frame uploads as, repacking 24-bit to 32 on the
/// way. `Bgrx` uploads as `BGRA`: same bytes, and iHD allocates no `BGRX`.
fn packed_rgb<'a>(
    format: PixelFormat,
    bytes: &'a [u8],
    repack: &'a mut Vec<u8>,
) -> Result<(u32, &'a [u8])> {
    Ok(match format {
        PixelFormat::Bgrx | PixelFormat::Bgra => (vpp::VA_FOURCC_BGRA, bytes),
        PixelFormat::Rgbx | PixelFormat::Rgba => (vpp::VA_FOURCC_RGBA, bytes),
        PixelFormat::X2Rgb10 => (vpp::VA_FOURCC_X2R10G10B10, bytes),
        PixelFormat::X2Bgr10 => (vpp::VA_FOURCC_X2B10G10R10, bytes),
        PixelFormat::Bgr | PixelFormat::Rgb => {
            repack.clear();
            repack.reserve(bytes.len() / 3 * 4);
            for px in bytes.chunks_exact(3) {
                repack.extend_from_slice(px);
                repack.push(255);
            }
            let fourcc = if format == PixelFormat::Bgr {
                vpp::VA_FOURCC_BGRA
            } else {
                vpp::VA_FOURCC_RGBA
            };
            (fourcc, repack.as_slice())
        }
        other => bail!("no native VAAPI ingest for CPU {other:?} frames"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 24-bit CPU frames become 32-bit with an opaque alpha; 32-bit ones upload
    /// as they are.
    #[test]
    fn packed_rgb_repacks_24_bit_only() {
        let mut scratch = Vec::new();
        let (fourcc, out) =
            packed_rgb(PixelFormat::Bgr, &[1, 2, 3, 4, 5, 6], &mut scratch).unwrap();
        assert_eq!(fourcc, vpp::VA_FOURCC_BGRA);
        assert_eq!(out, &[1, 2, 3, 255, 4, 5, 6, 255]);
        let bytes = [9u8; 8];
        let (fourcc, out) = packed_rgb(PixelFormat::Bgrx, &bytes, &mut scratch).unwrap();
        assert_eq!(fourcc, vpp::VA_FOURCC_BGRA);
        assert_eq!(out.as_ptr(), bytes.as_ptr(), "no copy");
        assert!(packed_rgb(PixelFormat::Nv12, &bytes, &mut scratch).is_err());
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
        assert!(enc.wave.is_some(), "the wave starts where the IDR used to");
        assert!(enc.reset());
        enc.submit(&frame(11)).expect("submit after reset");
        assert!(
            enc.poll().unwrap().unwrap().keyframe,
            "a rebuild starts with an IDR"
        );
    }
    /// BGRX frame of horizontal bands scrolled down by `shift` rows, with a diagonal so no
    /// two rows are alike: the encoder must reach for rows above to predict it.
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
        let mut enc =
            NativeVaapiEncoder::open(codec, w, h, 60, 4_000_000, 8, ChromaFormat::Yuv420, false)
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
                let w = enc.wave.expect("the wave is armed");
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
                assert!(enc.wave_spoiled && enc.wave_queued, "spoiled, one queued");
                assert_eq!(enc.wave.map(|w| w.index), Some(2), "the sweep runs on");
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
        assert!(enc.wave.is_none(), "the wave closed");
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
}
