//! Linux backend selection: the `PUNKTFUNK_ENCODER` resolver, the open ladder (direct-SDK
//! NVENC, Vulkan Video, native VAAPI, PyroWave, openh264) and every capability answer that
//! must agree with it, each read off one [`Resolved`].

use super::*;
use crate::env_knobs::Knobs;
use pf_frame::PixelFormat;

pub use crate::vk_util::anv_video_encode_flag;

/// What resolution found on Linux. Each cell is filled the first time an answer needs it.
#[derive(Debug, Default)]
pub(crate) struct Probes {
    /// The backend the capability answers read ([`Resolved::linux_resolved_backend`]).
    pub(crate) backend: OnceLock<LinuxBackend>,
    /// `auto` reads VAAPI ([`linux_auto_is_vaapi`]).
    pub(crate) auto_is_vaapi: OnceLock<bool>,
    /// The direct SDK's codec list, 4:4:4 and 10-bit caps.
    pub(crate) nvenc: OnceLock<NvencFacts>,
    /// The AMD/Intel plane's 8-bit codec set ([`Resolved::probe_vaapi_codec_support`]), kept
    /// only once it finds a codec.
    pub(crate) vaapi: OnceLock<CodecSupport>,
    /// Set when the `vaapi` probe found nothing, so the miss warns once.
    pub(crate) vaapi_missed: OnceLock<()>,
    /// libva's 10-bit encode entrypoints, HEVC then AV1.
    pub(crate) vaapi_ten_bit: [OnceLock<bool>; 2],
    /// Vulkan Video encode caps, HEVC then AV1.
    pub(crate) vulkan: [OnceLock<VulkanFacts>; 2],
    /// pf-zerocopy's raw lane: `PUNKTFUNK_NVENC_RAW`, and a box that runs the fused convert.
    pub(crate) nvenc_raw: OnceLock<bool>,
}

/// The direct SDK's answers. Codecs fail open (all-`false` is unanswered); 4:4:4 and 10-bit
/// fail closed.
#[derive(Clone, Copy, Debug)]
pub(crate) struct NvencFacts {
    pub(crate) codecs: CodecSupport,
    pub(crate) hevc_444: bool,
    pub(crate) ten_bit: CodecSupport,
}

/// Vulkan Video encode for one codec: an encode queue, and the profile at each depth. A device
/// can offer HEVC Main10 and still decline 10-bit AV1.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VulkanFacts {
    pub(crate) supported: bool,
    pub(crate) eight_bit: bool,
    pub(crate) ten_bit: bool,
    /// The pairs the device's EFC converts; `None` without an EFC.
    pub(crate) efc: Option<crate::vcn_firmware::EfcPairs>,
    pub(crate) vcn: Option<VcnFirmware>,
}

/// [`Resolved::wire_caps`] on Linux: the narrowed probe `/serverinfo` advertises too, so the
/// two cannot drift. PyroWave ORs onto the H.26x set whenever a GPU backend could open its
/// Vulkan device; `resolve_codec` ignores the bit unless the client prefers it.
pub(crate) fn wire_caps(r: &Resolved) -> u8 {
    let pyro = if r.inputs.features.pyrowave && r.backend() != LinuxBackend::Software {
        punktfunk_core::quic::CODEC_PYROWAVE
    } else {
        0
    };
    codec_support_wire_mask(r.advertised_codecs()).unwrap_or(0) | pyro
}

/// [`Resolved::advertised`] on Linux: every backend answers, narrowed.
pub(crate) fn advertised(r: &Resolved) -> Option<CodecSupport> {
    Some(r.advertised_codecs())
}

/// HEVC 4:4:4 on the resolved backend: the direct SDK's `YUV444_ENCODE` cap. The native VAAPI
/// session is 4:2:0 only.
pub(crate) fn hevc_444(r: &Resolved) -> bool {
    r.inputs.features.nvenc && !r.zero_copy_is_vaapi() && r.nvenc().hevc_444
}

/// 10-bit `codec` on the resolved backend, the Encoder row honoured. AMD/Intel tries Vulkan then
/// VAAPI, so 10-bit is there if either says yes; NVIDIA asks the driver's `10BIT_ENCODE` cap.
pub(crate) fn ten_bit(r: &Resolved, codec: Codec) -> bool {
    if r.zero_copy_is_vaapi() {
        return r.vulkan_10bit_available(codec) || r.vaapi_ten_bit(codec);
    }
    if !r.inputs.features.nvenc {
        return false;
    }
    let t = r.nvenc().ten_bit;
    match codec {
        Codec::H265 => t.h265,
        Codec::Av1 => t.av1,
        _ => false,
    }
}

/// [`Resolved::is_gpu`] on Linux.
pub(crate) fn is_gpu(r: &Resolved) -> bool {
    r.backend() != LinuxBackend::Software
}

/// Linux 4:4:4 is capture-side (portal RGB → `yuv444p`); no encoder ingests RGB for it.
pub(crate) fn ingests_rgb_444(_: &Resolved) -> bool {
    false
}

/// [`Resolved::rgb_full_range`] on Linux. Vulkan Video's EFC converts BT.709 at narrow range
/// only on a two-pair VCN; an SDR session there is planned narrow so the chip still converts.
/// `PUNKTFUNK_VULKAN_RGB_DIRECT=0` takes the EFC out, and the range goes back to the ask.
pub(crate) fn rgb_full_range(r: &Resolved, codec: Codec) -> RgbFullRange {
    let two_pairs = r.vulkan_encodes(codec)
        && r.vulkan(codec).efc == Some(crate::vcn_firmware::EfcPairs::TwoPairs)
        && Knobs::from_env().vulkan_rgb_direct != Some(false);
    RgbFullRange {
        sdr: !two_pairs,
        hdr: true,
    }
}

/// [`Resolved::vcn_firmware`] on Linux: HEVC's probe names the device both codecs encode on.
pub(crate) fn vcn_firmware(r: &Resolved) -> Option<VcnFirmware> {
    r.vulkan_encodes(Codec::H265)
        .then(|| r.vulkan(Codec::H265).vcn)
        .flatten()
}

/// [`Resolved::carries_sdr10`] on Linux.
pub(crate) fn sdr10(r: &Resolved, codec: Codec) -> bool {
    // PyroWave's own CSC widens packed RGB to 10-bit (`rgb2yuv10_709.comp` / the 4:4:4
    // twin) on its private Vulkan device — the resolved H.26x backend is irrelevant.
    if codec == Codec::PyroWave {
        return r.inputs.features.pyrowave;
    }
    // Direct NVENC (HEVC + AV1) widens 8→10 from packed RGB. On AMD/Intel, Vulkan Video carries
    // HEVC Main10 and AV1 10-bit SDR (`rgb2yuv10_709.comp`) where the device offers the 10-bit
    // profile, and VAAPI carries HEVC Main10 under BT.709 as the fallback. The encoder degrades a
    // planar surface to 8-bit if some path delivers one.
    match r.backend() {
        LinuxBackend::Nvenc => r.inputs.features.nvenc,
        LinuxBackend::AmdIntel => {
            codec == Codec::H265 || (codec == Codec::Av1 && r.vulkan_10bit_available(codec))
        }
        LinuxBackend::Vulkan => {
            matches!(codec, Codec::H265 | Codec::Av1) && r.vulkan_10bit_available(codec)
        }
        _ => false,
    }
}

/// openh264 rate-control misconfigures if handed a hardware-session bitrate.
const SW_BITRATE_CEIL: u64 = 100_000_000;

/// [`crate::open_video`]'s backend half: the resolver over `resolved`'s pref and `auto` answer,
/// so a CUDA frame still opens NVENC under `auto` and an unknown pref still fails here. Shared
/// dim/fps/chroma checks stay in the caller. Every backend writes and signals `p.full_range`;
/// PyroWave is only ever asked for full.
pub(crate) fn open(
    resolved: &Resolved,
    p: &OpenParams,
    knobs: &Knobs,
) -> Result<(Box<dyn Encoder>, &'static str)> {
    let OpenParams {
        codec,
        format,
        width,
        height,
        fps,
        bitrate_bps,
        cuda,
        bit_depth,
        hdr,
        chroma,
        full_range,
        ..
    } = *p;
    // Negotiated PyroWave bypasses `PUNKTFUNK_ENCODER` (that pref is a lab override).
    if codec == Codec::PyroWave {
        debug_assert!(full_range, "PyroWave converts at full range only");
        #[cfg(feature = "pyrowave")]
        {
            // Worker seam, not the encoder: GPU-priority needs `CAP_SYS_NICE`,
            // which only `punktfunk-encode-worker` may carry. See `pyrowave_remote`.
            // `hdr` is the session's BT.2020 PQ verdict — 10-bit without it is SDR.
            return pyrowave_remote::open_preferring_worker(
                width,
                height,
                fps,
                bitrate_bps,
                chroma,
                bit_depth,
                hdr,
                knobs,
            )
            .map(|e| (e, "pyrowave"));
        }
        #[cfg(not(feature = "pyrowave"))]
        anyhow::bail!(
            "session negotiated PyroWave but this host was built without --features \
             punktfunk-host/pyrowave (the advertisement bit should not have been set)"
        );
    }
    // Default VAAPI. HEVC/AV1 open Vulkan Video first when the probe says yes, or always under
    // a `vulkan` pin; a failed open falls back so the stream does not die, since the
    // advertisement counts VAAPI's HEVC. Vulkan imports the dmabuf and does its own CSC.
    let open_amd_intel = |pinned: bool| -> Result<(Box<dyn Encoder>, &'static str)> {
        // Vulkan when the device probe says yes (same profile query the open makes). Gamescope
        // has no embedded cursor — CSC blend is the only pointer path. A `no` goes to VAAPI here,
        // not a failed open. Vulkan gets `bit_depth`; the rule only picks the arm.
        #[cfg(not(feature = "vulkan-encode"))]
        let _ = pinned;
        #[cfg(feature = "vulkan-encode")]
        if pinned || resolved.amd_intel_opens_vulkan(codec, bit_depth == 10, hdr) {
            match vulkan_video::VulkanVideoEncoder::open(
                codec,
                format,
                width,
                height,
                fps,
                bitrate_bps,
                p.cursor_blend,
                bit_depth,
                hdr,
                full_range,
                knobs,
            ) {
                Ok(e) => {
                    tracing::info!(
                        codec = ?codec,
                        "Linux Vulkan Video encode (real RFI via DPB reference slots) — \
                         set PUNKTFUNK_VULKAN_ENCODE=0 for VAAPI"
                    );
                    return Ok((Box::new(e) as Box<dyn Encoder>, "vulkan"));
                }
                Err(e) => tracing::warn!(
                    error = %format!("{e:#}"),
                    "Vulkan Video encode open failed — falling back to VAAPI"
                ),
            }
        }
        // The native session takes every capture shape, NV12 dmabufs included.
        // H.264 and HEVC; AV1 on AMD/Intel is Vulkan Video's. 10-bit SDR arrives as an 8-bit
        // surface at depth 10, or as gamescope's own P010 / packed 10-bit.
        vaapi_native::NativeVaapiEncoder::open(
            codec,
            width,
            height,
            fps,
            bitrate_bps,
            bit_depth,
            chroma,
            hdr,
            full_range,
            knobs,
        )
        .map(|e| (Box::new(e) as Box<dyn Encoder>, "vaapi-native"))
    };
    let open_nvidia = || open_nvenc(p, knobs).map(|e| (e, "nvenc"));
    let pref = resolved.inputs.pref.as_str();
    // Same resolver the capability answers consult, so the alias table exists once.
    match resolve_linux_backend(pref, || resolved.auto_is_vaapi(), cuda) {
        Some(LinuxBackend::Nvenc) => open_nvidia(),
        Some(LinuxBackend::AmdIntel) => open_amd_intel(false),
        Some(LinuxBackend::Vulkan) => {
            #[cfg(feature = "vulkan-encode")]
            {
                if !matches!(codec, Codec::H265 | Codec::Av1) {
                    anyhow::bail!(
                        "the Vulkan Video encoder supports HEVC + AV1; the session negotiated {codec:?}"
                    );
                }
                open_amd_intel(true)
            }
            #[cfg(not(feature = "vulkan-encode"))]
            {
                anyhow::bail!(
                    "PUNKTFUNK_ENCODER=vulkan requires a build with --features vulkan-encode"
                )
            }
        }
        // Explicit-only: `auto` never picks it (a dead NVIDIA driver still
        // exposes `/dev/nvidiactl` and would resolve to NVENC). H.264 + CPU RGB.
        Some(LinuxBackend::Software) => {
            if codec != Codec::H264 {
                anyhow::bail!(
                    "the software encoder emits H.264 only; the session negotiated {codec:?} \
                     (a client must advertise CODEC_H264 to reach a software host)"
                );
            }
            sw::OpenH264Encoder::open(
                format,
                width,
                height,
                fps,
                bitrate_bps.min(SW_BITRATE_CEIL),
                full_range,
                knobs,
            )
            .map(|e| (Box::new(e) as Box<dyn Encoder>, "software"))
        }
        None => anyhow::bail!(
            "unknown PUNKTFUNK_ENCODER={pref:?} — use auto (default), nvenc, vaapi, vulkan, pyrowave, or software"
        ),
    }
}

/// NVIDIA: the direct-SDK session (`--features nvenc`), which takes CUDA
/// frames zero-copy and uploads CPU frames itself. Without it there is no NVENC.
fn open_nvenc(p: &OpenParams, knobs: &Knobs) -> Result<Box<dyn Encoder>> {
    #[cfg(feature = "nvenc")]
    {
        tracing::info!(
            codec = p.codec.label(),
            cuda = p.cuda,
            "Linux direct-SDK NVENC"
        );
        Ok(Box::new(nvenc_cuda::NvencCudaEncoder::open(
            p.codec,
            p.format,
            p.width,
            p.height,
            p.fps,
            p.bitrate_bps,
            p.cuda,
            p.bit_depth,
            p.hdr,
            p.chroma,
            p.cursor_blend,
            p.slices,
            p.full_range,
            knobs,
        )?) as Box<dyn Encoder>)
    }
    #[cfg(not(feature = "nvenc"))]
    {
        let _ = knobs;
        anyhow::bail!(
            "{} on NVIDIA needs the direct-SDK NVENC backend, which this build left out — \
             build with --features punktfunk-host/nvenc",
            p.codec.label()
        )
    }
}

/// Whether a native planar source can carry this depth and colour: 8-bit SDR is NV12, HDR is
/// P010, and so is 10-bit SDR from a producer that composites it (`sdr10_native`). Any other
/// 10-bit SDR captures 8-bit packed RGB for the BT.709 widening CSC — native NV12 cannot be a
/// P010 source there.
const fn native_planar_depth_matches(bit_depth: u8, hdr: bool, sdr10_native: bool) -> bool {
    bit_depth < 10 || hdr || sdr10_native
}

impl Resolved {
    /// Whether this session can ingest a producer's own NV12 without a host pass. Both
    /// AMD/Intel lanes can: Vulkan Video imports it as its picture, the native libva session
    /// encodes it as imported. AV1 is Vulkan Video's alone there, and neither has a pass to
    /// blend a pointer into, so a `cursor_blend` session captures RGB. NVENC's raw lane copies
    /// the two planes into its own slot. It blends a pointer into NV12 but not P010, so a P010
    /// session (HDR, or gamescope's 10-bit SDR) needs no blend.
    pub fn native_nv12_ok(
        &self,
        codec: Codec,
        bit_depth: u8,
        hdr: bool,
        cursor_blend: bool,
        sdr10_native: bool,
    ) -> bool {
        if !native_planar_depth_matches(bit_depth, hdr, sdr10_native) {
            return false;
        }
        if !self.zero_copy_is_vaapi() {
            let p010 = hdr || (bit_depth == 10 && sdr10_native);
            return !(p010 && cursor_blend)
                && codec != Codec::PyroWave
                && self.nvenc_raw_dmabuf_ok();
        }
        if cursor_blend {
            return false;
        }
        match codec {
            Codec::H264 | Codec::H265 => true,
            Codec::Av1 => {
                self.inputs.features.vulkan_encode
                    && self.inputs.vulkan_row
                    && self.vulkan_encode_available(codec)
            }
            _ => false,
        }
    }

    /// May capture plan the NVENC raw-dmabuf lane? Its identity-scoped health applies the
    /// failure latch later, where the node id is known. Off without direct-SDK NVENC or on
    /// the AMD/Intel plane.
    pub fn nvenc_raw_dmabuf_ok(&self) -> bool {
        self.inputs.features.nvenc
            && !self.zero_copy_is_vaapi()
            && *self.probes.nvenc_raw.get_or_init(|| {
                pf_zerocopy::nvenc_raw_enabled() && pf_zerocopy::fused_convert_available()
            })
    }

    /// May an HDR capture stay zero-copy on NVIDIA (packed 10-bit PQ/BT.2020 CUDA)? Only
    /// direct-SDK NVENC can: it registers `ARGB10`/`ABGR10` and CSCs in the encoder. Without it
    /// the capturer must not build the HDR importer.
    pub fn hdr_cuda_ok(&self) -> bool {
        self.inputs.features.nvenc && !self.zero_copy_is_vaapi()
    }

    /// Whether the resolved backend composites [`CapturedFrame::cursor`]. Answered
    /// before capture opens: blend-capable backends take cursor-as-metadata; else
    /// the compositor must embed the pointer.
    ///
    /// `cuda_planned` is the caller's CUDA-payload prediction, which picks the backend;
    /// `ten_bit` the negotiated depth and `hdr` the colour verdict. NVENC blends any
    /// payload while its Vulkan slot device is up, the rule its `caps()` states; the slot
    /// device is predicted up. The AMD/Intel arm asks the rule the open takes, so the
    /// prediction names the encoder the session gets.
    ///
    /// [`CapturedFrame::cursor`]: pf_frame::CapturedFrame::cursor
    pub fn cursor_blend_capable(
        &self,
        codec: Codec,
        cuda_planned: bool,
        ten_bit: bool,
        hdr: bool,
    ) -> bool {
        // Negotiated PyroWave is selected before the pref; its CSC composites the cursor.
        if codec == Codec::PyroWave {
            return true;
        }
        let backend =
            resolve_linux_backend(&self.inputs.pref, || self.auto_is_vaapi(), cuda_planned);
        // Compute-CSC arm (the one that blends). Probe last: it opens a Vulkan instance.
        let vulkan_csc = self.inputs.features.vulkan_encode
            && if matches!(backend, Some(LinuxBackend::AmdIntel)) {
                self.amd_intel_opens_vulkan(codec, ten_bit, hdr)
            } else {
                // An explicit Vulkan pref opens Vulkan at the negotiated depth.
                matches!(codec, Codec::H265 | Codec::Av1)
                    && self.inputs.vulkan_row
                    && self.vulkan_encode_available_at(codec, ten_bit)
            };
        cursor_blend_capable_for(backend, self.inputs.features.nvenc, vulkan_csc)
    }

    /// Tiled dmabuf modifiers the session's encoder lane proved it can import for
    /// the capture `fourcc` — what the gamescope producer's tiled offer narrows to.
    /// Empty means LINEAR-only. Each lane returns its own proved list: Vulkan
    /// Video answers come from the packed-usage probes, VAAPI answers from
    /// `Display::import_dmabuf` + VPP, never one lane filtered by the other.
    pub fn capture_modifiers(
        &self,
        codec: Codec,
        fourcc: u32,
        bit_depth: u8,
        hdr: bool,
    ) -> Vec<u64> {
        #[cfg(feature = "pyrowave")]
        if codec == Codec::PyroWave {
            return pyrowave_capture_modifiers(fourcc);
        }
        // A build without PyroWave has no module to ask; advertise LINEAR.
        #[cfg(not(feature = "pyrowave"))]
        if codec == Codec::PyroWave {
            return Vec::new();
        }
        // The rule the open takes, so the capture answers come from the encoder that opens.
        #[cfg(feature = "vulkan-encode")]
        if self.amd_intel_opens_vulkan(codec, bit_depth >= 10, hdr) {
            return vulkan_video::vulkan_capture_modifiers(codec, fourcc, bit_depth >= 10);
        }
        #[cfg(not(feature = "vulkan-encode"))]
        let _ = (bit_depth, hdr);
        let candidates = vaapi_native::capture_candidates(fourcc);
        vaapi_native::vaapi_capture_modifiers(fourcc, &candidates)
    }

    /// True if the Linux GPU backend is VAAPI rather than NVENC — so capture picks dmabuf
    /// passthrough vs EGL→CUDA. Mirrors [`crate::open_video`].
    pub fn zero_copy_is_vaapi(&self) -> bool {
        match self.backend() {
            LinuxBackend::Nvenc => false,
            LinuxBackend::AmdIntel => true,
            // Preserved `_` fallthrough, not endorsed. Vulkan-on-NVIDIA is a known
            // mismatch (EGL→CUDA for a dmabuf importer); software is latent (H.264).
            LinuxBackend::Vulkan | LinuxBackend::Software => self.auto_is_vaapi(),
        }
    }

    /// The backend every capability answer reads, resolved once.
    fn backend(&self) -> LinuxBackend {
        *self
            .probes
            .backend
            .get_or_init(|| self.linux_resolved_backend())
    }

    /// The resolver with an unknown pref answering as `auto`, and `cuda = false`: these answer
    /// pre-session questions.
    fn linux_resolved_backend(&self) -> LinuxBackend {
        resolve_linux_backend(&self.inputs.pref, || self.auto_is_vaapi(), false).unwrap_or_else(
            || {
                if self.auto_is_vaapi() {
                    LinuxBackend::AmdIntel
                } else {
                    LinuxBackend::Nvenc
                }
            },
        )
    }

    /// `auto`'s answer. A build without `nvenc` carries no NVIDIA arm at all, so it always
    /// answers VAAPI — whose Vulkan Video leg the NVIDIA driver serves too.
    fn auto_is_vaapi(&self) -> bool {
        !self.inputs.features.nvenc
            || *self
                .probes
                .auto_is_vaapi
                .get_or_init(|| linux_auto_is_vaapi(self.inputs.nvidia))
    }

    /// The direct SDK's answers, probed once per process by the backend.
    fn nvenc(&self) -> NvencFacts {
        *self.probes.nvenc.get_or_init(probe_nvenc)
    }

    /// Whether the open's AMD/Intel arm tries Vulkan Video. The open, the cursor prediction and
    /// the capture modifiers all read it.
    fn amd_intel_opens_vulkan(&self, codec: Codec, ten_bit: bool, hdr: bool) -> bool {
        self.inputs.features.vulkan_encode
            && amd_intel_vulkan_depth(codec, ten_bit, hdr).is_some_and(|ten| {
                self.inputs.vulkan_row && self.vulkan_encode_available_at(codec, ten)
            })
    }

    /// Vulkan Video is built, switched on and the AMD/Intel plane's encoder for `codec`.
    fn vulkan_encodes(&self, codec: Codec) -> bool {
        self.inputs.features.vulkan_encode
            && self.inputs.vulkan_row
            && matches!(codec, Codec::H265 | Codec::Av1)
            && matches!(
                self.backend(),
                LinuxBackend::AmdIntel | LinuxBackend::Vulkan
            )
    }

    /// Vulkan Video encode caps for `codec`. The probe opens its own Vulkan instance; callers
    /// check the feature and the row first.
    fn vulkan(&self, codec: Codec) -> VulkanFacts {
        match codec_slot(codec) {
            Some(i) => *self.probes.vulkan[i].get_or_init(|| probe_vulkan_encode_caps(codec)),
            None => VulkanFacts::default(),
        }
    }

    /// Can this GPU open a Vulkan Video encode session for `codec`? Only
    /// [`Self::native_nv12_ok`] asks (no fallback there).
    fn vulkan_encode_available(&self, codec: Codec) -> bool {
        self.vulkan(codec).supported
    }

    /// Can Vulkan Video encode `codec` at this depth on the selected GPU?
    fn vulkan_encode_available_at(&self, codec: Codec, ten_bit: bool) -> bool {
        let caps = self.vulkan(codec);
        caps.supported
            && if ten_bit {
                caps.ten_bit
            } else {
                caps.eight_bit
            }
    }

    /// Can Vulkan Video encode `codec` at 10-bit on the selected GPU, and is it enabled? Both
    /// 10-bit gates route here.
    fn vulkan_10bit_available(&self, codec: Codec) -> bool {
        self.inputs.features.vulkan_encode
            && self.inputs.vulkan_row
            && self.vulkan_encode_available_at(codec, true)
    }

    /// libva's 10-bit entrypoint for `codec` (HEVC Main10; nothing else).
    fn vaapi_ten_bit(&self, codec: Codec) -> bool {
        codec_slot(codec).is_some_and(|i| {
            *self.probes.vaapi_ten_bit[i]
                .get_or_init(|| vaapi_native::probe_can_encode(codec, true))
        })
    }

    /// What the AMD/Intel plane can encode: the native VAAPI session for H.264 and HEVC,
    /// Vulkan Video for HEVC and AV1 — the two arms the open tries, so nothing is advertised
    /// that would die at open. `None` while the probe finds nothing; it runs again next ask.
    fn vaapi_codec_support(&self) -> Option<CodecSupport> {
        probe_until_found(
            &self.probes.vaapi,
            &self.probes.vaapi_missed,
            "VAAPI",
            || self.probe_vaapi_codec_support(),
        )
    }

    /// The probe behind [`Self::vaapi_codec_support`].
    fn probe_vaapi_codec_support(&self) -> CodecSupport {
        // The same query the open makes, at the depth it opens with. AV1 has no
        // native VAAPI path at all, so this is the only thing that can say yes.
        let vulkan = |c| {
            self.inputs.features.vulkan_encode
                && self.inputs.vulkan_row
                && self.vulkan_encode_available_at(c, false)
        };
        let probe = |c| vaapi_native::probe_can_encode(c, false);
        let caps = CodecSupport {
            h264: probe(Codec::H264),
            h265: probe(Codec::H265) || vulkan(Codec::H265),
            av1: vulkan(Codec::Av1),
        };
        if caps.h264 || caps.h265 || caps.av1 {
            tracing::info!(
                h264 = caps.h264,
                h265 = caps.h265,
                av1 = caps.av1,
                "VAAPI encode capabilities probed"
            );
        }
        caps
    }

    /// The codec set a Linux host may advertise: the resolved backend's probe,
    /// narrowed to what that backend can open.
    fn advertised_codecs(&self) -> CodecSupport {
        let backend = self.backend();
        let probed = match backend {
            // openh264, and it never probes.
            LinuxBackend::Software => None,
            _ if self.zero_copy_is_vaapi() => self.vaapi_codec_support(),
            // Driver GUID list, like the VAAPI arm.
            LinuxBackend::Nvenc if self.inputs.features.nvenc => Some(self.nvenc().codecs),
            _ => None,
        };
        narrow_to_openable(backend, probed, self.inputs.features)
    }
}

/// The direct SDK's answers, logged once per process. Fail-open contract: see
/// `nvenc_cuda::probe_support`. Without `nvenc` nothing is probed.
fn probe_nvenc() -> NvencFacts {
    #[cfg(feature = "nvenc")]
    {
        static LOGGED: OnceLock<()> = OnceLock::new();
        let probed = nvenc_cuda::probe_support();
        LOGGED.get_or_init(|| {
            tracing::info!(
                h264 = probed.codecs.h264,
                h265 = probed.codecs.h265,
                av1 = probed.codecs.av1,
                hevc_444 = probed.hevc_444,
                "NVENC encode capabilities probed"
            );
        });
        NvencFacts {
            codecs: probed.codecs,
            hevc_444: probed.hevc_444,
            ten_bit: probed.ten_bit,
        }
    }
    #[cfg(not(feature = "nvenc"))]
    {
        let none = CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        };
        NvencFacts {
            codecs: none,
            hevc_444: false,
            ten_bit: none,
        }
    }
}

/// One Vulkan Video caps probe, one log line. Without `vulkan-encode` nothing is probed.
fn probe_vulkan_encode_caps(codec: Codec) -> VulkanFacts {
    #[cfg(feature = "vulkan-encode")]
    {
        let caps = vulkan_video::probe_encode_caps(codec);
        if caps.supported {
            tracing::info!(
                ?codec,
                eight_bit = caps.eight_bit,
                ten_bit = caps.ten_bit,
                efc = ?caps.efc,
                vcn_firmware = %caps.vcn.map(|v| v.to_string()).unwrap_or_default(),
                "Vulkan Video encode probed — producer-native NV12 capture is eligible, and a \
                 10-bit session keeps this backend (real RFI + the compute CSC's cursor blend) \
                 where the device accepts the profile"
            );
        } else {
            tracing::info!(
                ?codec,
                "Vulkan Video encode unavailable (no encode queue for this codec) — keeping the \
                 packed-RGB capture negotiation (the native-NV12 path has no VAAPI fallback)"
            );
        }
        VulkanFacts {
            supported: caps.supported,
            eight_bit: caps.eight_bit,
            ten_bit: caps.ten_bit,
            efc: caps.efc,
            vcn: caps.vcn,
        }
    }
    #[cfg(not(feature = "vulkan-encode"))]
    {
        let _ = codec;
        VulkanFacts::default()
    }
}

/// Dispatch-mirroring core of [`Resolved::cursor_blend_capable`], device-free for tests.
/// `direct_nvenc` / `vulkan_csc` are the blend-capable arms, already gated.
fn cursor_blend_capable_for(
    backend: Option<LinuxBackend>,
    direct_nvenc: bool,
    vulkan_csc: bool,
) -> bool {
    match backend {
        // Direct-SDK only (VkSlotBlend). A CUDA buffer and an uploaded CPU frame land in
        // the same slot, and the blend runs on the slot.
        Some(LinuxBackend::Nvenc) => direct_nvenc,
        // Compute-CSC blends at either depth. Cursor sessions stay off native-NV12
        // / RGB-direct, so CSC eligibility (already carrying depth) is the answer.
        Some(LinuxBackend::AmdIntel) | Some(LinuxBackend::Vulkan) => vulkan_csc,
        // Capturer may composite inline; the encoder does not. Report encoder truth.
        Some(LinuxBackend::Software) | None => false,
    }
}

/// The depth the AMD/Intel arm asks Vulkan Video for, or `None` when it goes straight to VAAPI.
/// HEVC and AV1 take Vulkan at the negotiated depth, HDR or SDR alike: 10-bit SDR is HEVC Main10
/// or AV1 10-bit under BT.709 through `rgb2yuv10_709.comp`. H.264 has no Vulkan path. The
/// colour verdict rides along for the callers that already carry it; the depth is the rule.
fn amd_intel_vulkan_depth(codec: Codec, ten_bit: bool, _hdr: bool) -> Option<bool> {
    match codec {
        Codec::H265 | Codec::Av1 => Some(ten_bit),
        _ => None,
    }
}

/// The `auto` decision on a build with `nvenc`: a manual GPU picks its vendor's backend (NVIDIA
/// still needs its device nodes, `nvidia`); else NVIDIA's presence. Resolves **`auto` only** —
/// [`Resolved::zero_copy_is_vaapi`] layers the Encoder row on top.
fn linux_auto_is_vaapi(nvidia: bool) -> bool {
    if let Some(g) = pf_gpu::manual_selection() {
        if g.vendor_id == pf_gpu::VENDOR_NVIDIA {
            return !nvidia;
        }
        return true;
    }
    !nvidia
}

/// Resolved Linux encode backend. One alias table for the open and every capability answer.
/// Labels come from the open sites — this picks which arm runs, not what actually opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LinuxBackend {
    Nvenc,
    AmdIntel,
    Vulkan,
    Software,
}

/// Pure core. `None` = unknown pref: the open bails, capability answers map it to auto.
/// `auto_is_vaapi` is lazy — `/serverinfo` polls these answers; explicit prefs must not probe.
/// `pyrowave` names a codec, not a backend: negotiation reads it, and H.26x resolves as `auto`.
fn resolve_linux_backend(
    pref: &str,
    auto_is_vaapi: impl FnOnce() -> bool,
    cuda: bool,
) -> Option<LinuxBackend> {
    Some(match pref {
        "nvenc" | "nvidia" | "cuda" => LinuxBackend::Nvenc,
        "vaapi" | "amd" | "intel" | "vaapi-native" => LinuxBackend::AmdIntel,
        "vulkan" | "vulkan-video" => LinuxBackend::Vulkan,
        "software" | "sw" | "openh264" => LinuxBackend::Software,
        // A CUDA frame can only be consumed by NVENC; else [`linux_auto_is_vaapi`].
        "auto" | "" | "pyrowave" => {
            if cuda || !auto_is_vaapi() {
                LinuxBackend::Nvenc
            } else {
                LinuxBackend::AmdIntel
            }
        }
        _ => return None,
    })
}

/// Dmabuf modifiers PyroWave's Vulkan device imports for the capture fourcc.
/// VAAPI LINEAR-only starves tiled Mutter+NVIDIA allocations.
#[cfg(feature = "pyrowave")]
pub(crate) fn pyrowave_capture_modifiers(fourcc: u32) -> Vec<u64> {
    pyrowave::capture_modifiers(fourcc)
}

/// A probe narrowed by what `backend` can open in a build with `features`. An all-`false`
/// probe means the GPU was unusable at probe time, not that it encodes nothing, so it fails
/// open to the superset — narrowing can then only take codecs away. Pure, so the ceilings are
/// testable without a GPU.
fn narrow_to_openable(
    backend: LinuxBackend,
    probed: Option<CodecSupport>,
    features: Features,
) -> CodecSupport {
    // A forced-vulkan pref is a ceiling, never a replacement: the arm encodes
    // HEVC/AV1 only (H.264 dies at open), and only in a build that has it.
    // A static HEVC|AV1 would add AV1 on GPUs whose probe withholds it.
    let ceiling = match backend {
        LinuxBackend::Software => CodecSupport {
            h264: true,
            h265: false,
            av1: false,
        },
        // The resolver knows the pref is vulkan; only the build's features know it can
        // open it. Else: advertise-then-die-at-open.
        LinuxBackend::Vulkan => {
            let built = features.vulkan_encode;
            CodecSupport {
                h264: false,
                h265: built,
                av1: built,
            }
        }
        // An nvenc pin likewise: its probe arm is compiled out with the feature.
        LinuxBackend::Nvenc => {
            let built = features.nvenc;
            CodecSupport {
                h264: built,
                h265: built,
                av1: built,
            }
        }
        _ => CodecSupport {
            h264: true,
            h265: true,
            av1: true,
        },
    };
    let caps = probed
        .filter(|c| c.h264 || c.h265 || c.av1)
        .unwrap_or(ceiling);
    CodecSupport {
        h264: caps.h264 && ceiling.h264,
        h265: caps.h265 && ceiling.h265,
        av1: caps.av1 && ceiling.av1,
    }
}

/// Open the software H.264 encoder by name, bypassing the backend ladder.
///
/// [`crate::open_video`] opens the backend [`Resolved`] names from `PUNKTFUNK_ENCODER`, and
/// `auto` never picks software. The browser plane decides later and wants this encoder
/// specifically: it serves a GPU-less host, at full range. Like [`crate::open_video`], it reads
/// the env knobs once per open.
pub fn open_software_h264(
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
) -> Result<Box<dyn Encoder>> {
    sw::OpenH264Encoder::open(
        format,
        width,
        height,
        fps,
        bitrate_bps.min(SW_BITRATE_CEIL),
        true,
        &Knobs::from_env(),
    )
    .map(|e| Box::new(e) as Box<dyn Encoder>)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolution of `pref` on no selected GPU, no NVIDIA, the Vulkan row on, this build.
    fn resolved(pref: &str) -> Resolved {
        Resolved::new(ResolveInputs {
            pref: pref.into(),
            gpu: String::new(),
            vulkan_row: true,
            nvidia: false,
            features: Features::BUILT,
        })
    }

    #[test]
    fn the_advertisement_never_offers_what_the_backend_cannot_open() {
        use LinuxBackend::*;
        let caps = |h264, h265, av1| CodecSupport { h264, h265, av1 };
        let built = Features::BUILT;
        let probed = caps(true, true, false);
        // A GPU probe passes through on the vendor backends.
        assert_eq!(
            (
                narrow_to_openable(AmdIntel, Some(probed), built).h264,
                narrow_to_openable(AmdIntel, Some(probed), built).av1
            ),
            (true, false)
        );
        // A vulkan pin drops H.264 (it bails at open) and keeps what the probe found.
        let vulkan = narrow_to_openable(Vulkan, Some(caps(true, true, true)), built);
        assert_eq!(
            (vulkan.h264, vulkan.h265, vulkan.av1),
            (
                false,
                cfg!(feature = "vulkan-encode"),
                cfg!(feature = "vulkan-encode")
            )
        );
        // An nvenc pin advertises only what this build can open.
        let nvenc = cfg!(feature = "nvenc");
        let nv = narrow_to_openable(Nvenc, None, built);
        assert_eq!((nv.h264, nv.h265, nv.av1), (nvenc, nvenc, nvenc));
        // openh264 is H.264 whatever a GPU probe says.
        let sw = narrow_to_openable(Software, Some(caps(true, true, true)), built);
        assert_eq!((sw.h264, sw.h265, sw.av1), (true, false, false));
        // An unusable probe fails open to the superset, still narrowed.
        let unprobed = narrow_to_openable(AmdIntel, None, built);
        assert!(unprobed.h264 && unprobed.h265 && unprobed.av1);
        let empty = narrow_to_openable(AmdIntel, Some(caps(false, false, false)), built);
        assert!(empty.h264 && empty.h265 && empty.av1);
    }

    #[test]
    fn cursor_blend_capability_mirrors_the_dispatch() {
        use LinuxBackend::*;
        assert!(
            cursor_blend_capable_for(Some(Nvenc), true, false),
            "a CUDA buffer or an uploaded CPU frame: both reach the slot blend"
        );
        assert!(
            !cursor_blend_capable_for(Some(Nvenc), false, false),
            "a build without the `nvenc` feature has no NVENC at all"
        );
        assert!(cursor_blend_capable_for(Some(AmdIntel), false, true));
        assert!(
            !cursor_blend_capable_for(Some(AmdIntel), false, false),
            "no eligible Vulkan CSC arm (H.264, PUNKTFUNK_VULKAN_ENCODE=0, unsupported \
             device) resolves to native VAAPI, which cannot blend"
        );
        assert!(cursor_blend_capable_for(Some(Vulkan), false, true));
        assert!(!cursor_blend_capable_for(Some(Software), true, true));
        assert!(!cursor_blend_capable_for(None, true, true));
    }

    #[test]
    fn amd_intel_hevc_takes_vulkan_at_either_depth() {
        assert_eq!(
            amd_intel_vulkan_depth(Codec::H265, true, false),
            Some(true),
            "HEVC 10-bit SDR opens Vulkan Main10 like AV1 — and predicts the blend it gets"
        );
        assert_eq!(amd_intel_vulkan_depth(Codec::H265, true, true), Some(true));
        assert_eq!(
            amd_intel_vulkan_depth(Codec::H265, false, false),
            Some(false)
        );
        assert_eq!(amd_intel_vulkan_depth(Codec::Av1, true, false), Some(true));
        assert_eq!(amd_intel_vulkan_depth(Codec::H264, false, false), None);
    }

    /// Resolver alias table. The panicking closure is the laziness contract:
    /// an explicit pref must not run the auto probe (`/serverinfo` polls).
    #[test]
    fn linux_backend_resolver_table() {
        use LinuxBackend::*;
        let no_probe = || -> bool { panic!("explicit prefs must not run the auto probe") };
        for pref in ["nvenc", "nvidia", "cuda"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(Nvenc));
        }
        for pref in ["vaapi", "amd", "intel"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(AmdIntel));
        }
        for pref in ["vulkan", "vulkan-video"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(Vulkan));
        }
        // A PyroWave pin never forces wavelet AUs onto an H.26x session: H.26x runs as `auto`.
        assert_eq!(
            resolve_linux_backend("pyrowave", || true, false),
            Some(AmdIntel)
        );
        assert_eq!(
            resolve_linux_backend("pyrowave", || false, false),
            Some(Nvenc)
        );
        for pref in ["software", "sw", "openh264"] {
            assert_eq!(resolve_linux_backend(pref, no_probe, false), Some(Software));
        }
        assert_eq!(resolve_linux_backend("", || true, false), Some(AmdIntel));
        assert_eq!(resolve_linux_backend("auto", || false, false), Some(Nvenc));
        // CUDA is NVENC-only and short-circuits the probe (`||` order).
        assert_eq!(resolve_linux_backend("auto", no_probe, true), Some(Nvenc));
        // Unknown pref: dispatch bails; mirrors map this to auto.
        assert_eq!(resolve_linux_backend("banana", no_probe, false), None);
        // Explicit pref is never overridden by `cuda`.
        assert_eq!(
            resolve_linux_backend("vaapi", no_probe, true),
            Some(AmdIntel)
        );
    }

    /// Native-planar depth parity: 8-bit SDR (NV12), HDR (P010) and a producer's own 10-bit
    /// SDR (P010) may take the native source; any other 10-bit SDR must not — it captures
    /// 8-bit packed RGB for the widening CSC and native NV12 cannot be a P010 source.
    #[test]
    fn native_planar_depth_matches_excludes_widened_ten_bit_sdr() {
        assert!(native_planar_depth_matches(8, false, false));
        assert!(native_planar_depth_matches(10, true, false));
        assert!(!native_planar_depth_matches(10, false, false));
        assert!(native_planar_depth_matches(10, false, true));
    }

    fn params(codec: Codec) -> OpenParams {
        OpenParams {
            codec,
            format: PixelFormat::Bgrx,
            width: 64,
            height: 64,
            fps: 30,
            bitrate_bps: 1_000_000,
            cuda: false,
            bit_depth: 8,
            hdr: false,
            chroma: ChromaFormat::Yuv420,
            cursor_blend: false,
            slices: 4,
            full_range: true,
        }
    }

    /// Linux dispatch through the resolver, GPU-free via the software arm.
    #[test]
    fn open_video_backend_dispatches_software() {
        let sw = resolved("software");
        let knobs = Knobs::from_env();
        let (enc, label) =
            open(&sw, &params(Codec::H264), &knobs).expect("software arm must open GPU-free");
        assert_eq!(label, "software");
        drop(enc);
        let err = match open(&sw, &params(Codec::H265), &knobs) {
            // `expect_err` needs `Ok: Debug`; `Box<dyn Encoder>` isn't.
            Ok(_) => panic!("software emits H.264 only; an H.265 session must be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("H.264"), "{err:#}");
    }

    /// A `vulkan` pin the GPU can't open falls back to VAAPI rather than failing the session.
    /// GPU-free: without a device the error must be VAAPI's, not Vulkan's.
    #[cfg(feature = "vulkan-encode")]
    #[test]
    fn a_vulkan_pin_falls_back_to_vaapi() {
        match open(
            &resolved("vulkan"),
            &params(Codec::H265),
            &Knobs::from_env(),
        ) {
            Ok((_, label)) => assert!(matches!(label, "vulkan" | "vaapi-native"), "{label}"),
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("libva") || msg.contains("VAAPI"), "{msg}");
            }
        }
    }

    /// `auto` on an AMD/Intel box opens the native VAAPI session.
    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn auto_opens_the_native_vaapi_session() {
        let p = OpenParams {
            format: PixelFormat::Bgra,
            width: 320,
            height: 240,
            fps: 60,
            bitrate_bps: 2_000_000,
            ..params(Codec::H264)
        };
        let auto = Resolved::new(ResolveInputs {
            nvidia: pf_gpu::nvidia_present(),
            ..resolved("auto").inputs
        });
        let (enc, backend) = open(&auto, &p, &Knobs::from_env()).expect("open");
        assert_eq!(backend, "vaapi-native");
        assert!(enc.caps().supports_rfi);
    }

    // The resolution table: hand-filled cells, no GPU, no config.

    const ALL: CodecSupport = CodecSupport {
        h264: true,
        h265: true,
        av1: true,
    };

    /// Every encode feature on, so a row can turn one off.
    const FULL: Features = Features {
        nvenc: true,
        vulkan_encode: true,
        pyrowave: true,
        qsv: false,
    };

    fn inputs(pref: &str, features: Features) -> ResolveInputs {
        ResolveInputs {
            pref: pref.into(),
            gpu: "card0".into(),
            vulkan_row: true,
            nvidia: false,
            features,
        }
    }

    fn fill<T: std::fmt::Debug>(cell: &OnceLock<T>, v: T) {
        cell.set(v).expect("a row fills each cell once");
    }

    fn vulkan(eight_bit: bool, ten_bit: bool) -> VulkanFacts {
        VulkanFacts {
            supported: eight_bit || ten_bit,
            eight_bit,
            ten_bit,
            ..VulkanFacts::default()
        }
    }

    fn nvenc(hevc_444: bool, ten_bit: bool) -> NvencFacts {
        NvencFacts {
            codecs: ALL,
            hevc_444,
            ten_bit: CodecSupport {
                h264: false,
                h265: ten_bit,
                av1: ten_bit,
            },
        }
    }

    /// `/serverinfo` polls with the Encoder row as set: an explicit pref answers every question
    /// without the `auto` probe, even on a box with NVIDIA's nodes.
    #[test]
    fn an_explicit_pref_never_runs_the_auto_probe() {
        for pref in ["vaapi", "nvenc"] {
            let r = Resolved::new(ResolveInputs {
                nvidia: true,
                ..inputs(pref, FULL)
            });
            fill(&r.probes.vaapi, ALL);
            fill(&r.probes.nvenc, nvenc(false, false));
            let _ = (r.wire_caps(), r.is_gpu(), r.zero_copy_is_vaapi());
            assert!(r.probes.auto_is_vaapi.get().is_none(), "{pref}");
        }
        let vaapi_on_nvidia = Resolved::new(ResolveInputs {
            nvidia: true,
            ..inputs("vaapi", FULL)
        });
        assert!(vaapi_on_nvidia.zero_copy_is_vaapi() && !vaapi_on_nvidia.hdr_cuda_ok());
    }

    /// `auto` takes NVENC where NVIDIA drives it, and only in a build with the direct SDK; a
    /// build without it never asks.
    #[test]
    fn auto_takes_nvenc_only_with_the_direct_sdk() {
        let nvidia = Resolved::new(inputs("auto", FULL));
        fill(&nvidia.probes.auto_is_vaapi, false);
        assert!(!nvidia.zero_copy_is_vaapi() && nvidia.hdr_cuda_ok());
        let amd = Resolved::new(inputs("auto", FULL));
        fill(&amd.probes.auto_is_vaapi, true);
        assert!(amd.zero_copy_is_vaapi());
        let no_sdk = Resolved::new(inputs(
            "auto",
            Features {
                nvenc: false,
                ..FULL
            },
        ));
        assert!(no_sdk.zero_copy_is_vaapi() && !no_sdk.hdr_cuda_ok());
        assert!(no_sdk.probes.auto_is_vaapi.get().is_none());
    }

    /// An unknown pref answers as `auto` for the advertisement and still fails at open.
    #[test]
    fn an_unknown_pref_answers_auto_and_fails_the_open() {
        let r = Resolved::new(inputs("banana", FULL));
        fill(&r.probes.auto_is_vaapi, true);
        assert!(r.zero_copy_is_vaapi() && r.is_gpu());
        let err = match open(&r, &params(Codec::H264), &Knobs::from_env()) {
            Ok(_) => panic!("an unknown pref must not open"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("unknown PUNKTFUNK_ENCODER"),
            "{err:#}"
        );
    }

    /// A pin advertises only what this build can open: no H.26x for a `vulkan` pin without
    /// Vulkan Video, nothing for an `nvenc` pin without the direct SDK (sweep R5-04).
    #[test]
    fn a_pin_advertises_only_what_the_build_opens() {
        let codecs = punktfunk_core::quic::CODEC_H264
            | punktfunk_core::quic::CODEC_HEVC
            | punktfunk_core::quic::CODEC_AV1;
        let vulkan_pin = Resolved::new(inputs(
            "vulkan",
            Features {
                vulkan_encode: false,
                ..FULL
            },
        ));
        fill(&vulkan_pin.probes.auto_is_vaapi, true);
        fill(&vulkan_pin.probes.vaapi, ALL);
        assert_eq!(vulkan_pin.wire_caps() & codecs, 0);
        let nvenc_pin = Resolved::new(inputs(
            "nvenc",
            Features {
                nvenc: false,
                pyrowave: false,
                ..FULL
            },
        ));
        assert_eq!(nvenc_pin.wire_caps(), 0);
        let with_pyrowave = Resolved::new(inputs(
            "nvenc",
            Features {
                nvenc: false,
                ..FULL
            },
        ));
        assert_eq!(
            with_pyrowave.wire_caps(),
            punktfunk_core::quic::CODEC_PYROWAVE
        );
    }

    /// A `software` pin is H.264 on the CPU, and never offers PyroWave.
    #[test]
    fn a_software_pin_is_h264_on_the_cpu() {
        let r = Resolved::new(inputs("software", FULL));
        assert_eq!(r.wire_caps(), punktfunk_core::quic::CODEC_H264);
        assert!(!r.is_gpu());
    }

    /// The Encoder row keys the resolution: a 4:4:4 verdict probed under `nvenc` never answers
    /// for `vaapi` on the same GPU (sweep R5-01).
    #[test]
    fn an_encoder_row_flip_resolves_afresh() {
        let cache = Mutex::new(HashMap::new());
        let nv = resolve_in(&cache, inputs("nvenc", FULL));
        fill(&nv.probes.nvenc, nvenc(true, true));
        assert!(nv.can_encode_444(Codec::H265));
        let va = resolve_in(&cache, inputs("vaapi", FULL));
        assert!(!Arc::ptr_eq(&nv, &va));
        assert!(!va.can_encode_444(Codec::H265));
        assert!(Arc::ptr_eq(&nv, &resolve_in(&cache, inputs("nvenc", FULL))));
    }

    /// The Vulkan row decides what AMD/Intel opens: with it HEVC 10-bit and AV1 native NV12
    /// come from Vulkan Video, without it libva answers alone (10-02).
    #[test]
    fn the_vulkan_row_decides_the_amd_answers() {
        let cache = Mutex::new(HashMap::new());
        let on = resolve_in(&cache, inputs("vaapi", FULL));
        let off = resolve_in(
            &cache,
            ResolveInputs {
                vulkan_row: false,
                ..inputs("vaapi", FULL)
            },
        );
        assert!(!Arc::ptr_eq(&on, &off));
        for r in [&on, &off] {
            fill(&r.probes.vulkan[0], vulkan(true, true));
            fill(&r.probes.vulkan[1], vulkan(true, false));
            fill(&r.probes.vaapi_ten_bit[0], false);
        }
        assert!(on.can_encode_10bit(Codec::H265) && !off.can_encode_10bit(Codec::H265));
        assert!(on.native_nv12_ok(Codec::Av1, 8, false, false, false));
        assert!(!off.native_nv12_ok(Codec::Av1, 8, false, false, false));
        assert!(off.native_nv12_ok(Codec::H265, 8, false, false, false));
    }

    /// Which backend writes 10-bit from an 8-bit SDR surface.
    #[test]
    fn the_sdr10_carry_follows_the_backend() {
        let amd = Resolved::new(inputs("vaapi", FULL));
        fill(&amd.probes.vulkan[1], vulkan(true, false));
        assert!(amd.carries_sdr10(Codec::H265));
        assert!(
            !amd.carries_sdr10(Codec::Av1),
            "AV1 needs Vulkan's 10-bit profile"
        );
        let amd_av1 = Resolved::new(inputs("vaapi", FULL));
        fill(&amd_av1.probes.vulkan[1], vulkan(true, true));
        assert!(amd_av1.carries_sdr10(Codec::Av1));
        assert!(Resolved::new(inputs("nvenc", FULL)).carries_sdr10(Codec::H265));
        let no_sdk = Features {
            nvenc: false,
            ..FULL
        };
        assert!(!Resolved::new(inputs("nvenc", no_sdk)).carries_sdr10(Codec::H265));
        let vulkan_pin = Resolved::new(inputs("vulkan", FULL));
        fill(&vulkan_pin.probes.vulkan[0], vulkan(true, true));
        assert!(vulkan_pin.carries_sdr10(Codec::H265) && !vulkan_pin.carries_sdr10(Codec::H264));
        assert!(!Resolved::new(inputs("software", FULL)).carries_sdr10(Codec::H265));
        assert!(Resolved::new(inputs("software", FULL)).carries_sdr10(Codec::PyroWave));
        let no_pyrowave = Features {
            pyrowave: false,
            ..FULL
        };
        assert!(!Resolved::new(inputs("software", no_pyrowave)).carries_sdr10(Codec::PyroWave));
    }

    /// 4:4:4 is NVENC's cap on HEVC, PyroWave's always, nobody's on H.264 or AMD/Intel.
    #[test]
    fn the_444_verdict_follows_the_backend() {
        let nv = Resolved::new(inputs("nvenc", FULL));
        fill(&nv.probes.nvenc, nvenc(true, false));
        assert!(nv.can_encode_444(Codec::H265) && !nv.can_encode_444(Codec::H264));
        assert!(nv.can_encode_444(Codec::PyroWave));
        let no_cap = Resolved::new(inputs("nvenc", FULL));
        fill(&no_cap.probes.nvenc, nvenc(false, false));
        assert!(!no_cap.can_encode_444(Codec::H265));
        let no_sdk = Features {
            nvenc: false,
            ..FULL
        };
        assert!(!Resolved::new(inputs("nvenc", no_sdk)).can_encode_444(Codec::H265));
        assert!(!Resolved::new(inputs("vaapi", FULL)).can_encode_444(Codec::H265));
    }
}
