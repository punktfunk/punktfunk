//! An encode session over libva: config, context, surfaces, and a short pending
//! queue between enqueue and collect — H.264 or HEVC, chosen at open.
//!
//! The unsafe half of the native VAAPI encoder. What a picture *is* — the parameter
//! sets, the parameter buffers — comes from [`pf_vaapi`], which is pure and tested
//! anywhere; this drives libva with it.
//!
//! Deliberately narrow: CBR, one slice per picture, one reference per picture. One
//! is not a simplification we chose — `VAConfigAttribEncMaxRefFrames` is `l0=1` on
//! radeonsi, so a second list entry would be advertised and ignored. Which one is
//! the point: every reference lives in a slot, and a loss is answered by predicting
//! from a slot the client still has, not by an IDR. H.264 keeps the slots as
//! long-term pictures; HEVC lists them in every slice header's reference picture
//! set.

use std::collections::VecDeque;
use std::os::raw::c_int;
use std::os::raw::c_void;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use pf_vaapi::config::VA_CONFIG_ATTRIB_RT_FORMAT;
use pf_vaapi::config::VA_PROFILE_H264_HIGH;
use pf_vaapi::config::VA_RT_FORMAT_YUV420;
use pf_vaapi::config::VA_RT_FORMAT_YUV420_10;
use pf_vaapi::enc_h264 as vah;
use pf_vaapi::enc_h265 as vahevc;
use pf_vaapi::enc_h265::HevcFeatures;
use pf_vaapi::enc_params::packed_slice_header;
use pf_vaapi::enc_params::va_pic_fields;
use pf_vaapi::enc_params::PictureSlice;
use pf_vaapi::enc_params::SessionParams;
use pf_vaapi::hevc::HdrStatic;
use pf_vaapi::hevc::HevcParams;
use pf_vaapi::hevc::HevcSlice;
use pf_vaapi::vpp::rt_format_for;
use pf_vaapi::vpp::VA_RT_FORMAT_RGB32;
use pf_vaapi::vpp::VA_RT_FORMAT_RGB32_10;

use crate::vpp::Vpp;
use crate::Display;
use crate::DmabufSource;
use crate::VaBufferId;
use crate::VaConfigAttrib;
use crate::VaContextId;
use crate::VaSurfaceId;
use crate::VA_INVALID_ID;

const VA_CONFIG_ATTRIB_RATE_CONTROL: u32 = 5;
const VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS: u32 = 10;
/// `VASurfaceStatus`: work is still queued against the surface.
const VA_SURFACE_RENDERING: c_int = 1;
/// `VASurfaceStatus`: a display pipeline holds the surface.
const VA_SURFACE_DISPLAYING: c_int = 2;
/// The loaded runtime exposes `vaSyncBuffer`, but this driver does not implement it.
const VA_STATUS_ERROR_UNIMPLEMENTED: c_int = 0x0000_0014;
/// `vaSyncBuffer`: the exact coded output is not complete at the requested deadline.
const VA_STATUS_ERROR_TIMEDOUT: c_int = 0x0000_0026;
const VA_TIMEOUT_INFINITE: u64 = u64::MAX;

/// Which codec a session opens, and the facts only that codec needs.
#[derive(Clone, Copy, Debug)]
pub enum CodecParams {
    H264,
    /// Main, or Main 10 with P010 surfaces; `colour` is the VUI's H.273 triple.
    Hevc {
        ten_bit: bool,
        colour: [u8; 3],
    },
}

/// The codec as opened: HEVC carries what the driver said it can do.
#[derive(Clone, Copy, Debug)]
enum Codec {
    H264,
    Hevc(HevcParams),
}

/// What one encoded picture came out as.
#[derive(Debug)]
pub struct EncodedPicture {
    pub bytes: Vec<u8>,
    /// `true` when this picture opened a GOP: it carries SPS, PPS and an IDR slice.
    pub is_idr: bool,
    /// Predicted from a pre-loss slot on request: the wire's recovery anchor.
    pub recovery_anchor: bool,
    /// This picture's index in the session, the number a slot remembers it by.
    pub wire: i64,
}

/// One reference the session still holds.
#[derive(Clone, Copy, Debug)]
struct Slot {
    surface: VaSurfaceId,
    wire: i64,
    /// HEVC's picture order count, what its reference picture set names it by.
    poc: i32,
    /// Cleared by [`H264Encoder::distrust`]; an untrusted slot is never predicted
    /// from again, and is replaced in its turn.
    trusted: bool,
    /// Part-refreshed picture of an intra refresh wave: resident and predicted from by
    /// the next wave frame, never an RFI anchor. The close is stored clean.
    dirty: bool,
}

/// The intra stripe one wave picture carries, in the driver's row unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stripe {
    pub first_row: u16,
    pub rows: u16,
}

/// One picture the driver is still encoding. Slot bookkeeping ran at enqueue —
/// the queue is in-order, so a later picture may already name this one as its
/// reference; [`Encoder::collect`] owes only the sync, the read-back and the
/// per-picture releases.
struct Pending {
    /// The picture's source surface — what `vaSyncSurface` names.
    surface: VaSurfaceId,
    /// A producer-direct import, destroyed at collect; session inputs are not.
    direct: Option<VaSurfaceId>,
    /// Its coded buffer, returned to the free pool once read.
    coded: VaBufferId,
    is_idr: bool,
    anchor: bool,
    wire: i64,
}

/// A live encode session. Owns its config, context, surfaces and coded buffers,
/// and releases them in the order libva requires.
pub struct Encoder {
    display: Display,
    codec: Codec,
    /// `VAEntrypointEncSliceLP` where the driver has it: Intel's fixed-function
    /// VDEnc, the only path under the frame budget there. AMD has only `EncSlice`.
    entrypoint: c_int,
    /// HDR10 static metadata, written as an SEI with every HEVC IDR.
    hdr: Option<HdrStatic>,
    /// HEVC picture order count of the next picture; 0 after an IDR.
    poc: i32,
    config: u32,
    context: VaContextId,
    /// Pictures the caller fills and `vaBeginPicture` reads.
    input: Vec<VaSurfaceId>,
    /// Where the driver writes each reconstruction — what `CurrPic` names and what a
    /// later `ReferenceFrames` entry points at. Distinct from the input: the driver
    /// writes these while it is still reading that, and a surface cannot be both.
    /// One more than there are slots, so the picture being written never shares a
    /// surface with a reference being read.
    recon: Vec<VaSurfaceId>,
    /// Reconstruction surfaces no slot holds.
    free: Vec<VaSurfaceId>,
    /// The long-term references, by `LongTermFrameIdx`.
    slots: Vec<Option<Slot>>,
    /// Pictures queued so far; the next one's `wire`.
    wire: i64,
    /// The whole pool — one per pending encode, so a picture drains while the
    /// next is written. `coded_free` is the submit-side bound: empty means the
    /// pitch is full and a collect runs first.
    coded: Vec<VaBufferId>,
    coded_free: Vec<VaBufferId>,
    /// Pictures submitted to the driver and not yet collected, in encode order.
    pending: VecDeque<Pending>,
    /// Collected early because a submit needed the coded buffer back.
    ready: VecDeque<EncodedPicture>,
    /// Ingest: RGB captures are converted — a larger one scaled — into the input
    /// surface here. A producer's own NV12/P010 skips it (`direct`). `None` only
    /// once `Drop` has destroyed it.
    vpp: Option<Vpp>,
    /// Where CPU RGB lands before conversion, and the (fourcc, width, height) it
    /// was made for.
    staging: Option<(VaSurfaceId, (u32, u32, u32))>,
    /// A producer's own NV12/P010 at the session's size: the next picture is encoded
    /// straight from this import, no VPP pass. Moves into `pending` at enqueue and
    /// is destroyed at collect.
    direct: Option<VaSurfaceId>,
    /// The first direct picture has been logged; a field log, not a per-frame one.
    direct_seen: bool,
    params: SessionParams,
    /// Frames encoded since the last IDR; `frame_num` in the slice header.
    frame_num: u16,
    /// Toggled on every IDR, so two in a row are told apart.
    idr_pic_id: u16,
    /// Rolling index for the picture being queued.
    next_surface: usize,
}

impl Encoder {
    /// Input surfaces rotate one past the four-picture encode pitch, so the surface a
    /// submit fills is never one an in-flight encode still reads.
    const SURFACES: usize = 5;

    /// Coded buffers — the pending pitch, matching NVENC's default asynchronous depth.
    /// Four lets VideoProc for the next frame overlap prior encode work under contention.
    const CODED_BUFS: usize = 4;

    /// Open a session on `display`. An error destroys the config and context made
    /// here; the surfaces and coded buffers go with the display, which this owns.
    pub fn new(display: Display, params: SessionParams, codec: CodecParams) -> Result<Self> {
        let coded_w = i32::from(params.width_in_mbs()) * 16;
        let coded_h = i32::from(params.height_in_mbs()) * 16;
        let (profile, rt_format) = match codec {
            CodecParams::H264 => (VA_PROFILE_H264_HIGH, VA_RT_FORMAT_YUV420),
            CodecParams::Hevc { ten_bit: false, .. } => {
                (vahevc::VA_PROFILE_HEVC_MAIN, VA_RT_FORMAT_YUV420)
            }
            CodecParams::Hevc { ten_bit: true, .. } => {
                (vahevc::VA_PROFILE_HEVC_MAIN10, VA_RT_FORMAT_YUV420_10)
            }
        };
        // Low power where it exists and rate-controls, unless
        // `PUNKTFUNK_VAAPI_LOW_POWER=0`. VDEnc's bitrate control is HuC firmware; a
        // box without it (the passthrough VM) offers CQP only there.
        let entrypoints = display.entrypoints(profile)?;
        let allow_lp = std::env::var("PUNKTFUNK_VAAPI_LOW_POWER")
            .ok()
            .is_none_or(|v| v != "0");
        let lp_has_cbr = || {
            let mut rc = [VaConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: 0,
            }];
            // SAFETY: `rc` is a live array of one entry the call fills in place.
            let status = unsafe {
                (display.va.get_config_attributes)(
                    display.display,
                    profile,
                    vah::VA_ENTRYPOINT_ENC_SLICE_LP,
                    rc.as_mut_ptr().cast::<c_void>(),
                    1,
                )
            };
            status == crate::VA_STATUS_SUCCESS && rc[0].value & vah::VA_RC_CBR != 0
        };
        let entrypoint =
            if allow_lp && entrypoints.contains(&vah::VA_ENTRYPOINT_ENC_SLICE_LP) && lp_has_cbr() {
                vah::VA_ENTRYPOINT_ENC_SLICE_LP
            } else if entrypoints.contains(&vah::VA_ENTRYPOINT_ENC_SLICE) {
                vah::VA_ENTRYPOINT_ENC_SLICE
            } else {
                bail!("no encode entrypoint for profile {profile} ({entrypoints:?})");
            };
        tracing::info!(profile, entrypoint, "VAAPI encode entrypoint");

        // Ask the driver what it supports before telling it what we want.
        // `vaCreateConfig` does not reject an attribute it dislikes — it drops it and
        // returns success, and a dropped packed-header attribute means every header
        // the app later supplies is discarded while the encode still reports fine.
        // The only way to know is to query, then pass back what came out.
        let mut probe = [
            VaConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: 0,
            },
            VaConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: 0,
            },
            VaConfigAttrib {
                kind: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: 0,
            },
            VaConfigAttrib {
                kind: vahevc::VA_CONFIG_ATTRIB_ENC_HEVC_FEATURES,
                value: 0,
            },
            VaConfigAttrib {
                kind: vahevc::VA_CONFIG_ATTRIB_ENC_HEVC_BLOCK_SIZES,
                value: 0,
            },
            VaConfigAttrib {
                kind: vahevc::VA_CONFIG_ATTRIB_PREDICTION_DIRECTION,
                value: 0,
            },
        ];
        // SAFETY: `probe` is a live array of exactly `probe.len()` entries the call
        // fills in place; profile and entrypoint are libva enum values.
        let status = unsafe {
            (display.va.get_config_attributes)(
                display.display,
                profile,
                entrypoint,
                probe.as_mut_ptr().cast::<c_void>(),
                probe.len() as c_int,
            )
        };
        display.va.check("vaGetConfigAttributes", status)?;

        let supported_rc = probe[1].value;
        let supported_packed = probe[2].value;
        if supported_rc & vah::VA_RC_CBR == 0 {
            bail!("this driver offers no CBR rate control for profile {profile}");
        }
        // The driver dictates HEVC's block sizes and which tools it has; the SPS is
        // written from its word, as ffmpeg does, and a silent driver gets ffmpeg's
        // guess.
        let codec = match codec {
            CodecParams::H264 => Codec::H264,
            CodecParams::Hevc { ten_bit, colour } => {
                let (features, blocks) = (probe[3].value, probe[4].value);
                let mut features = if features == vahevc::VA_ATTRIB_NOT_SUPPORTED
                    || blocks == vahevc::VA_ATTRIB_NOT_SUPPORTED
                {
                    HevcFeatures::guessed()
                } else {
                    HevcFeatures::from_attributes(features, blocks)
                };
                let direction = probe[5].value;
                features.gpb = direction != vahevc::VA_ATTRIB_NOT_SUPPORTED
                    && direction & vahevc::VA_PREDICTION_DIRECTION_BI_NOT_EMPTY != 0;
                Codec::Hevc(HevcParams {
                    common: params,
                    ten_bit,
                    colour,
                    features,
                })
            }
        };
        // Both kinds are required: the session writes every header itself, and
        // radeonsi drops the packed SPS and PPS of a picture that brings no slice
        // header.
        let want_packed =
            vah::VA_ENC_PACKED_HEADER_FLAG_SEQUENCE | vah::VA_ENC_PACKED_HEADER_FLAG_SLICE;
        let packed = supported_packed & want_packed;
        if packed & vah::VA_ENC_PACKED_HEADER_FLAG_SEQUENCE == 0 {
            bail!("this driver will not take a packed sequence header ({supported_packed:#x})");
        }
        if packed & vah::VA_ENC_PACKED_HEADER_FLAG_SLICE == 0 {
            bail!("this driver will not take a packed slice header ({supported_packed:#x})");
        }

        let attribs = [
            VaConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: rt_format,
            },
            VaConfigAttrib {
                kind: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: vah::VA_RC_CBR,
            },
            VaConfigAttrib {
                kind: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: packed,
            },
        ];
        let config = display.create_config(profile, entrypoint, &attribs)?;

        let slot_count = usize::from(params.slots.max(1));
        let surface_count = Self::SURFACES + slot_count + 1;
        let mut surfaces = vec![VA_INVALID_ID; surface_count];
        // SAFETY: `surfaces` is a live array of exactly `surface_count` ids the call
        // writes through; no surface attributes are passed, so the null is correct.
        let status = unsafe {
            (display.va.create_surfaces)(
                display.display,
                rt_format,
                coded_w as u32,
                coded_h as u32,
                surfaces.as_mut_ptr(),
                surface_count as u32,
                std::ptr::null_mut(),
                0,
            )
        };
        display.va.check("vaCreateSurfaces", status)?;
        let context =
            display.create_context(config.id(), coded_w as u32, coded_h as u32, &mut surfaces)?;

        // A coded buffer must hold the largest picture the session can emit. An IDR
        // at a low QP is far bigger than the average rate suggests; the uncompressed
        // frame is the bound that cannot be exceeded.
        let coded_size = (coded_w as u32 * coded_h as u32 * 3 / 2).max(1 << 20);
        let mut coded = Vec::with_capacity(Self::CODED_BUFS);
        for _ in 0..Self::CODED_BUFS {
            let mut buf = VA_INVALID_ID;
            // SAFETY: `context` is live, the size is non-zero, one element, and no
            // initial data — which is what a coded (output) buffer takes.
            let status = unsafe {
                (display.va.create_buffer)(
                    display.display,
                    context.id(),
                    vah::VA_ENC_CODED_BUFFER_TYPE,
                    coded_size,
                    1,
                    std::ptr::null_mut(),
                    &mut buf,
                )
            };
            display.va.check("vaCreateBuffer(coded)", status)?;
            coded.push(buf);
        }
        let coded_free = coded.clone();

        let vpp = Vpp::new(&display, params.width, params.height)?;

        let recon = surfaces.split_off(Self::SURFACES);
        let (config, context) = (config.keep(), context.keep());
        Ok(Self {
            display,
            codec,
            entrypoint,
            hdr: None,
            poc: 0,
            config,
            context,
            input: surfaces,
            free: recon.clone(),
            recon,
            slots: vec![None; slot_count],
            wire: 0,
            coded,
            coded_free,
            pending: VecDeque::new(),
            ready: VecDeque::new(),
            vpp: Some(vpp),
            staging: None,
            direct: None,
            direct_seen: false,
            params,
            frame_num: 0,
            idr_pic_id: 0,
            next_surface: 0,
        })
    }

    /// The `wire` the next picture will carry.
    pub fn next_wire(&self) -> i64 {
        self.wire
    }

    /// Whether the session runs on the driver's low-power entrypoint.
    pub fn low_power(&self) -> bool {
        self.entrypoint == vah::VA_ENTRYPOINT_ENC_SLICE_LP
    }

    /// Picture height in the driver's intra refresh row unit: macroblock rows for H.264;
    /// for HEVC 32-px rows on Intel's VDEnc (`ceil(height / 32)` in the driver) and CTB rows
    /// on AMD, whose firmware takes the stripe in CTBs.
    pub fn wave_rows(&self) -> u32 {
        let h = self.params.height;
        match self.codec {
            Codec::H264 => h.div_ceil(16),
            Codec::Hevc(_) if self.low_power() => h.div_ceil(32),
            Codec::Hevc(hevc) => h.div_ceil(8u32 << hevc.features.log2_ctb_minus3),
        }
    }

    /// The anchor candidates, as `(slot, wire)` — what `rfi::plan_slot_recovery`
    /// takes: trusted and fully refreshed.
    pub fn slots(&self) -> Vec<(usize, i64)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.filter(|s| s.trusted && !s.dirty).map(|s| (i, s.wire)))
            .collect()
    }

    /// Stop predicting from these slots: the plan's `tainted` mask.
    pub fn distrust(&mut self, mask: u32) {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if mask & (1 << i) != 0 {
                if let Some(s) = slot {
                    s.trusted = false;
                }
            }
        }
    }

    /// Stop predicting from every slot; the next picture is an IDR.
    pub fn distrust_all(&mut self) {
        self.distrust(u32::MAX);
    }

    /// The surface a caller writes its picture into before [`Self::encode`].
    pub fn input_surface(&self) -> VaSurfaceId {
        self.input[self.next_surface]
    }

    /// Retarget in place: the next picture is rate-controlled to `bps`, with no IDR
    /// and no rebuild — the ABR step that would otherwise cost a full rebuild.
    pub fn set_bitrate(&mut self, bps: u32) {
        self.params.bitrate_bps = bps;
    }

    /// What the next picture is rate-controlled to.
    pub fn bitrate_bps(&self) -> u32 {
        self.params.bitrate_bps
    }

    /// HDR10 static metadata to carry as an SEI on every HEVC IDR; `None` stops.
    pub fn set_hdr(&mut self, hdr: Option<HdrStatic>) {
        self.hdr = hdr;
    }

    /// Whether the session encodes ten-bit pictures, and so converts into P010.
    fn ten_bit(&self) -> bool {
        matches!(self.codec, Codec::Hevc(h) if h.ten_bit)
    }

    /// The VUI colour triple this session emits (`[1,1,1]` BT.709, `[9,16,9]` BT.2020 PQ). The
    /// VPP RGB→YUV matrix follows it, so a 10-bit SDR session converts as BT.709, not BT.2020.
    fn colour(&self) -> [u8; 3] {
        match self.codec {
            Codec::Hevc(h) => h.colour,
            _ => pf_vaapi::hevc::COLOUR_BT709,
        }
    }

    /// Drop a pending direct picture: its encode synced, or a later submit replaced it.
    fn clear_direct(&mut self) {
        if let Some(surface) = self.direct.take() {
            self.display.destroy_surface(surface);
        }
    }

    /// Fill the next input surface with NV12 from `y` and `uv`, for tests. Capture
    /// goes through [`Self::submit_packed`] or [`Self::submit_dmabuf`].
    pub fn write_nv12(&self, y: &[u8], uv: &[u8]) -> Result<()> {
        self.display.map_image(self.input_surface(), |image, ptr| {
            if image.num_planes < 2 {
                bail!("expected NV12 (2 planes), got {}", image.num_planes);
            }
            let rows = usize::from(image.height);
            let cols = usize::from(image.width);
            if y.len() < rows * cols || uv.len() < rows / 2 * cols {
                bail!("source planes are smaller than the surface");
            }
            for (plane, src, plane_rows) in [(0, y, rows), (1, uv, rows / 2)] {
                let pitch = image.pitches[plane] as usize;
                for row in 0..plane_rows {
                    // SAFETY: the mapped image is at least `data_size` bytes and
                    // the driver's own pitches and offsets bound every row written;
                    // the sources were length-checked above.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            src.as_ptr().add(row * cols),
                            ptr.add(image.offsets[plane] as usize + row * pitch),
                            cols,
                        );
                    }
                }
            }
            Ok(())
        })
    }

    fn vpp(&self) -> &Vpp {
        self.vpp.as_ref().expect("the VPP context lives until drop")
    }

    fn vpp_mut(&mut self) -> &mut Vpp {
        self.vpp.as_mut().expect("the VPP context lives until drop")
    }

    /// Take only `crop` (`x, y, width, height`) of every later picture; `None` is the whole.
    pub fn set_source_crop(&mut self, crop: Option<[u32; 4]>) {
        self.vpp_mut().crop = crop;
    }

    /// Ingest a `width`×`height` packed RGB picture from the CPU — eight-bit or
    /// ten-bit, by `fourcc` — uploaded to a staging surface and converted on the
    /// GPU into the next input surface at the session's depth. A picture larger
    /// than the session is scaled down on the way.
    pub fn submit_packed(
        &mut self,
        bytes: &[u8],
        fourcc: u32,
        width: u32,
        height: u32,
        row_bytes: usize,
    ) -> Result<()> {
        let rt_format = match rt_format_for(fourcc) {
            Some(rt @ (VA_RT_FORMAT_RGB32 | VA_RT_FORMAT_RGB32_10)) => rt,
            _ => bail!("no packed RGB ingest for fourcc {fourcc:#x}"),
        };
        self.clear_direct();
        let shape = (fourcc, width, height);
        let staging = match self.staging {
            Some((surface, s)) if s == shape => surface,
            _ => {
                if let Some((old, _)) = self.staging.take() {
                    self.display.destroy_surface(old);
                }
                let surface =
                    self.display
                        .create_surface(rt_format, Some(fourcc), width, height)?;
                self.staging = Some((surface, shape));
                surface
            }
        };
        self.display.write_packed(staging, bytes, row_bytes)?;
        self.vpp().convert(
            &self.display,
            staging,
            (width, height),
            true,
            self.colour(),
            self.input_surface(),
        )
    }

    /// Ingest a capture dmabuf, imported for this picture. A producer's own NV12/P010
    /// at the session's size and depth is encoded as imported; anything else is
    /// converted — and scaled down when larger — into the next input surface.
    /// `true` = the direct import was retained for encode (its producer hold must
    /// outlive GPU completion); `false` = VPP already consumed the source.
    pub fn submit_dmabuf(&mut self, source: &DmabufSource) -> Result<bool> {
        let rt_format = pf_vaapi::vpp::import_format(source.drm_fourcc)
            .map(|(_, rt)| rt)
            .ok_or_else(|| anyhow!("no ingest for DRM fourcc {:#x}", source.drm_fourcc))?;
        let is_rgb = rt_format == VA_RT_FORMAT_RGB32 || rt_format == VA_RT_FORMAT_RGB32_10;
        // Imported once per picture; a cache keyed on the fd would save the ioctl.
        let surface = self.display.import_dmabuf(source)?;
        if direct_ingest(
            rt_format,
            (source.width, source.height),
            (self.params.width, self.params.height),
            self.ten_bit(),
            self.vpp().crop.is_some(),
        ) {
            self.clear_direct();
            self.direct = Some(surface);
            if !self.direct_seen {
                self.direct_seen = true;
                tracing::info!(
                    fourcc = format_args!("{:#010x}", source.drm_fourcc),
                    ten_bit = self.ten_bit(),
                    "VAAPI: encoding the producer's own picture direct (no conversion pass)"
                );
            }
            return Ok(true);
        }
        let converted = self.vpp().convert(
            &self.display,
            surface,
            (source.width, source.height),
            is_rgb,
            self.colour(),
            self.input_surface(),
        );
        self.display.destroy_surface(surface);
        converted.map(|()| false)
    }

    /// Queue the picture currently in [`Self::input_surface`]; [`Self::collect`]
    /// hands the finished picture back.
    ///
    /// `force_idr` opens a GOP: SPS and PPS are packed ahead of the slice, so a
    /// client that joins here has parameter sets. Every other picture is a P
    /// predicted from the newest trusted slot — or an IDR when there is none.
    pub fn encode(&mut self, force_idr: bool) -> Result<()> {
        let newest = self
            .slots()
            .into_iter()
            .max_by_key(|&(_, wire)| wire)
            .map(|(slot, _)| slot);
        let reference = if force_idr { None } else { newest };
        self.encode_with(reference, false, None, false)
    }

    /// Queue the picture predicting from `slot` — the recovery anchor a loss plan
    /// picked. Refuses a slot that is empty or distrusted.
    pub fn encode_anchored(&mut self, slot: usize) -> Result<()> {
        match self.slots.get(slot) {
            Some(Some(s)) if s.trusted => self.encode_with(Some(slot), true, None, false),
            _ => bail!("slot {slot} holds no trusted reference"),
        }
    }

    /// Queue one frame of an intra refresh wave: `stripe` is coded intra, the rest
    /// predicts from the previous picture whatever its trust, and the result is
    /// stored `dirty` until the wave's close. An IDR when the session holds no
    /// picture at all.
    pub fn encode_wave(&mut self, stripe: Stripe, dirty: bool) -> Result<()> {
        let previous = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|s| (i, s.wire)))
            .max_by_key(|&(_, wire)| wire)
            .map(|(slot, _)| slot);
        self.encode_with(previous, false, Some(stripe), dirty)
    }

    /// A coded buffer for the next picture. Every one spoken for means the pitch
    /// is full of pendings — collect the oldest first; that wait is the
    /// submit-side bound, not caller latency.
    fn acquire_coded(&mut self) -> Result<VaBufferId> {
        if self.coded_free.is_empty() {
            // `collect_pending`, not `collect`: a `ready` hit would hand back a picture
            // without releasing a coded buffer, leaving none free below.
            if let Some(picture) = self.collect_pending(true)? {
                self.ready.push_back(picture);
            }
        }
        self.coded_free
            .pop()
            .ok_or_else(|| anyhow!("no coded buffer free after a collect"))
    }

    /// Queue one picture: parameter buffers rendered, the encode submitted, the
    /// slot bookkeeping done — the queue is in-order, so a later picture may
    /// already name this one as its reference. [`Self::collect`] owes only the
    /// sync, the read-back and the per-picture releases.
    fn encode_with(
        &mut self,
        reference: Option<usize>,
        anchor: bool,
        stripe: Option<Stripe>,
        dirty: bool,
    ) -> Result<()> {
        let coded = self.acquire_coded()?;
        let is_idr = reference.is_none();
        let slot_count = self.slots.len();
        // HEVC names the kept pictures in every slice header, so a distrusted
        // picture is dropped by both sides now. H.264's long-term indices stay in
        // step by replacement instead. A wave's first frame still predicts from the
        // distrusted picture before it, so that one stays.
        if matches!(self.codec, Codec::Hevc(_)) {
            for (i, slot) in self.slots.iter_mut().enumerate() {
                if slot.is_some_and(|s| !s.trusted) && reference != Some(i) {
                    self.free.push(slot.take().expect("checked").surface);
                }
            }
        }
        let poc = if is_idr { 0 } else { self.poc };
        let slice = PictureSlice {
            is_idr,
            // An IDR restarts frame_num at 0, wherever the count stood.
            frame_num: if is_idr { 0 } else { self.frame_num },
            idr_pic_id: self.idr_pic_id ^ u16::from(is_idr),
            slot: if is_idr {
                0
            } else {
                (self.wire as usize % slot_count) as u8
            },
            max_slots: slot_count as u8,
            reference_slot: reference.map(|s| s as u8),
        };
        let direct = self.direct.take();
        let surface = direct.unwrap_or(self.input[self.next_surface]);
        let recon = self
            .free
            .pop()
            .ok_or_else(|| anyhow!("no free reconstruction surface"))?;
        let sps = self.params.sps();
        let pps = self.params.pps(std::rc::Rc::clone(&sps));

        // SAFETY: `context` and `surface` are live on this display; the call takes
        // both by value and starts a picture that `end_picture` below closes.
        let status =
            unsafe { (self.display.va.begin_picture)(self.display.display, self.context, surface) };
        self.display.va.check("vaBeginPicture", status)?;

        let mut owned: Vec<VaBufferId> = Vec::new();
        let result = match self.codec {
            Codec::H264 => self.render_picture(&mut owned, &sps, &pps, recon, coded, slice, stripe),
            Codec::Hevc(hevc) => {
                self.render_picture_hevc(&mut owned, &hevc, recon, coded, poc, slice, stripe)
            }
        }
        .and_then(|()| self.render_ids(&mut owned.clone()));
        if result.is_err() {
            self.free.push(recon);
        }

        // SAFETY: `context` is live; `end_picture` closes the picture `begin_picture`
        // opened, and must run even if a render failed or the driver keeps the
        // context open forever.
        let end = unsafe { (self.display.va.end_picture)(self.display.display, self.context) };
        for buf in owned {
            // SAFETY: every id here came from `vaCreateBuffer` on this display and
            // is destroyed exactly once; the driver has consumed them by end_picture.
            unsafe { (self.display.va.destroy_buffer)(self.display.display, buf) };
        }
        if let Err(e) = result.and_then(|()| self.display.va.check("vaEndPicture", end)) {
            if !self.free.contains(&recon) {
                self.free.push(recon);
            }
            self.coded_free.push(coded);
            if let Some(direct) = direct {
                self.display.destroy_surface(direct);
            }
            return Err(e);
        }

        // The picture is the long-term reference in its slot from here — the encode
        // may still run, but the queue is in-order so nothing later overtakes it.
        // An IDR also emptied the decoder's DPB: every other slot goes with it.
        if is_idr {
            for slot in self.slots.iter_mut() {
                if let Some(old) = slot.take() {
                    self.free.push(old.surface);
                }
            }
        }
        let held = Slot {
            surface: recon,
            wire: self.wire,
            poc,
            trusted: true,
            dirty,
        };
        if let Some(old) = self.slots[usize::from(slice.slot)].replace(held) {
            self.free.push(old.surface);
        }
        let wire = self.wire;
        self.wire += 1;
        self.poc = poc + 1;
        self.next_surface = (self.next_surface + 1) % Self::SURFACES;
        let max_frame_num = 1u16 << (sps.log2_max_frame_num_minus4 + 4);
        self.frame_num = (slice.frame_num + 1) % max_frame_num;
        self.idr_pic_id = slice.idr_pic_id;
        self.pending.push_back(Pending {
            surface,
            direct,
            coded,
            is_idr,
            anchor,
            wire,
        });
        Ok(())
    }

    /// The next finished picture, in encode order — first anything collected
    /// early, then the pending queue. `block` waits out the driver's surface;
    /// without it a `vaQuerySurfaceStatus` probe decides and `None` means the
    /// oldest is still rendering.
    pub fn collect(&mut self, block: bool) -> Result<Option<EncodedPicture>> {
        if let Some(picture) = self.ready.pop_front() {
            return Ok(Some(picture));
        }
        self.collect_pending(block)
    }

    /// `true` when this picture's coded output is complete. New libva synchronizes the exact
    /// output buffer; older runtimes retain the input-surface compatibility path.
    fn pending_complete(&self, pending: &Pending, block: bool) -> Result<bool> {
        if let Some(sync) = self.display.va.sync_buffer {
            let timeout = if block { VA_TIMEOUT_INFINITE } else { 0 };
            // SAFETY: `coded` is a live output buffer on this display; the call only waits or
            // probes it and retains no pointer.
            let status = unsafe { sync(self.display.display, pending.coded, timeout) };
            if status != VA_STATUS_ERROR_UNIMPLEMENTED {
                if !block && status == VA_STATUS_ERROR_TIMEDOUT {
                    return Ok(false);
                }
                self.display.va.check("vaSyncBuffer", status)?;
                return Ok(true);
            }
        }
        if !block {
            let mut status = 0;
            // SAFETY: `surface` is live on this display and `status` is written through; the
            // result is read only on `VA_STATUS_SUCCESS`.
            let queried = unsafe {
                (self.display.va.query_surface_status)(
                    self.display.display,
                    pending.surface,
                    &mut status,
                )
            };
            self.display.va.check("vaQuerySurfaceStatus", queried)?;
            return Ok(status != VA_SURFACE_RENDERING && status != VA_SURFACE_DISPLAYING);
        }
        // SAFETY: the input surface is live and uniquely names this queued picture on the
        // compatibility path.
        self.display.va.check("vaSyncSurface", unsafe {
            (self.display.va.sync_surface)(self.display.display, pending.surface)
        })?;
        Ok(true)
    }

    /// The pending queue half of [`Self::collect`], with `ready` untouched: the only shape
    /// that guarantees a coded buffer comes back, so [`Self::acquire_coded`] calls it directly.
    fn collect_pending(&mut self, block: bool) -> Result<Option<EncodedPicture>> {
        let Some(pending) = self.pending.front() else {
            return Ok(None);
        };
        if !self.pending_complete(pending, block)? {
            return Ok(None);
        }
        let pending = self.pending.pop_front().expect("front checked above");
        let bytes = self.read_coded(pending.coded);
        self.coded_free.push(pending.coded);
        if let Some(direct) = pending.direct {
            self.display.destroy_surface(direct);
        }
        Ok(Some(EncodedPicture {
            bytes: bytes?,
            is_idr: pending.is_idr,
            recovery_anchor: pending.anchor,
            wire: pending.wire,
        }))
    }

    /// Build and hand over every buffer this picture needs, in the order the
    /// drivers parse them: sequence, packed parameter sets, picture, slice, then
    /// the packed slice header — which Mesa reads against the sequence and picture
    /// state the earlier buffers set.
    #[allow(clippy::too_many_arguments)]
    fn render_picture(
        &self,
        owned: &mut Vec<VaBufferId>,
        sps: &cros_codecs::codec::h264::parser::Sps,
        pps: &cros_codecs::codec::h264::parser::Pps,
        recon: VaSurfaceId,
        coded: VaBufferId,
        slice: PictureSlice,
        stripe: Option<Stripe>,
    ) -> Result<()> {
        let is_idr = slice.is_idr;
        let seq = self.params.va_sequence(sps);
        self.render(owned, vah::VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE, &seq)?;
        let (rc, hrd, frame_rate) = self.params.rate_control();
        self.render_misc(owned, vah::VA_ENC_MISC_PARAMETER_TYPE_RATE_CONTROL, &rc)?;
        self.render_misc(owned, vah::VA_ENC_MISC_PARAMETER_TYPE_HRD, &hrd)?;
        self.render_misc(
            owned,
            vah::VA_ENC_MISC_PARAMETER_TYPE_FRAME_RATE,
            &frame_rate,
        )?;

        if is_idr {
            let (packed_sps, packed_pps) = pf_vaapi::enc_params::packed_parameter_sets(sps, pps);
            let mut sets = packed_sps;
            sets.extend_from_slice(&packed_pps);
            let bits = (sets.len() * 8) as u32;
            self.render_packed(owned, vah::VA_ENC_PACKED_HEADER_TYPE_SEQUENCE, &sets, bits)?;
        }

        let mut pic = vah::VaEncPictureParameterBufferH264 {
            coded_buf: coded,
            frame_num: slice.frame_num,
            pic_init_qp: self.params.initial_qp,
            ..Default::default()
        };
        // For a long-term picture `frame_idx` is its `LongTermFrameIdx` — the slot.
        let long_term = |surface: VaSurfaceId, slot: u8| pf_vaapi::va::VaPictureH264 {
            picture_id: surface,
            frame_idx: u32::from(slot),
            flags: pf_vaapi::va::VA_PICTURE_H264_LONG_TERM_REFERENCE,
            top_field_order_cnt: 0,
            bottom_field_order_cnt: 0,
            va_reserved: [0; 4],
        };
        pic.curr_pic = long_term(recon, slice.slot);
        pic.pic_fields = va_pic_fields(pps, is_idr);
        // The whole DPB, trusted or not: the driver's eviction must match the
        // decoder's, and a distrusted slot is still a picture the decoder holds.
        let mut reference_list_entry = None;
        if !is_idr {
            let live = self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, s)| s.map(|s| (i as u8, s.surface)));
            for (n, (slot, surface)) in live.enumerate() {
                pic.reference_frames[n] = long_term(surface, slot);
                if Some(slot) == slice.reference_slot {
                    reference_list_entry = Some(pic.reference_frames[n]);
                }
            }
        }
        self.render(owned, vah::VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE, &pic)?;

        let mut va_slice = vah::VaEncSliceParameterBufferH264 {
            num_macroblocks: self.params.mbs_per_picture(),
            slice_type: if is_idr { 2 } else { 0 },
            idr_pic_id: slice.idr_pic_id,
            num_ref_idx_active_override_flag: u8::from(!is_idr),
            ..Default::default()
        };
        if let Some(entry) = reference_list_entry {
            va_slice.ref_pic_list_0[0] = entry;
        } else if !is_idr {
            bail!("reference slot {:?} is not held", slice.reference_slot);
        }
        self.render(owned, vah::VA_ENC_SLICE_PARAMETER_BUFFER_TYPE, &va_slice)?;
        // After the slice: iHD parses buffers in order and its slice parser drops a rolling
        // refresh it has not yet seen a P slice for; Mesa stores it whenever it arrives.
        self.render_stripe(owned, stripe)?;

        // Neither driver writes a slice header of its own: radeonsi templates its
        // from this one, iHD copies it.
        let (header, bits) = packed_slice_header(sps, pps, slice);
        self.render_packed(owned, vah::VA_ENC_PACKED_HEADER_TYPE_SLICE, &header, bits)
    }

    /// The HEVC picture: sequence, rate control, the packed VPS/SPS/PPS (and HDR SEI)
    /// on an IDR, then picture, slice and the packed slice header whose reference
    /// picture set is every trusted slot, closest first, the reference marked used.
    #[allow(clippy::too_many_arguments)]
    fn render_picture_hevc(
        &self,
        owned: &mut Vec<VaBufferId>,
        hevc: &HevcParams,
        recon: VaSurfaceId,
        coded: VaBufferId,
        poc: i32,
        slice: PictureSlice,
        stripe: Option<Stripe>,
    ) -> Result<()> {
        let is_idr = slice.is_idr;
        let seq = hevc.va_sequence();
        self.render(owned, vah::VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE, &seq)?;
        let (rc, hrd, frame_rate) = self.params.rate_control();
        self.render_misc(owned, vah::VA_ENC_MISC_PARAMETER_TYPE_RATE_CONTROL, &rc)?;
        self.render_misc(owned, vah::VA_ENC_MISC_PARAMETER_TYPE_HRD, &hrd)?;
        self.render_misc(
            owned,
            vah::VA_ENC_MISC_PARAMETER_TYPE_FRAME_RATE,
            &frame_rate,
        )?;

        if is_idr {
            let mut sets = hevc.vps();
            sets.extend(hevc.sps());
            sets.extend(hevc.pps());
            if let Some(hdr) = &self.hdr {
                sets.extend(hevc.hdr_sei(hdr));
            }
            let bits = (sets.len() * 8) as u32;
            self.render_packed(owned, vah::VA_ENC_PACKED_HEADER_TYPE_SEQUENCE, &sets, bits)?;
        }

        // The DPB the decoder keeps, closest first; only trusted slots survive to
        // here (see `encode_with`).
        let mut kept: Vec<(u8, VaSurfaceId, i32)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.map(|s| (i as u8, s.surface, s.poc)))
            .collect();
        kept.sort_by_key(|&(_, _, p)| std::cmp::Reverse(p));
        if is_idr {
            kept.clear();
        }
        let entry = |surface: VaSurfaceId, poc: i32, used: bool| vahevc::VaPictureHEVC {
            picture_id: surface,
            pic_order_cnt: poc,
            flags: if used {
                vahevc::VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE
            } else {
                0
            },
            va_reserved: [0; 4],
        };
        let mut pic = vahevc::VaEncPictureParameterBufferHEVC {
            decoded_curr_pic: entry(recon, poc, false),
            coded_buf: coded,
            collocated_ref_pic_index: if hevc.features.temporal_mvp { 0 } else { 0xff },
            pic_init_qp: self.params.initial_qp,
            diff_cu_qp_delta_depth: hevc.diff_cu_qp_delta_depth(),
            nal_unit_type: if is_idr {
                vahevc::NAL_IDR_W_RADL
            } else {
                vahevc::NAL_TRAIL_R
            },
            pic_fields: vahevc::pic_fields(is_idr, &hevc.features),
            ..Default::default()
        };
        let mut rps = Vec::with_capacity(kept.len());
        let mut reference_entry = None;
        for (n, &(slot, surface, kept_poc)) in kept.iter().enumerate() {
            let used = Some(slot) == slice.reference_slot;
            pic.reference_frames[n] = entry(surface, kept_poc, used);
            if used {
                reference_entry = Some(pic.reference_frames[n]);
            }
            rps.push((kept_poc, used));
        }
        self.render(owned, vah::VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE, &pic)?;

        let b_slice = !is_idr && hevc.features.gpb;
        let mut va_slice = vahevc::VaEncSliceParameterBufferHEVC {
            num_ctu_in_slice: hevc.ctus_per_picture(),
            slice_type: if is_idr {
                2
            } else if b_slice {
                0
            } else {
                1
            },
            slice_fields: vahevc::slice_fields(is_idr, &hevc.features),
            ..Default::default()
        };
        if let Some(entry) = reference_entry {
            va_slice.ref_pic_list0[0] = entry;
            if b_slice {
                va_slice.ref_pic_list1[0] = entry;
            }
        } else if !is_idr {
            bail!("reference slot {:?} is not held", slice.reference_slot);
        }
        self.render(owned, vah::VA_ENC_SLICE_PARAMETER_BUFFER_TYPE, &va_slice)?;
        // After the slice: iHD parses buffers in order and its slice parser drops a rolling
        // refresh it has not yet seen a P slice for; Mesa stores it whenever it arrives.
        self.render_stripe(owned, stripe)?;

        let header = hevc.slice_header(HevcSlice {
            is_idr,
            poc,
            rps: &rps,
        });
        let bits = (header.len() * 8) as u32;
        self.render_packed(owned, vah::VA_ENC_PACKED_HEADER_TYPE_SLICE, &header, bits)
    }

    /// The wave's intra stripe for this picture, when it carries one. Both drivers
    /// constrain the clean rows' prediction to clean rows themselves.
    fn render_stripe(&self, owned: &mut Vec<VaBufferId>, stripe: Option<Stripe>) -> Result<()> {
        let Some(stripe) = stripe.filter(|s| s.rows > 0) else {
            return Ok(());
        };
        let rir = vah::VaEncMiscParameterRir {
            rir_flags: vah::VA_RIR_ROW,
            intra_insertion_location: stripe.first_row,
            intra_insert_size: stripe.rows,
            ..Default::default()
        };
        self.render_misc(owned, vah::VA_ENC_MISC_PARAMETER_TYPE_RIR, &rir)
    }

    /// A misc buffer is a four-byte type tag with the payload inline after it, in
    /// one allocation.
    fn render_misc<T: Copy>(
        &self,
        owned: &mut Vec<VaBufferId>,
        kind: u32,
        value: &T,
    ) -> Result<()> {
        let mut bytes = Vec::with_capacity(4 + std::mem::size_of::<T>());
        bytes.extend_from_slice(&kind.to_ne_bytes());
        // SAFETY: every `T` here is a `repr(C)` struct without implicit padding (the
        // RIR struct pads itself), so all `size_of::<T>()` bytes are initialised.
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts((value as *const T).cast::<u8>(), std::mem::size_of::<T>())
        });
        let buf = self.create(
            vah::VA_ENC_MISC_PARAMETER_BUFFER_TYPE,
            bytes.len() as u32,
            bytes.as_ptr().cast::<c_void>(),
        )?;
        owned.push(buf);
        Ok(())
    }

    /// One `vaCreateBuffer` + `vaRenderPicture` for a plain parameter struct.
    fn render<T>(&self, owned: &mut Vec<VaBufferId>, kind: u32, value: &T) -> Result<()> {
        let buf = self.create(
            kind,
            std::mem::size_of::<T>() as u32,
            (value as *const T).cast::<c_void>(),
        )?;
        owned.push(buf);
        Ok(())
    }

    /// A packed header is a descriptor and its bytes, and the two must reach the
    /// driver in **one** `vaRenderPicture` call. Rendered as separate calls they are
    /// silently dropped: the picture still encodes, the driver still reports success,
    /// and the header simply is not in the stream.
    ///
    /// `bit_length` is exact, not `bytes.len() * 8`: a slice header ends mid-byte
    /// and the driver writes slice data from the very next bit.
    fn render_packed(
        &self,
        owned: &mut Vec<VaBufferId>,
        kind: u32,
        bytes: &[u8],
        bit_length: u32,
    ) -> Result<()> {
        let desc = vah::VaEncPackedHeaderParameterBuffer {
            kind,
            bit_length,
            // Our synthesizer already inserted emulation prevention; asking the
            // driver to insert it again would corrupt every 00 00 0x sequence.
            has_emulation_bytes: 1,
            va_reserved: [0; 4],
        };
        let desc_buf = self.create(
            vah::VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE,
            std::mem::size_of_val(&desc) as u32,
            (&raw const desc).cast::<c_void>(),
        )?;
        owned.push(desc_buf);
        let data_buf = self.create(
            vah::VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE,
            bytes.len() as u32,
            bytes.as_ptr().cast::<c_void>(),
        )?;
        owned.push(data_buf);
        Ok(())
    }

    /// One `vaCreateBuffer`, returning the id. The caller owns it.
    fn create(&self, kind: u32, size: u32, data: *const c_void) -> Result<VaBufferId> {
        let mut buf = VA_INVALID_ID;
        // SAFETY: `data` points at `size` bytes libva copies out before returning,
        // or is null for an output buffer; `buf` is a local written through.
        let status = unsafe {
            (self.display.va.create_buffer)(
                self.display.display,
                self.context,
                kind,
                size,
                1,
                data.cast_mut(),
                &mut buf,
            )
        };
        self.display
            .va
            .check("vaCreateBuffer", status)
            .with_context(|| format!("buffer type {kind}"))?;
        Ok(buf)
    }

    fn render_ids(&self, ids: &mut [VaBufferId]) -> Result<()> {
        // SAFETY: `ids` is a live array of exactly `ids.len()` buffer ids created on
        // this context; `vaRenderPicture` reads them and does not retain the array.
        let status = unsafe {
            (self.display.va.render_picture)(
                self.display.display,
                self.context,
                ids.as_mut_ptr(),
                ids.len() as c_int,
            )
        };
        self.display.va.check("vaRenderPicture", status)
    }

    /// Copy the picture out of `coded`.
    fn read_coded(&self, coded: VaBufferId) -> Result<Vec<u8>> {
        let mut ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: `coded` is live on this display and `ptr` is written through.
        let status = unsafe { (self.display.va.map_buffer)(self.display.display, coded, &mut ptr) };
        self.display.va.check("vaMapBuffer(coded)", status)?;
        if ptr.is_null() {
            bail!("vaMapBuffer(coded) returned success with a null pointer");
        }

        let collected = (|| -> Result<Vec<u8>> {
            let mut out = Vec::new();
            let mut seg = ptr.cast::<vah::VaCodedBufferSegment>();
            while !seg.is_null() {
                // SAFETY: libva guarantees the mapped coded buffer begins with a
                // `VACodedBufferSegment` and that `next` is either null or another.
                let s = unsafe { *seg };
                if s.buf.is_null() {
                    bail!("coded segment has no data pointer");
                }
                if std::env::var_os("PF_ENC_DEBUG").is_some() {
                    eprintln!(
                        "  segment: size={} bit_offset={} status={:#x}",
                        s.size, s.bit_offset, s.status
                    );
                }
                // SAFETY: `s.buf` points at `s.size` bytes inside the mapping the
                // driver just filled, valid until `vaUnmapBuffer`.
                let bytes =
                    unsafe { std::slice::from_raw_parts(s.buf.cast::<u8>(), s.size as usize) };
                out.extend_from_slice(bytes);
                seg = s.next.cast::<vah::VaCodedBufferSegment>();
            }
            if out.is_empty() {
                bail!("the encoder produced no bytes");
            }
            Ok(out)
        })();

        // SAFETY: the buffer that was mapped, unmapped once, on every path.
        let unmap = unsafe { (self.display.va.unmap_buffer)(self.display.display, coded) };
        let out = collected?;
        self.display.va.check("vaUnmapBuffer(coded)", unmap)?;
        Ok(out)
    }
}

impl Drop for Encoder {
    /// libva's teardown order: buffers, then context, then surfaces, then config.
    /// Pending output is synchronized before the coded buffers are destroyed —
    /// the driver may still be writing one — and each pending direct import goes
    /// with its picture. The display is dropped last, by its own `Drop`.
    fn drop(&mut self) {
        if let Some(vpp) = self.vpp.take() {
            vpp.destroy(&self.display);
        }
        self.clear_direct();
        if let Some((staging, _)) = self.staging.take() {
            self.display.destroy_surface(staging);
        }
        while let Some(pending) = self.pending.pop_front() {
            // Best effort: teardown has no error path, but output storage stays live until this
            // exact picture has been given a completion opportunity.
            let _ = self.pending_complete(&pending, true);
            if let Some(direct) = pending.direct {
                self.display.destroy_surface(direct);
            }
        }
        // SAFETY: every id was created on this display and is destroyed once.
        unsafe {
            for &coded in &self.coded {
                (self.display.va.destroy_buffer)(self.display.display, coded);
            }
            (self.display.va.destroy_context)(self.display.display, self.context);
            (self.display.va.destroy_surfaces)(
                self.display.display,
                self.input.as_mut_ptr(),
                self.input.len() as c_int,
            );
            (self.display.va.destroy_surfaces)(
                self.display.display,
                self.recon.as_mut_ptr(),
                self.recon.len() as c_int,
            );
            (self.display.va.destroy_config)(self.display.display, self.config);
        }
    }
}

/// Open a display and an encoder on it, for callers with no display of their own.
pub fn open(params: SessionParams, codec: CodecParams) -> Result<Encoder> {
    let va = crate::Libva::load().context("libva")?;
    let display = Display::open(va).context("no VAAPI display")?;
    Encoder::new(display, params, codec).map_err(|e| anyhow!("{e:#}"))
}

/// A dmabuf already in the session's format and size needs no VPP pass. A crop (a
/// mirrored head) still needs one: the encoder reads the whole surface.
fn direct_ingest(
    rt_format: u32,
    source: (u32, u32),
    session: (u32, u32),
    ten_bit: bool,
    cropped: bool,
) -> bool {
    let session_rt = if ten_bit {
        VA_RT_FORMAT_YUV420_10
    } else {
        VA_RT_FORMAT_YUV420
    };
    rt_format == session_rt && source == session && !cropped
}

#[cfg(test)]
mod direct_ingest_tests {
    use super::*;

    #[test]
    fn only_a_matching_yuv_source_skips_the_vpp_pass() {
        let s = (1920, 1080);
        assert!(direct_ingest(VA_RT_FORMAT_YUV420, s, s, false, false));
        assert!(direct_ingest(VA_RT_FORMAT_YUV420_10, s, s, true, false));
        // Depth, size, RGB and a crop each keep the conversion.
        assert!(!direct_ingest(VA_RT_FORMAT_YUV420, s, s, true, false));
        assert!(!direct_ingest(
            VA_RT_FORMAT_YUV420,
            (3840, 2160),
            s,
            false,
            false
        ));
        assert!(!direct_ingest(VA_RT_FORMAT_RGB32, s, s, false, false));
        assert!(!direct_ingest(VA_RT_FORMAT_YUV420, s, s, false, true));
    }
}
