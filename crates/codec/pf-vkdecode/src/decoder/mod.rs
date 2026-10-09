//! What every Vulkan Video decoder hands out: [`DecodedVkFrame`], its
//! [`DecodeStatus`], and [`VkDecodeError`]. The codec decoders live in
//! [`crate::decoder_h264`], [`crate::decoder_h265`] and [`crate::decoder_av1`],
//! over the machinery in `core`.
//!
//! Decode targets come from a picture pool decoupled from DPB slots
//! ([`crate::images`]). A slot binds a free picture at activation so a delivered
//! picture is never a decode target while the consumer reads it. The decoder
//! signals `value+1` at write; the presenter waits, samples, restores layout,
//! and signals `value+1` again; `release_frame` reports that write-back before
//! the picture's next use.

use ash::vk;
use pf_bitstream::h264::ColourDescription;
use pf_bitstream::h264::DisplayCrop;
use pf_bitstream::h264::PlanError;

use crate::caps::CapsError;
use crate::device::AllocError;
use crate::device::DeviceError;
use crate::images::HOLD_HEADROOM;
use crate::params::ParamsError;
use crate::params_av1::ParamsAv1Error;
use crate::params_h265::H265ParamsError;
use crate::pic::PlanToVkError;
use crate::pic_av1::PlanToVkAv1Error;
use crate::pic_h265::PlanToVkH265Error;
use crate::session::SessionError;

pub(crate) mod core;

/// 5 s: longer than a real decode, finite against a wedged driver. Matches the
/// encoder fence budget so session recovery is never parked forever.
const DECODE_TIMEOUT_NS: u64 = 5_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeStatus {
    Pending,
    Ok,
    /// Error, recycled query, or device lost: content is unproven; conceal
    /// (`want_keyframe`).
    Failed,
}

/// Display-ready pool picture; the decoder does not touch it until
/// `release_frame`.
///
/// Pixels ready when `semaphore` reaches [`Self::value`]. A sampler must, in
/// the same submission that waits `value`, signal `value + 1` after reads and
/// layout restore, then `release_frame(frame, true)`. Drop unsampled with
/// `false`. Release every frame once, including stale-generation ones (graveyard).
#[derive(Debug, Clone)]
pub struct DecodedVkFrame {
    pub image: vk::Image,
    /// Caps-resolved `output_format` of the session that decoded this picture;
    /// [`Self::view`] aliases it. Do not assume 8-bit 4:2:0: H.265 Main 10 is
    /// [`crate::P010`], RExt 4:4:4 is [`crate::YUV444_8`] / [`crate::YUV444_10`],
    /// and a renegotiation can change format mid-stream. [`crate::plane_formats`]
    /// maps this to [`Self::plane_views`].
    pub format: vk::Format,
    pub view: vk::ImageView,
    /// Presenter sampler views; formats from [`crate::plane_formats`] on
    /// [`Self::format`].
    pub plane_views: [vk::ImageView; 2],
    /// Array layer this picture occupies; 0 when its backing image is private.
    pub layer: u32,
    /// Layout at the semaphore signal, and the layout the consumer must restore
    /// after sampling: `VIDEO_DECODE_DPB_KHR` (coincide) or `VIDEO_DECODE_DST_KHR`.
    pub layout: vk::ImageLayout,
    /// Allocated extent (`pictureAccessGranularity`-aligned). UV-scale math
    /// divides by this (1088-row class); display region is [`Self::crop`].
    pub coded_width: u32,
    pub coded_height: u32,
    pub crop: DisplayCrop,
    /// Active-SPS colour; per frame like [`Self::crop`]. A new SPS can switch
    /// HDR mid-stream. Unspecified VUI is inferred by pf-bitstream.
    pub colour: ColourDescription,
    pub semaphore: vk::Semaphore,
    pub value: u64,
    pub poc: i32,
    pub is_idr: bool,
    /// Recovery-point SEI for this AU (and any outstanding one before it); see
    /// [`crate::recovery`]. `NONE` when the stream carries none.
    ///
    /// [`Self::is_idr`] cannot answer for intra-refresh: the wave never emits an
    /// IDR, so a consumer freezing on loss has no decoder-visible clean point.
    pub recovery: crate::recovery::RecoveryMark,
    /// Decode-order ordinal stamped at plan time (1 for the first picture).
    /// Survives session rebuilds: it describes the stream, not Vulkan objects.
    /// Delivery order is not decode order; compare against the ordinal current
    /// at freeze-arm so a flushed pre-loss picture cannot lift the freeze.
    pub decode_order: u64,
    /// Whole prediction chain was fully available
    /// ([`pf_bitstream::h264::PicturePlan::references_clean`]). `true` for
    /// IDR/IRAP/key and any picture whose chain is clean; `false` once this AU
    /// or an ancestor needed concealment.
    ///
    /// Corroborates a host `USER_FLAG_RECOVERY_ANCHOR`: the host infers
    /// known-good from what the client received; this is what actually decoded.
    /// Ignore if you have no such claim to check.
    pub references_clean: bool,
    pub query_slot: u32,
    /// Submission ordinal: the query slot is stale if re-armed since.
    pub submission: u64,
    pub picture: u32,
    /// Session generation; `release_frame` routes by this (current vs graveyard).
    pub generation: u64,
    /// The picture carries TRANSFER_SRC, so a consumer may copy it out
    /// ([`crate::VkDecoder::copy_out`]).
    pub copyable: bool,
}

/// Decode failure. Never panics; device loss is first-class so the session
/// layer can tear down and rebuild.
#[derive(Debug)]
pub enum VkDecodeError {
    Plan(PlanError),
    /// H.265 plan failure. `h265::PlanError::RaslSkipped` is not this: the
    /// decoder answers `Ok(None)` for a RASL after an open-GOP join.
    PlanH265(pf_bitstream::h265::PlanError),
    /// Parameter set has no Std representation (stream-integrity failure).
    Params(ParamsError),
    /// H.265 parameter set has no Std representation, or the stream sits
    /// outside the envelope (chroma / bit depth / profile). Refused rather
    /// than half-converted.
    ParamsH265(H265ParamsError),
    PlanAv1(pf_bitstream::av1::PlanError),
    ConvertAv1(PlanToVkAv1Error),
    /// AV1 sequence header has no Std representation, or the stream sits
    /// outside the envelope (sampling / bit depth / profile).
    ParamsAv1(ParamsAv1Error),
    /// Tile groups could not be split into `pTileOffsets` ranges. Refused
    /// rather than submitting the whole OBU as tiles ([`crate::decoder_av1`]).
    TilesAv1(pf_bitstream::av1::tiles::Av1TileError),
    /// Named a reference slot the planner no longer holds. Fatal: the planner
    /// compacting survivors into `AuPlan::refs` would make later names resolve
    /// to the wrong picture. `ref_index` is LAST_FRAME=0 … ALTREF_FRAME=6.
    MissingReferenceAv1 {
        slot: u8,
        ref_index: u8,
    },
    /// Every frame of this temporal unit was skipped while waiting for a key
    /// after a failure. Same kind as [`pf_bitstream::h264::PlanError::AwaitingIdr`]:
    /// an error per AU, so the consumer's demotion streak can fire. `Ok(None)`
    /// would reset that streak once per inter frame and never demote.
    AwaitingKeyAv1,
    /// Plan-to-Vulkan conversion. `CapacityMismatch` is consumed by rebuild and
    /// surfaces only if the rebuilt session still mismatches.
    Convert(PlanToVkError),
    ConvertH265(PlanToVkH265Error),
    /// Device cannot host any session (demote to the next rung).
    Caps(CapsError),
    Device(DeviceError),
    Unsupported(String),
    /// Vulkan call failed (anything but device loss).
    Vk(vk::Result),
    /// `VK_ERROR_DEVICE_LOST`. Later calls fail fast until the owner rebuilds.
    DeviceLost,
    /// Bounded GPU wait expired; fatal for this decoder instance.
    Timeout(&'static str),
    /// Picture pool exhausted: consumer holds more than [`HOLD_HEADROOM`]
    /// unreleased frames while the DPB is full. AU planned but not decoded;
    /// release frames and request a keyframe.
    NoFreeSlot,
    /// A referenced DPB slot binds no image. Fatal on all three codecs.
    ///
    /// H.265/AV1 name slots by index, so dropping one silently re-points later
    /// names. H.264 has no such arrays but hardware still decodes against the
    /// unbound slot. Safe only because the decoder's recovery latch flushes to
    /// the next IRAP/IDR.
    UnboundReferenceSlot {
        slot: u8,
    },
    /// Frame's generation has no retired pool (double release, or outlived the
    /// graveyard entry).
    StaleFrame {
        frame_generation: u64,
        current_generation: u64,
    },
    NoMemoryType {
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    },
}

impl std::fmt::Display for VkDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VkDecodeError::Plan(e) => write!(f, "AU planning failed: {e}"),
            VkDecodeError::PlanH265(e) => write!(f, "H.265 AU planning failed: {e}"),
            VkDecodeError::Params(e) => write!(f, "parameter-set conversion failed: {e}"),
            VkDecodeError::ParamsH265(e) => {
                write!(f, "H.265 parameter-set conversion failed: {e}")
            }
            VkDecodeError::PlanAv1(e) => write!(f, "AV1 AU planning failed: {e}"),
            VkDecodeError::ConvertAv1(e) => write!(f, "AV1 plan conversion failed: {e}"),
            VkDecodeError::ParamsAv1(e) => {
                write!(f, "AV1 sequence-header conversion failed: {e}")
            }
            VkDecodeError::TilesAv1(e) => write!(f, "AV1 tile split failed: {e}"),
            VkDecodeError::MissingReferenceAv1 { slot, ref_index } => {
                write!(
                    f,
                    "AV1 reference name {ref_index} points at slot {slot}, which holds \
                     no picture — the surviving references would renumber"
                )
            }
            VkDecodeError::AwaitingKeyAv1 => write!(
                f,
                "every frame of this AV1 temporal unit was skipped — the decoder is \
                 waiting for the next key frame after a failure"
            ),
            VkDecodeError::Convert(e) => write!(f, "plan conversion failed: {e}"),
            VkDecodeError::ConvertH265(e) => write!(f, "H.265 plan conversion failed: {e}"),
            VkDecodeError::Caps(e) => write!(f, "decode capabilities unusable: {e}"),
            VkDecodeError::Device(e) => write!(f, "device handles rejected: {e}"),
            VkDecodeError::Unsupported(what) => write!(f, "outside device caps: {what}"),
            VkDecodeError::Vk(r) => write!(f, "Vulkan call failed: {r:?}"),
            VkDecodeError::DeviceLost => write!(f, "GPU device lost (VK_ERROR_DEVICE_LOST)"),
            VkDecodeError::Timeout(what) => {
                write!(f, "GPU wait expired after {DECODE_TIMEOUT_NS} ns: {what}")
            }
            VkDecodeError::NoFreeSlot => {
                write!(
                    f,
                    "picture pool exhausted — more than {HOLD_HEADROOM} delivered frames \
                     are unreleased (release_frame owed)"
                )
            }
            VkDecodeError::UnboundReferenceSlot { slot } => {
                write!(
                    f,
                    "DPB slot {slot} is referenced by this AU but binds no image — \
                     the H.265 RPS index arrays would point at the wrong pictures"
                )
            }
            VkDecodeError::StaleFrame {
                frame_generation,
                current_generation,
            } => {
                write!(
                    f,
                    "frame from session generation {frame_generation} (current \
                     {current_generation}) has no retired pool — double release?"
                )
            }
            VkDecodeError::NoMemoryType { type_bits, flags } => {
                write!(
                    f,
                    "no memory type satisfies bits {type_bits:#x} with {flags:?}"
                )
            }
        }
    }
}

impl VkDecodeError {
    /// The device cannot host this codec at all: every AU refuses the same way, so a
    /// failure streak only delays the rung below.
    pub fn is_device_fact(&self) -> bool {
        matches!(self, VkDecodeError::Caps(_))
    }

    /// Nothing was fed: the planner waits for an IDR, or for the parameter sets a
    /// decoder built mid-GOP has not seen. Idle, not a refusal. H.26x reads the
    /// planner's own rule; AV1's wait is this decoder's.
    pub fn awaits_idr(&self) -> bool {
        match self {
            VkDecodeError::Plan(e) => e.awaits_idr(),
            VkDecodeError::PlanH265(e) => e.awaits_idr(),
            VkDecodeError::AwaitingKeyAv1 => true,
            _ => false,
        }
    }
}

impl std::error::Error for VkDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            VkDecodeError::Plan(e) => Some(e),
            VkDecodeError::PlanH265(e) => Some(e),
            VkDecodeError::Params(e) => Some(e),
            VkDecodeError::ParamsH265(e) => Some(e),
            VkDecodeError::Convert(e) => Some(e),
            VkDecodeError::ConvertH265(e) => Some(e),
            VkDecodeError::PlanAv1(e) => Some(e),
            VkDecodeError::ConvertAv1(e) => Some(e),
            VkDecodeError::ParamsAv1(e) => Some(e),
            VkDecodeError::TilesAv1(e) => Some(e),
            VkDecodeError::Caps(e) => Some(e),
            VkDecodeError::Device(e) => Some(e),
            _ => None,
        }
    }
}

impl From<vk::Result> for VkDecodeError {
    fn from(r: vk::Result) -> Self {
        if r == vk::Result::ERROR_DEVICE_LOST {
            VkDecodeError::DeviceLost
        } else {
            VkDecodeError::Vk(r)
        }
    }
}

impl From<PlanError> for VkDecodeError {
    fn from(e: PlanError) -> Self {
        VkDecodeError::Plan(e)
    }
}

impl From<ParamsError> for VkDecodeError {
    fn from(e: ParamsError) -> Self {
        VkDecodeError::Params(e)
    }
}

impl From<H265ParamsError> for VkDecodeError {
    fn from(e: H265ParamsError) -> Self {
        VkDecodeError::ParamsH265(e)
    }
}

impl From<ParamsAv1Error> for VkDecodeError {
    fn from(e: ParamsAv1Error) -> Self {
        VkDecodeError::ParamsAv1(e)
    }
}

impl From<PlanToVkAv1Error> for VkDecodeError {
    fn from(e: PlanToVkAv1Error) -> Self {
        VkDecodeError::ConvertAv1(e)
    }
}

impl From<CapsError> for VkDecodeError {
    fn from(e: CapsError) -> Self {
        VkDecodeError::Caps(e)
    }
}

impl From<DeviceError> for VkDecodeError {
    fn from(e: DeviceError) -> Self {
        VkDecodeError::Device(e)
    }
}

impl From<SessionError> for VkDecodeError {
    fn from(e: SessionError) -> Self {
        match e {
            SessionError::Vk(r) => VkDecodeError::from(r),
            SessionError::Params(p) => VkDecodeError::Params(p),
            SessionError::ParamsH265(p) => VkDecodeError::ParamsH265(p),
            SessionError::ParamsAv1(p) => VkDecodeError::ParamsAv1(p),
            SessionError::NoMemoryType { type_bits, flags } => {
                VkDecodeError::NoMemoryType { type_bits, flags }
            }
        }
    }
}

impl From<AllocError> for VkDecodeError {
    fn from(e: AllocError) -> Self {
        match e {
            AllocError::Vk(r) => VkDecodeError::from(r),
            AllocError::NoMemoryType { type_bits, flags } => {
                VkDecodeError::NoMemoryType { type_bits, flags }
            }
        }
    }
}
