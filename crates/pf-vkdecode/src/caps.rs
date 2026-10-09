//! Decode capability query and derivation for all three codecs: picture-format
//! vocabulary, coincide/distinct/layered arrangement, and the profile chain every
//! Vulkan object of a session is created against.
//!
//! [`query_caps`] is the only function that talks to the driver
//! (`vkGetPhysicalDeviceVideoCapabilitiesKHR` and the three video-format enumerations)
//! and copies facts into [`RawCaps`]. [`derive_caps`] is pure over that
//! hand-buildable struct into [`DecodeCaps`]. Mode and format decisions are
//! unit-tested without a GPU. The codec modules add only profile keys and chains.

use ash::vk;
use ash::vk::native as hh;

use crate::caps_av1::Av1ProfileChain;
use crate::caps_av1::Av1ProfileKey;
use crate::caps_h265::H265ProfileChain;
use crate::caps_h265::H265ProfileKey;
use crate::device::DecodeDevice;

/// 8-bit 4:2:0 semi-planar. Every H.264 session here, and H.265 Main via
/// [`crate::caps_h265::output_format_for`].
pub const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;
/// 10-bit 4:2:0 (P010). H.265 Main 10's picture format.
pub const P010: vk::Format = vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16;
/// 8-bit 4:4:4 two-plane. H.265 RExt 4:4:4 8-bit, when the device advertises it.
pub const YUV444_8: vk::Format = vk::Format::G8_B8R8_2PLANE_444_UNORM;
/// 10-bit 4:4:4 two-plane. H.265 RExt 4:4:4 10-bit, when the device advertises it.
pub const YUV444_10: vk::Format = vk::Format::G10X6_B10X6R10X6_2PLANE_444_UNORM_3PACK16;

/// Every picture format a session can deliver. Consumers pin per-format tables
/// against this list rather than a hand copy: a fifth format here (12-bit RExt)
/// must fail the consumer's test instead of falling through. [`plane_formats`]
/// and [`crate::caps_h265::output_format_for`] are tested to agree with it.
pub const OUTPUT_FORMATS: [vk::Format; 4] = [NV12, P010, YUV444_8, YUV444_10];

/// Per-plane `R*`/`R*G*` view formats the presenter samples, or `None` if this
/// crate has no mapping. Views exist only under `MUTABLE_FORMAT` and must match
/// spec table 49.1: 8-bit two-plane → `R8`/`R8G8`, 10-bit `3PACK16` → `R10X6`/
/// `R10X6G10X6`. An `R8` view on a 10-bit plane silently reads half of each sample.
///
/// Comparisons, not `match`: `vk::Format` is an ash newtype over `i32` whose
/// field is private, so the constants are not structural patterns.
pub fn plane_formats(format: vk::Format) -> Option<[vk::Format; 2]> {
    if format == NV12 || format == YUV444_8 {
        Some([vk::Format::R8_UNORM, vk::Format::R8G8_UNORM])
    } else if format == P010 || format == YUV444_10 {
        Some([
            vk::Format::R10X6_UNORM_PACK16,
            vk::Format::R10X6G10X6_UNORM_2PACK16,
        ])
    } else {
        None
    }
}

/// Distinct-mode DPB: reference-only. Format queries use this exact usage;
/// a weaker query would validate an image the pool never creates.
pub const DPB_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR;
/// Distinct-mode output: decode destination plus presenter sampling.
pub const OUTPUT_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR.as_raw() | vk::ImageUsageFlags::SAMPLED.as_raw(),
);
/// Coincide-mode: DPB + decode destination + presenter sampling on one image.
pub const COINCIDE_USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR.as_raw()
        | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR.as_raw()
        | vk::ImageUsageFlags::SAMPLED.as_raw(),
);

/// One `VkVideoFormatPropertiesKHR` entry as this crate consumes it. Creation
/// must stay inside the advertised usage and create-flag envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoFormat {
    pub format: vk::Format,
    /// Driver `imageUsageFlags` under the queried profile (a superset of the query
    /// usage on a conformant driver).
    pub image_usage: vk::ImageUsageFlags,
    /// Driver `imageCreateFlags`. Per-plane views need `MUTABLE_FORMAT` here.
    ///
    /// This is also the only gate on `VK_IMAGE_CREATE_EXTENDED_USAGE_BIT`, the spec
    /// escape from `image_usage`. VUID-VkImageCreateInfo-pNext-06811 admits a usage
    /// bit outside `image_usage` only when create flags include `EXTENDED_USAGE`,
    /// and admits that flag only when it is also in this field (or is
    /// `VIDEO_PROFILE_INDEPENDENT`, which needs `VK_KHR_video_maintenance1`).
    /// Empty here closes that hatch as well as `MUTABLE_FORMAT`.
    pub image_create_flags: vk::ImageCreateFlags,
    /// `imageType`. VUID-06811 compares it for equality, so it is recorded rather
    /// than assumed. [`crate::images`] creates `TYPE_2D`.
    pub image_type: vk::ImageType,
    /// `imageTiling`. Likewise equality-compared by VUID-06811; pools create `OPTIMAL`.
    pub image_tiling: vk::ImageTiling,
    /// Usage the query that returned this entry asked for. Empty in a hand-built entry.
    pub queried: vk::ImageUsageFlags,
}

impl VideoFormat {
    /// The driver answered with a mask that lacks the usage it was asked about: one
    /// fixed report for every query. Intel's Windows driver does this, and its decode
    /// images read as zeros when sampled, so the mask is the truth about `SAMPLED`.
    pub fn echoed(&self) -> bool {
        !self.image_usage.contains(self.queried)
    }
}

impl Default for VideoFormat {
    /// `TYPE_2D` + `OPTIMAL`, matching the pools, so a fixture that names only the
    /// interesting fields still describes a creatable image.
    fn default() -> Self {
        Self {
            format: vk::Format::UNDEFINED,
            image_usage: vk::ImageUsageFlags::empty(),
            image_create_flags: vk::ImageCreateFlags::empty(),
            image_type: vk::ImageType::TYPE_2D,
            image_tiling: vk::ImageTiling::OPTIMAL,
            queried: vk::ImageUsageFlags::empty(),
        }
    }
}

/// Everything the thin query copies out of the driver, hand-buildable for tests.
///
/// The three format lists match the three usages the pools create with
/// ([`DPB_USAGE`], [`OUTPUT_USAGE`], [`COINCIDE_USAGE`]). A usage the driver
/// rejects yields an empty list: the query maps `VK_ERROR_FORMAT_NOT_SUPPORTED`
/// and `VK_ERROR_IMAGE_USAGE_NOT_SUPPORTED` for that usage to empty rather than
/// failing the whole probe.
#[derive(Debug, Clone)]
pub struct RawCaps {
    pub capability_flags: vk::VideoCapabilityFlagsKHR,
    /// Coincide/distinct advertisement (`VkVideoDecodeCapabilitiesKHR::flags`).
    pub decode_flags: vk::VideoDecodeCapabilityFlagsKHR,
    pub min_bitstream_buffer_offset_alignment: u64,
    pub min_bitstream_buffer_size_alignment: u64,
    pub picture_access_granularity: vk::Extent2D,
    pub min_coded_extent: vk::Extent2D,
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_reference_pictures: u32,
    /// The profile's codec `maxLevelIdc` (AV1: `maxLevel`), tagged with its codec.
    pub max_level: MaxLevelIdc,
    /// Session creation echoes `VkVideoCapabilitiesKHR::stdHeaderVersion` back.
    pub std_header_version: vk::ExtensionProperties,
    /// Distinct-mode DPB formats (queried with [`DPB_USAGE`]).
    pub dpb_formats: Vec<VideoFormat>,
    /// Distinct-mode output formats (queried with [`OUTPUT_USAGE`]).
    pub output_formats: Vec<VideoFormat>,
    /// Coincide-mode formats (queried with [`COINCIDE_USAGE`]).
    pub coincide_formats: Vec<VideoFormat>,
}

/// A device's decode level ceiling, tagged with the codec whose Std code space
/// it is stated in.
///
/// `StdVideoH264LevelIdc`, `StdVideoH265LevelIdc` and `StdVideoAV1Level` are all
/// `c_uint` aliases. Assigning one where another belongs compiles, and the
/// numbers look plausible (H.264 4.1 ≠ H.265 4.1; AV1 5.1 is 13, H.265 5.1 is 12).
/// The gate is a numeric compare against [`Self::code_point`] and is only sound
/// within one codec, which is why the value carries its tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxLevelIdc {
    H264(hh::StdVideoH264LevelIdc),
    H265(hh::StdVideoH265LevelIdc),
    /// `VkVideoDecodeAV1CapabilitiesKHR::maxLevel`. Unlike H.264/H.265 this is the
    /// bitstream's own index (`seq_level_idx`: 2.0 = 0 … 7.3 = 23).
    ///
    /// Valid only over 0…23. `seq_level_idx` is 5 bits; 31 is Annex A's "maximum
    /// parameters" sentinel and outranks even a device at the enum's top. The AV1
    /// gate therefore treats a stream above this ceiling as advisory
    /// (`VkDecoder::ensure_state`).
    Av1(hh::StdVideoAV1Level),
}

impl MaxLevelIdc {
    /// Raw Std code point for the level gate. Compare only against the same codec
    /// (the variant says which).
    pub fn code_point(self) -> u32 {
        match self {
            MaxLevelIdc::H264(level) | MaxLevelIdc::H265(level) | MaxLevelIdc::Av1(level) => level,
        }
    }
}

impl std::fmt::Display for MaxLevelIdc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MaxLevelIdc::H264(level) => write!(f, "H.264 Std level {level}"),
            MaxLevelIdc::H265(level) => write!(f, "H.265 Std level {level}"),
            MaxLevelIdc::Av1(level) => write!(f, "AV1 Std level {level}"),
        }
    }
}

/// Session-shaping facts for one profile. Rebuilt only when the stream
/// renegotiates to a different profile.
#[derive(Debug, Clone)]
pub struct DecodeCaps {
    /// `true`: decode output is the DPB image. `false`: separate DPB array and
    /// output images. When the driver advertises both, a usable coincide wins
    /// (half the images).
    pub coincide: bool,
    /// `true` when the driver does not advertise `SEPARATE_REFERENCE_IMAGES`: every
    /// DPB slot must be a layer of one image array. Distinct mode uses a
    /// reference-only array; coincide mode makes every picture one layer.
    pub layered_dpb: bool,
    /// Bitstream buffer alignments, floored at 1 so ring math never divides by
    /// a zero an uninitialized fixture would carry.
    pub min_bitstream_offset_alignment: u64,
    pub min_bitstream_size_alignment: u64,
    pub picture_access_granularity: vk::Extent2D,
    pub min_coded_extent: vk::Extent2D,
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_references: u32,
    pub max_level_idc: MaxLevelIdc,
    /// DPB image format (`== output_format` in coincide mode).
    pub dpb_format: vk::Format,
    pub output_format: vk::Format,
    /// Per-plane views of [`Self::output_format`], resolved here so the pool never
    /// re-derives (or guesses) them.
    pub plane_view_formats: [vk::Format; 2],
    pub std_header_version: vk::ExtensionProperties,
}

impl DecodeCaps {
    /// `coded` rounded up to `pictureAccessGranularity` — the extent pool images
    /// are created at. Per-picture `codedExtent` stays the stream size. A zero
    /// granularity axis (uninitialized fixture) degrades to 1.
    pub fn aligned_extent(&self, coded: vk::Extent2D) -> vk::Extent2D {
        let round = |value: u32, granularity: u32| -> u32 {
            let granularity = granularity.max(1);
            value.div_ceil(granularity) * granularity
        };
        vk::Extent2D {
            width: round(coded.width, self.picture_access_granularity.width),
            height: round(coded.height, self.picture_access_granularity.height),
        }
    }
}

/// Raw caps that do not add up to a usable decoder. Device gaps the caller
/// demotes on, not stream conditions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapsError {
    /// Neither COINCIDE nor DISTINCT advertised — no DPB arrangement (spec
    /// requires at least one; a broken driver).
    NoDecodeMode,
    /// The mode's format list does not contain the picture format the stream
    /// needs. `wanted` is [`NV12`] for H.264/H.265 Main, [`P010`] for Main 10,
    /// the 4:4:4 pair for RExt. Refused before any session exists.
    NoFormat {
        mode: &'static str,
        wanted: vk::Format,
    },
    /// No per-plane mapping in [`plane_formats`]. Unreachable for the four
    /// envelope formats; guards a future format arriving without its views.
    NoPlaneMapping { format: vk::Format },
    /// The wanted format's entry in `mode` is missing a usage bit the pool would
    /// create with. Creating anyway would be a silent VUID violation.
    UsageUnsupported {
        mode: &'static str,
        /// Picture format whose entry fell short — not always NV12 (Main 10 is P010).
        format: vk::Format,
        missing: vk::ImageUsageFlags,
    },
    /// Presenter-facing entry for `mode` does not allow `MUTABLE_FORMAT`, so the
    /// per-plane views [`plane_formats`] cannot exist on this device.
    NoMutableFormat {
        mode: &'static str,
        format: vk::Format,
    },
    /// The picture has more slice segments than the owner allows this driver
    /// ([`crate::VkH265Decoder::refuse_multi_slice`]). Refused before any driver call.
    SliceSegments { segments: usize },
    /// The driver answered the `mode` query with a mask that lacks the queried usage
    /// ([`VideoFormat::echoed`]). Its decode images cannot be sampled.
    EchoedReport {
        mode: &'static str,
        format: vk::Format,
    },
}

/// One slice segment per picture when `single_slice` is set, any number otherwise.
pub(crate) fn require_segments(single_slice: bool, segments: usize) -> Result<(), CapsError> {
    if single_slice && segments > 1 {
        Err(CapsError::SliceSegments { segments })
    } else {
        Ok(())
    }
}

impl std::fmt::Display for CapsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CapsError::NoDecodeMode => {
                write!(
                    f,
                    "driver advertises neither DPB_AND_OUTPUT_COINCIDE nor DISTINCT"
                )
            }
            CapsError::NoFormat { mode, wanted } => {
                write!(
                    f,
                    "no {wanted:?} in the {mode} video format properties for this profile"
                )
            }
            CapsError::NoPlaneMapping { format } => {
                write!(f, "no per-plane view mapping for {format:?}")
            }
            CapsError::UsageUnsupported {
                mode,
                format,
                missing,
            } => {
                write!(
                    f,
                    "the {mode} {format:?} entry does not advertise usage {missing:?}"
                )?;
                // Without `SAMPLED` no shader can read the picture, so this rung
                // cannot exist here. `--probe-decode` prints the driver's envelope.
                if missing.contains(vk::ImageUsageFlags::SAMPLED) {
                    write!(
                        f,
                        " — no shader can read this device's decoded pictures, so the \
                         zero-copy path cannot exist on it (see --probe-decode)"
                    )?;
                }
                Ok(())
            }
            CapsError::NoMutableFormat { mode, format } => {
                write!(
                    f,
                    "the {mode} {format:?} entry does not allow MUTABLE_FORMAT (per-plane views)"
                )
            }
            CapsError::SliceSegments { segments } => {
                write!(
                    f,
                    "this driver takes one slice segment per picture, the stream sends {segments}"
                )
            }
            CapsError::EchoedReport { mode, format } => {
                write!(
                    f,
                    "the {mode} {format:?} report echoes the query; decode images here \
                     read as zeros when sampled, a copy stage is needed"
                )
            }
        }
    }
}

impl std::error::Error for CapsError {}

/// Derive session-shaping facts from one raw query, for a stream whose picture
/// format is `wanted`: [`NV12`] for H.264, the profile key's format for H.265
/// and AV1. Pure; arrangement lives in `derive_arrangement`.
///
/// Missing `wanted` under the advertised mode is [`CapsError::NoFormat`] with
/// mode and format named. There is no fallback to a shallower format that would
/// lose bits.
pub fn derive_caps(raw: &RawCaps, wanted: vk::Format) -> Result<DecodeCaps, CapsError> {
    let arrangement = derive_arrangement(
        raw.capability_flags,
        raw.decode_flags,
        wanted,
        &raw.dpb_formats,
        &raw.output_formats,
        &raw.coincide_formats,
    )?;
    Ok(DecodeCaps {
        coincide: arrangement.coincide,
        layered_dpb: arrangement.layered_dpb,
        min_bitstream_offset_alignment: raw.min_bitstream_buffer_offset_alignment.max(1),
        min_bitstream_size_alignment: raw.min_bitstream_buffer_size_alignment.max(1),
        picture_access_granularity: raw.picture_access_granularity,
        min_coded_extent: raw.min_coded_extent,
        max_coded_extent: raw.max_coded_extent,
        max_dpb_slots: raw.max_dpb_slots,
        max_active_references: raw.max_active_reference_pictures,
        max_level_idc: raw.max_level,
        dpb_format: arrangement.dpb_format,
        output_format: arrangement.output_format,
        plane_view_formats: arrangement.plane_view_formats,
        std_header_version: raw.std_header_version,
    })
}

/// Codec-agnostic half of derivation: DPB arrangement and which format lists
/// satisfy picture format `wanted`.
pub(crate) struct Arrangement {
    coincide: bool,
    layered_dpb: bool,
    dpb_format: vk::Format,
    output_format: vk::Format,
    plane_view_formats: [vk::Format; 2],
}

/// Decide the DPB arrangement and validate `wanted` against the format lists of
/// the roles that arrangement creates. H.265 and AV1 derive `wanted` from the
/// stream's chroma and bit depth ([`crate::caps_h265::output_format_for`]).
pub(crate) fn derive_arrangement(
    capability_flags: vk::VideoCapabilityFlagsKHR,
    decode_flags: vk::VideoDecodeCapabilityFlagsKHR,
    wanted: vk::Format,
    dpb_formats: &[VideoFormat],
    output_formats: &[VideoFormat],
    coincide_formats: &[VideoFormat],
) -> Result<Arrangement, CapsError> {
    let coincide =
        decode_flags.contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE);
    let distinct =
        decode_flags.contains(vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT);
    if !coincide && !distinct {
        return Err(CapsError::NoDecodeMode);
    }

    let layered_dpb =
        !capability_flags.contains(vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES);
    // Presenter-facing images need the pool's exact usage plus MUTABLE_FORMAT;
    // distinct DPB needs neither sampling nor plane views.
    let try_coincide = || -> Result<(vk::Format, vk::Format), CapsError> {
        let mode = "coincide (DPB|DST|SAMPLED)";
        let entry = pick_format(coincide_formats, wanted, mode)?;
        require_usage(&entry, COINCIDE_USAGE, mode)?;
        require_mutable(&entry, mode)?;
        Ok((entry.format, entry.format))
    };
    let try_distinct = || -> Result<(vk::Format, vk::Format), CapsError> {
        let dpb = pick_format(dpb_formats, wanted, "DPB")?;
        require_usage(&dpb, DPB_USAGE, "DPB")?;
        let out_mode = "output (DST|SAMPLED)";
        let output = pick_format(output_formats, wanted, out_mode)?;
        require_usage(&output, OUTPUT_USAGE, out_mode)?;
        require_mutable(&output, out_mode)?;
        Ok((dpb.format, output.format))
    };
    // Coincide preferred (struct docs). A device offering both whose coincide
    // arrangement is unusable decodes distinct; the error names distinct then.
    // `PUNKTFUNK_VKDECODE_ARRANGEMENT=distinct` skips coincide on such a device: the
    // switch for a driver whose coincide path faults at the first decode.
    let prefer_distinct = distinct
        && std::env::var("PUNKTFUNK_VKDECODE_ARRANGEMENT")
            .ok()
            .as_deref()
            == Some("distinct");
    let (coincide, (dpb_format, output_format)) = match try_coincide() {
        Ok(formats) if coincide && !prefer_distinct => (true, formats),
        Err(unusable) if !distinct => return Err(unusable),
        tried => {
            if let (true, Err(unusable)) = (coincide, &tried) {
                tracing::info!(%unusable, "coincide arrangement unusable, decoding distinct");
            }
            (false, try_distinct()?)
        }
    };
    let plane_view_formats = plane_formats(output_format).ok_or(CapsError::NoPlaneMapping {
        format: output_format,
    })?;

    Ok(Arrangement {
        coincide,
        layered_dpb,
        dpb_format,
        output_format,
        plane_view_formats,
    })
}

fn pick_format(
    formats: &[VideoFormat],
    wanted: vk::Format,
    mode: &'static str,
) -> Result<VideoFormat, CapsError> {
    formats
        .iter()
        .copied()
        .find(|f| f.format == wanted)
        .ok_or(CapsError::NoFormat { mode, wanted })
}

/// Pool creation usage must sit inside what the driver advertised. An echoed report
/// refuses outright: Intel's Windows driver decodes into such an image, and every
/// sample of it reads zero.
fn require_usage(
    entry: &VideoFormat,
    usage: vk::ImageUsageFlags,
    mode: &'static str,
) -> Result<(), CapsError> {
    if entry.echoed() {
        return Err(CapsError::EchoedReport {
            mode,
            format: entry.format,
        });
    }
    let missing = usage & !entry.image_usage;
    if missing.is_empty() {
        Ok(())
    } else {
        // The entry's own format, not the caller's `wanted`: they match here,
        // and the driver's record is what the message should name.
        Err(CapsError::UsageUnsupported {
            mode,
            format: entry.format,
            missing,
        })
    }
}

/// Per-plane views need `MUTABLE_FORMAT` in the driver's create-flag report.
fn require_mutable(entry: &VideoFormat, mode: &'static str) -> Result<(), CapsError> {
    if entry
        .image_create_flags
        .contains(vk::ImageCreateFlags::MUTABLE_FORMAT)
    {
        Ok(())
    } else {
        Err(CapsError::NoMutableFormat {
            mode,
            format: entry.format,
        })
    }
}

/// One movable H.264 decode profile chain. Vulkan profile identity is by value,
/// so every consumer rebuilds a structurally identical chain rather than sharing
/// pointers.
///
/// [`Self::wire`] links `profile.p_next` to this struct's own `h264` field.
/// Do not move the value between `wire()` and the last use of the returned
/// reference — or of any raw pointer taken from it. `wire` borrows `self` for
/// the reference's life, so passing the reference on keeps the chain pinned.
/// Taking a `*const` ends the borrow at that line: the decoder's query
/// pool must do that (`push_next` would clobber the profile's own `p_next`), so
/// it holds the reference across the call in `OpRing::create_status_query_pool`.
pub(crate) struct H264ProfileChain {
    h264: vk::VideoDecodeH264ProfileInfoKHR<'static>,
    /// Decode usage hints between the profile and the codec struct. Optional by the
    /// spec, but Intel's Windows driver walks the chain expecting it and faults on
    /// the first parameters create without it; FFmpeg always chains it.
    usage: vk::VideoDecodeUsageInfoKHR<'static>,
    profile: vk::VideoProfileInfoKHR<'static>,
}

impl H264ProfileChain {
    /// Unwired chain for one SPS profile. `std_profile_idc` is the converted
    /// idc (66/77/100/244 pass-through). H.264 here is 8-bit 4:2:0 progressive.
    pub(crate) fn new(std_profile_idc: hh::StdVideoH264ProfileIdc) -> Self {
        Self {
            h264: vk::VideoDecodeH264ProfileInfoKHR::default()
                .std_profile_idc(std_profile_idc)
                .picture_layout(vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE),
            usage: vk::VideoDecodeUsageInfoKHR::default(),
            profile: vk::VideoProfileInfoKHR::default()
                .video_codec_operation(vk::VideoCodecOperationFlagsKHR::DECODE_H264)
                .chroma_subsampling(vk::VideoChromaSubsamplingFlagsKHR::TYPE_420)
                .luma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8)
                .chroma_bit_depth(vk::VideoComponentBitDepthFlagsKHR::TYPE_8),
        }
    }

    /// Wire the internal `p_next` chain and hand out the profile root. The root, and
    /// any copy of it, borrows `self`; a raw pointer from it must not outlive `self`
    /// in place.
    pub(crate) fn wire(&mut self) -> &vk::VideoProfileInfoKHR<'_> {
        self.usage.p_next = (&self.h264 as *const vk::VideoDecodeH264ProfileInfoKHR<'_>).cast();
        self.profile.p_next = (&self.usage as *const vk::VideoDecodeUsageInfoKHR<'_>).cast();
        &self.profile
    }
}

/// Codec profile a session — and every image, buffer and query pool created
/// for it — is built against. A `Copy` descriptor, not a chain: Vulkan profile
/// identity is by value, so each consumer rebuilds its own chain from this.
///
/// `StdVideoH264ProfileIdc`, `StdVideoH265ProfileIdc` and `StdVideoAV1Profile`
/// are all `c_uint`; a bare idc would let one codec's profile build another's
/// chain. The enum makes that unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeProfile {
    H264(hh::StdVideoH264ProfileIdc),
    H265(H265ProfileKey),
    /// AV1's key also carries `filmGrainSupport`, which is part of the profile,
    /// so a grain pool is a different pool from a grain-less one
    /// ([`crate::caps_av1`]).
    Av1(Av1ProfileKey),
}

impl DecodeProfile {
    /// Fresh unwired chain. Call [`ProfileChain::wire`] and keep it immobile
    /// while the wired pointers live.
    pub(crate) fn chain(self) -> ProfileChain {
        match self {
            DecodeProfile::H264(idc) => ProfileChain::H264(H264ProfileChain::new(idc)),
            DecodeProfile::H265(key) => ProfileChain::H265(H265ProfileChain::new(key)),
            DecodeProfile::Av1(key) => ProfileChain::Av1(Av1ProfileChain::new(key)),
        }
    }
}

/// One codec's profile chain, type-erased for shared creation paths. Same
/// immobility contract as the three variants.
pub(crate) enum ProfileChain {
    H264(H264ProfileChain),
    H265(H265ProfileChain),
    Av1(Av1ProfileChain),
}

impl ProfileChain {
    /// Wire the chain and hand out the profile root, borrowing `self` as the
    /// variants' `wire` does.
    pub(crate) fn wire(&mut self) -> &vk::VideoProfileInfoKHR<'_> {
        match self {
            ProfileChain::H264(chain) => chain.wire(),
            ProfileChain::H265(chain) => chain.wire(),
            ProfileChain::Av1(chain) => chain.wire(),
        }
    }
}

/// Asks the driver for video capabilities (decode + the profile's codec struct
/// chained) and the three format-property enumerations. Copies facts out;
/// derivation is [`derive_caps`].
///
/// A device that cannot host the profile (a grain-enabled AV1 profile, most
/// importantly) fails the capabilities call with a profile-unsupported result.
///
/// # Safety
///
/// `dev` wraps live handles per [`crate::DeviceHandles`] (instance-level
/// functions against its physical device).
pub(crate) unsafe fn query_caps(
    dev: &DecodeDevice,
    profile: DecodeProfile,
) -> Result<RawCaps, vk::Result> {
    let mut chain = profile.chain();
    let wired = chain.wire();

    let mut h264_caps = vk::VideoDecodeH264CapabilitiesKHR::default();
    let mut h265_caps = vk::VideoDecodeH265CapabilitiesKHR::default();
    let mut av1_caps = vk::VideoDecodeAV1CapabilitiesKHR::default();
    let mut decode_caps = vk::VideoDecodeCapabilitiesKHR::default();
    // `push_next` prepends: pushing the codec struct first leaves
    // VkVideoDecodeCapabilitiesKHR directly after the base. Reversed, a driver
    // that fills by position reads a Std level as the decode-mode flags.
    let caps = vk::VideoCapabilitiesKHR::default();
    let caps = match profile {
        DecodeProfile::H264(_) => caps.push_next(&mut h264_caps),
        DecodeProfile::H265(_) => caps.push_next(&mut h265_caps),
        DecodeProfile::Av1(_) => caps.push_next(&mut av1_caps),
    };
    let mut caps = caps.push_next(&mut decode_caps);
    // SAFETY: physical device is live (DeviceHandles contract); `wired` roots a
    // fully wired, immovable chain; `caps` chains driver-fillable structs that all
    // outlive the call.
    let r = unsafe {
        (dev.video_queue_instance()
            .fp()
            .get_physical_device_video_capabilities_khr)(
            dev.physical_device(), wired, &mut caps
        )
    };
    if r != vk::Result::SUCCESS {
        return Err(r);
    }
    // Copy out before the chained &mut borrows end.
    let capability_flags = caps.flags;
    let min_bitstream_buffer_offset_alignment = caps.min_bitstream_buffer_offset_alignment;
    let min_bitstream_buffer_size_alignment = caps.min_bitstream_buffer_size_alignment;
    let picture_access_granularity = caps.picture_access_granularity;
    let min_coded_extent = caps.min_coded_extent;
    let max_coded_extent = caps.max_coded_extent;
    let max_dpb_slots = caps.max_dpb_slots;
    let max_active_reference_pictures = caps.max_active_reference_pictures;
    let std_header_version = caps.std_header_version;
    let decode_flags = decode_caps.flags;
    let max_level = max_level_of(profile, &h264_caps, &h265_caps, &av1_caps);

    // Verbatim driver fill, before interpretation. Populated `max_dpb_slots` next
    // to `decode_flags` 0 is a real "no DPB mode"; zeros on both mean the query
    // never landed.
    tracing::debug!(
        ?profile,
        ?capability_flags,
        ?decode_flags,
        decode_flags_raw = decode_flags.as_raw(),
        %max_level,
        max_dpb_slots,
        max_active_reference_pictures,
        ?min_coded_extent,
        ?max_coded_extent,
        ?picture_access_granularity,
        "driver video capabilities, verbatim"
    );

    // Query the real creation usages (SAMPLED included for presenter-facing
    // roles) so the answers validate the images the pools build.
    // SAFETY: same liveness as above; the helper wires its own chain (this and
    // the two calls below).
    let dpb_formats = unsafe { query_formats(dev, profile, DPB_USAGE)? };
    // SAFETY: as above.
    let output_formats = unsafe { query_formats(dev, profile, OUTPUT_USAGE)? };
    // SAFETY: as above.
    let coincide_formats = unsafe { query_formats(dev, profile, COINCIDE_USAGE)? };

    Ok(RawCaps {
        capability_flags,
        decode_flags,
        min_bitstream_buffer_offset_alignment,
        min_bitstream_buffer_size_alignment,
        picture_access_granularity,
        min_coded_extent,
        max_coded_extent,
        max_dpb_slots,
        max_active_reference_pictures,
        max_level,
        std_header_version,
        dpb_formats,
        output_formats,
        coincide_formats,
    })
}

/// The level ceiling from the codec struct `profile` chained, tagged with that
/// codec. The other two structs were never handed to the driver.
fn max_level_of(
    profile: DecodeProfile,
    h264: &vk::VideoDecodeH264CapabilitiesKHR<'_>,
    h265: &vk::VideoDecodeH265CapabilitiesKHR<'_>,
    av1: &vk::VideoDecodeAV1CapabilitiesKHR<'_>,
) -> MaxLevelIdc {
    match profile {
        DecodeProfile::H264(_) => MaxLevelIdc::H264(h264.max_level_idc),
        DecodeProfile::H265(_) => MaxLevelIdc::H265(h265.max_level_idc),
        DecodeProfile::Av1(_) => MaxLevelIdc::Av1(av1.max_level),
    }
}

/// Video format properties for one usage. A usage the implementation rejects
/// maps to an empty list ("not this arrangement"); [`derive_caps`] routes around.
///
/// # Safety
///
/// As [`query_caps`].
pub(crate) unsafe fn query_formats(
    dev: &DecodeDevice,
    decode_profile: DecodeProfile,
    usage: vk::ImageUsageFlags,
) -> Result<Vec<VideoFormat>, vk::Result> {
    // SAFETY: the caller's DeviceHandles contract makes these two live, which is
    // exactly what the physical-device form needs.
    unsafe {
        query_formats_on(
            dev.video_queue_instance(),
            dev.physical_device(),
            decode_profile,
            usage,
        )
    }
}

/// [`query_formats`] against a bare physical device — no `VkDevice`.
///
/// [`crate::probe`] enumerates through this same path so its answer is the one
/// derivation will see. `vkGetPhysicalDeviceVideoFormatPropertiesKHR` is an
/// instance-level command over a physical device.
///
/// # Safety
///
/// `video_queue_instance` must be loaded against a live `VkInstance`, and
/// `physical_device` must be one of that instance's physical devices.
pub(crate) unsafe fn query_formats_on(
    video_queue_instance: &ash::khr::video_queue::Instance,
    physical_device: vk::PhysicalDevice,
    decode_profile: DecodeProfile,
    usage: vk::ImageUsageFlags,
) -> Result<Vec<VideoFormat>, vk::Result> {
    let mut chain = decode_profile.chain();
    let profile = chain.wire();
    let mut profile_list =
        vk::VideoProfileListInfoKHR::default().profiles(std::slice::from_ref(profile));
    let info = vk::PhysicalDeviceVideoFormatInfoKHR::default()
        .image_usage(usage)
        .push_next(&mut profile_list);

    let fp = video_queue_instance
        .fp()
        .get_physical_device_video_format_properties_khr;
    let mut count = 0u32;
    // SAFETY: live physical device; `info` roots a wired chain outliving the call;
    // null properties pointer is the spec's count-query form.
    let r = unsafe { fp(physical_device, &info, &mut count, std::ptr::null_mut()) };
    match r {
        vk::Result::SUCCESS => {}
        // This usage/profile has no formats: an arrangement gap, not a failure.
        vk::Result::ERROR_FORMAT_NOT_SUPPORTED
        | vk::Result::ERROR_IMAGE_USAGE_NOT_SUPPORTED_KHR => return Ok(Vec::new()),
        err => return Err(err),
    }
    let mut props = vec![vk::VideoFormatPropertiesKHR::default(); count as usize];
    // SAFETY: as above, with a properties array of exactly the driver-reported count.
    let r = unsafe { fp(physical_device, &info, &mut count, props.as_mut_ptr()) };
    if r != vk::Result::SUCCESS && r != vk::Result::INCOMPLETE {
        return Err(r);
    }
    props.truncate(count as usize);
    // The driver's masks stay verbatim for the probe; the queried usage rides beside them.
    Ok(props
        .iter()
        .map(|p| VideoFormat {
            format: p.format,
            image_usage: p.image_usage_flags,
            image_create_flags: p.image_create_flags,
            image_type: p.image_type,
            image_tiling: p.image_tiling,
            queried: usage,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(format: vk::Format, usage: vk::ImageUsageFlags) -> VideoFormat {
        VideoFormat {
            format,
            image_usage: usage,
            image_create_flags: vk::ImageCreateFlags::MUTABLE_FORMAT
                | vk::ImageCreateFlags::ALIAS
                | vk::ImageCreateFlags::EXTENDED_USAGE,
            ..Default::default()
        }
    }

    /// One fixed video-only mask and no create flags for every query, as Intel's
    /// Windows driver answers.
    fn echoed_entry(format: vk::Format, queried: vk::ImageUsageFlags) -> VideoFormat {
        VideoFormat {
            format,
            image_usage: vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR
                | vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR,
            queried,
            ..Default::default()
        }
    }

    #[test]
    fn a_second_slice_segment_is_refused_only_for_a_single_slice_driver() {
        assert!(require_segments(false, 4).is_ok());
        assert!(require_segments(true, 1).is_ok());
        let refused = require_segments(true, 2).unwrap_err();
        assert_eq!(refused, CapsError::SliceSegments { segments: 2 });
        assert!(crate::VkDecodeError::Caps(refused).is_device_fact());
    }

    #[test]
    fn an_echoed_report_refuses_by_name() {
        let e = echoed_entry(NV12, COINCIDE_USAGE);
        assert!(e.echoed());
        let refused = require_usage(&e, COINCIDE_USAGE, "coincide").unwrap_err();
        assert_eq!(
            refused,
            CapsError::EchoedReport {
                mode: "coincide",
                format: NV12
            }
        );
        assert!(crate::VkDecodeError::Caps(refused).is_device_fact());
    }

    #[test]
    fn a_report_that_answers_the_query_keeps_its_refusals() {
        let mut e = entry(NV12, DPB_USAGE);
        e.queried = DPB_USAGE;
        assert!(!e.echoed());
        assert!(require_usage(&e, DPB_USAGE, "DPB").is_ok());
        assert!(require_usage(&e, COINCIDE_USAGE, "coincide").is_err());
        e.image_create_flags = vk::ImageCreateFlags::empty();
        assert!(require_mutable(&e, "coincide").is_err());
    }

    fn radv_like() -> RawCaps {
        RawCaps {
            capability_flags: vk::VideoCapabilityFlagsKHR::SEPARATE_REFERENCE_IMAGES,
            decode_flags: vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE,
            min_bitstream_buffer_offset_alignment: 128,
            min_bitstream_buffer_size_alignment: 128,
            picture_access_granularity: vk::Extent2D {
                width: 1,
                height: 1,
            },
            min_coded_extent: vk::Extent2D {
                width: 16,
                height: 16,
            },
            max_coded_extent: vk::Extent2D {
                width: 8192,
                height: 8192,
            },
            max_dpb_slots: 17,
            max_active_reference_pictures: 16,
            max_level: MaxLevelIdc::H264(hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2),
            std_header_version: vk::ExtensionProperties::default(),
            dpb_formats: vec![],
            output_formats: vec![],
            coincide_formats: vec![entry(NV12, COINCIDE_USAGE), entry(P010, COINCIDE_USAGE)],
        }
    }

    /// Distinct only, no separate reference images (layered DPB). DPB entry has
    /// neither sampling nor mutable format — requiring them would fail real devices.
    fn nvidia_like() -> RawCaps {
        RawCaps {
            capability_flags: vk::VideoCapabilityFlagsKHR::empty(),
            decode_flags: vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT,
            dpb_formats: vec![VideoFormat {
                format: NV12,
                image_usage: DPB_USAGE,
                image_create_flags: vk::ImageCreateFlags::empty(),
                ..Default::default()
            }],
            output_formats: vec![entry(NV12, OUTPUT_USAGE)],
            coincide_formats: vec![],
            ..radv_like()
        }
    }

    /// [`OUTPUT_FORMATS`] is what a consumer pins against: listed-but-unmappable
    /// fails a pool build; produced-but-unlisted is silent wrong colour math.
    #[test]
    fn the_output_format_vocabulary_is_the_whole_of_what_this_crate_delivers() {
        for format in OUTPUT_FORMATS {
            assert!(
                plane_formats(format).is_some(),
                "{format:?} is advertised as an output but has no plane views"
            );
        }
        // Every (chroma, depth) pair the H.265 envelope admits must land here.
        for chroma in 0u8..=4 {
            for depth in 0u8..=4 {
                if let Some(f) = crate::caps_h265::output_format_for(chroma, depth) {
                    assert!(
                        OUTPUT_FORMATS.contains(&f),
                        "output_format_for({chroma}, {depth}) = {f:?} is outside \
                         OUTPUT_FORMATS"
                    );
                }
            }
        }
        assert!(OUTPUT_FORMATS.contains(&NV12));
    }

    #[test]
    fn a_coincide_device_derives_coincide_with_one_shared_format() {
        let caps = derive_caps(&radv_like(), NV12).unwrap();
        assert!(caps.coincide);
        assert!(
            !caps.layered_dpb,
            "separate reference images advertised — per-slot images"
        );
        assert_eq!(caps.dpb_format, NV12);
        assert_eq!(caps.output_format, NV12);
        assert_eq!(caps.max_dpb_slots, 17);
        assert_eq!(caps.min_bitstream_offset_alignment, 128);
    }

    /// All three `StdVideo*Level*` types are `c_uint` and the code spaces disagree
    /// (H.264 6.2 is 18, H.265 6.2 is 12, AV1 5.1 is 13). Each profile must read
    /// its own codec's struct, or the decoders' numeric level gate compares the
    /// wrong code space.
    #[test]
    fn each_profile_reads_the_level_ceiling_of_its_own_codec_struct() {
        let h264 = vk::VideoDecodeH264CapabilitiesKHR {
            max_level_idc: hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2,
            ..Default::default()
        };
        let h265 = vk::VideoDecodeH265CapabilitiesKHR {
            max_level_idc: hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2,
            ..Default::default()
        };
        let av1 = vk::VideoDecodeAV1CapabilitiesKHR {
            max_level: hh::StdVideoAV1Level_STD_VIDEO_AV1_LEVEL_5_1,
            ..Default::default()
        };

        let h264_key = hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_HIGH;
        let h265_key = crate::caps_h265::H265ProfileKey::from_stream(1, 1, false, 0, 0).unwrap();
        let av1_key = crate::caps_av1::Av1ProfileKey::from_stream(0, 1, 8, false).unwrap();
        assert_eq!(
            max_level_of(DecodeProfile::H264(h264_key), &h264, &h265, &av1),
            MaxLevelIdc::H264(hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2)
        );
        assert_eq!(
            max_level_of(DecodeProfile::H265(h265_key), &h264, &h265, &av1),
            MaxLevelIdc::H265(hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2)
        );
        assert_eq!(
            max_level_of(DecodeProfile::Av1(av1_key), &h264, &h265, &av1),
            MaxLevelIdc::Av1(hh::StdVideoAV1Level_STD_VIDEO_AV1_LEVEL_5_1)
        );
        assert_ne!(
            MaxLevelIdc::H265(hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2),
            MaxLevelIdc::H264(hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2),
            "same number, different codec — not the same ceiling"
        );
    }

    #[test]
    fn a_distinct_device_derives_distinct_with_a_layered_dpb() {
        let caps = derive_caps(&nvidia_like(), NV12).unwrap();
        assert!(!caps.coincide);
        assert!(
            caps.layered_dpb,
            "no SEPARATE_REFERENCE_IMAGES — one image array carries every slot"
        );
        assert_eq!(caps.dpb_format, NV12);
        assert_eq!(caps.output_format, NV12);
    }

    #[test]
    fn a_device_advertising_both_modes_prefers_coincide() {
        let mut raw = radv_like();
        raw.decode_flags = vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE
            | vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT;
        raw.dpb_formats = vec![entry(NV12, DPB_USAGE)];
        raw.output_formats = vec![entry(NV12, OUTPUT_USAGE)];
        let caps = derive_caps(&raw, NV12).unwrap();
        assert!(caps.coincide, "coincide wins when both are offered");
    }

    /// NVIDIA on Windows: both modes, no `SEPARATE_REFERENCE_IMAGES`. A layered
    /// coincide array is valid; distinct remains the fallback for a bad format.
    #[test]
    fn a_device_whose_coincide_is_unusable_decodes_distinct() {
        let both = vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_COINCIDE
            | vk::VideoDecodeCapabilityFlagsKHR::DPB_AND_OUTPUT_DISTINCT;
        let mut raw = nvidia_like();
        raw.decode_flags = both;
        raw.coincide_formats = vec![entry(NV12, COINCIDE_USAGE)];
        let caps = derive_caps(&raw, NV12).unwrap();
        assert!(caps.coincide && caps.layered_dpb);

        // Same device, but the coincide list lacks the wanted format.
        raw.coincide_formats = vec![entry(P010, COINCIDE_USAGE)];
        let caps = derive_caps(&raw, NV12).unwrap();
        assert!(!caps.coincide && caps.layered_dpb);
    }

    #[test]
    fn no_mode_and_no_nv12_are_distinct_hard_errors() {
        let mut raw = radv_like();
        raw.decode_flags = vk::VideoDecodeCapabilityFlagsKHR::empty();
        assert_eq!(
            derive_caps(&raw, NV12).unwrap_err(),
            CapsError::NoDecodeMode
        );

        let mut raw = radv_like();
        raw.coincide_formats = vec![entry(P010, COINCIDE_USAGE)];
        assert_eq!(
            derive_caps(&raw, NV12).unwrap_err(),
            CapsError::NoFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                wanted: NV12
            }
        );

        // Distinct mode names which half is missing NV12.
        let mut raw = nvidia_like();
        raw.output_formats = vec![];
        assert_eq!(
            derive_caps(&raw, NV12).unwrap_err(),
            CapsError::NoFormat {
                mode: "output (DST|SAMPLED)",
                wanted: NV12
            }
        );
    }

    #[test]
    fn an_advertised_usage_missing_a_creation_bit_is_an_error_naming_the_gap() {
        // Coincide entry with decode but not sampling: presenter cannot read it.
        let mut raw = radv_like();
        raw.coincide_formats = vec![entry(
            NV12,
            vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
        )];
        assert_eq!(
            derive_caps(&raw, NV12).unwrap_err(),
            CapsError::UsageUnsupported {
                mode: "coincide (DPB|DST|SAMPLED)",
                format: NV12,
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );
        assert!(
            derive_caps(&raw, NV12)
                .unwrap_err()
                .to_string()
                .contains("zero-copy path cannot exist"),
            "a missing SAMPLED must name its consequence: {}",
            derive_caps(&raw, NV12).unwrap_err()
        );

        let mut raw = nvidia_like();
        raw.output_formats = vec![entry(NV12, vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR)];
        assert_eq!(
            derive_caps(&raw, NV12).unwrap_err(),
            CapsError::UsageUnsupported {
                mode: "output (DST|SAMPLED)",
                format: NV12,
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );

        // A 10-bit stream is refused about P010, not NV12.
        let mut raw = radv_like();
        raw.coincide_formats = vec![entry(
            P010,
            vk::ImageUsageFlags::VIDEO_DECODE_DPB_KHR | vk::ImageUsageFlags::VIDEO_DECODE_DST_KHR,
        )];
        let err = derive_caps(&raw, P010).unwrap_err();
        assert_eq!(
            err,
            CapsError::UsageUnsupported {
                mode: "coincide (DPB|DST|SAMPLED)",
                format: P010,
                missing: vk::ImageUsageFlags::SAMPLED
            }
        );
        assert!(err.to_string().contains("G10X6"), "{err}");
    }

    #[test]
    fn a_presenter_facing_entry_without_mutable_format_is_refused() {
        let mut raw = radv_like();
        raw.coincide_formats = vec![VideoFormat {
            format: NV12,
            image_usage: COINCIDE_USAGE,
            // A non-empty report that lacks MUTABLE_FORMAT: an explicit envelope, refused.
            image_create_flags: vk::ImageCreateFlags::ALIAS,
            ..Default::default()
        }];
        assert_eq!(
            derive_caps(&raw, NV12).unwrap_err(),
            CapsError::NoMutableFormat {
                mode: "coincide (DPB|DST|SAMPLED)",
                format: NV12,
            }
        );

        // Distinct DPB needs no MUTABLE_FORMAT (nvidia_like has empty create flags).
        assert!(derive_caps(&nvidia_like(), NV12).is_ok());
    }

    #[test]
    fn extents_round_up_to_the_picture_access_granularity() {
        let mut raw = radv_like();
        raw.picture_access_granularity = vk::Extent2D {
            width: 64,
            height: 16,
        };
        let caps = derive_caps(&raw, NV12).unwrap();
        // 1920×1080: width already aligned; height rounds to 1088.
        let aligned = caps.aligned_extent(vk::Extent2D {
            width: 1920,
            height: 1080,
        });
        assert_eq!((aligned.width, aligned.height), (1920, 1088));

        // Granularity 1 is identity; a zero axis degrades to 1, not a panic.
        let mut raw = radv_like();
        raw.picture_access_granularity = vk::Extent2D {
            width: 0,
            height: 1,
        };
        let caps = derive_caps(&raw, NV12).unwrap();
        let aligned = caps.aligned_extent(vk::Extent2D {
            width: 321,
            height: 241,
        });
        assert_eq!((aligned.width, aligned.height), (321, 241));
    }

    #[test]
    fn coincide_with_a_layered_dpb_uses_one_picture_array() {
        let mut raw = radv_like();
        raw.capability_flags = vk::VideoCapabilityFlagsKHR::empty();
        let caps = derive_caps(&raw, NV12).unwrap();
        assert!(caps.coincide && caps.layered_dpb);
    }

    /// `derive_caps` never reads the codec. Each codec's device derives the same
    /// arrangement, passes its own DPB depth and level through, and takes a
    /// 10-bit or 4:4:4 format only when it lists that format.
    #[test]
    fn every_codec_device_derives_through_the_same_rules() {
        let devices = [
            (
                MaxLevelIdc::H264(hh::StdVideoH264LevelIdc_STD_VIDEO_H264_LEVEL_IDC_6_2),
                17,
            ),
            (
                MaxLevelIdc::H265(hh::StdVideoH265LevelIdc_STD_VIDEO_H265_LEVEL_IDC_6_2),
                17,
            ),
            (
                MaxLevelIdc::Av1(hh::StdVideoAV1Level_STD_VIDEO_AV1_LEVEL_5_1),
                9,
            ),
        ];
        for (max_level, max_dpb_slots) in devices {
            let nv12_only = RawCaps {
                max_level,
                max_dpb_slots,
                coincide_formats: vec![entry(NV12, COINCIDE_USAGE)],
                ..radv_like()
            };
            let caps = derive_caps(&nv12_only, NV12).unwrap();
            assert!(caps.coincide && !caps.layered_dpb);
            assert_eq!(caps.max_dpb_slots, max_dpb_slots);
            assert_eq!(caps.max_level_idc, max_level);

            for wanted in [P010, YUV444_8, YUV444_10] {
                assert_eq!(
                    derive_caps(&nv12_only, wanted).unwrap_err(),
                    CapsError::NoFormat {
                        mode: "coincide (DPB|DST|SAMPLED)",
                        wanted
                    }
                );
                let listed = RawCaps {
                    coincide_formats: vec![
                        entry(NV12, COINCIDE_USAGE),
                        entry(wanted, COINCIDE_USAGE),
                    ],
                    ..nv12_only.clone()
                };
                let caps = derive_caps(&listed, wanted).unwrap();
                assert_eq!(caps.output_format, wanted);
                assert_eq!(Some(caps.plane_view_formats), plane_formats(wanted));
            }
        }
    }

    #[test]
    fn zero_alignments_normalize_to_one_so_ring_math_never_divides_by_zero() {
        let mut raw = radv_like();
        raw.min_bitstream_buffer_offset_alignment = 0;
        raw.min_bitstream_buffer_size_alignment = 0;
        let caps = derive_caps(&raw, NV12).unwrap();
        assert_eq!(caps.min_bitstream_offset_alignment, 1);
        assert_eq!(caps.min_bitstream_size_alignment, 1);
    }

    #[test]
    fn plane_view_formats_follow_the_picture_format_bit_depth() {
        // 8-bit families sample through R8/R8G8; 10-bit 3PACK16 must use R10X6.
        // An R8 view over a 10-bit plane reads half of every sample.
        assert_eq!(
            plane_formats(NV12),
            Some([vk::Format::R8_UNORM, vk::Format::R8G8_UNORM])
        );
        assert_eq!(plane_formats(YUV444_8), plane_formats(NV12));
        assert_eq!(
            plane_formats(P010),
            Some([
                vk::Format::R10X6_UNORM_PACK16,
                vk::Format::R10X6G10X6_UNORM_2PACK16
            ])
        );
        assert_eq!(plane_formats(YUV444_10), plane_formats(P010));
        // Anything else has no mapping — derivation refuses rather than guesses.
        assert_eq!(plane_formats(vk::Format::R8G8B8A8_UNORM), None);

        // Derived caps carry the resolved pair, so pools never re-derive.
        let caps = derive_caps(&radv_like(), NV12).unwrap();
        assert_eq!(
            caps.plane_view_formats,
            [vk::Format::R8_UNORM, vk::Format::R8G8_UNORM]
        );
    }

    #[test]
    fn the_profile_chain_wires_h264_behind_the_root_profile() {
        let mut chain =
            H264ProfileChain::new(hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN);
        let profile = chain.wire();
        assert_eq!(
            profile.video_codec_operation,
            vk::VideoCodecOperationFlagsKHR::DECODE_H264
        );
        assert!(!profile.p_next.is_null());
        // SAFETY: wire() pointed p_next at chain's own usage field, which lives for
        // this whole scope and is a valid VideoDecodeUsageInfoKHR.
        let usage = unsafe { &*profile.p_next.cast::<vk::VideoDecodeUsageInfoKHR<'_>>() };
        assert_eq!(usage.s_type, vk::StructureType::VIDEO_DECODE_USAGE_INFO_KHR);
        assert_eq!(
            usage.video_usage_hints,
            vk::VideoDecodeUsageFlagsKHR::DEFAULT
        );
        // SAFETY: wire() pointed the usage struct's p_next at chain's own h264 field,
        // which lives for this whole scope and is a valid VideoDecodeH264ProfileInfoKHR.
        let h264 = unsafe { &*usage.p_next.cast::<vk::VideoDecodeH264ProfileInfoKHR<'_>>() };
        assert_eq!(
            h264.std_profile_idc,
            hh::StdVideoH264ProfileIdc_STD_VIDEO_H264_PROFILE_IDC_MAIN
        );
        assert_eq!(
            h264.picture_layout,
            vk::VideoDecodeH264PictureLayoutFlagsKHR::PROGRESSIVE
        );
    }
}
