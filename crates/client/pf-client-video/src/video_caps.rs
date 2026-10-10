//! What this client can decode and promise in the Hello. [`native_evidence`] and
//! [`native_rung_admitted`] decide which rung `auto` may put first; the codec, 10-bit,
//! 4:4:4, HDR and multi-slice gates decide what is advertised, and
//! [`last_rung_verdict`] picks the retry when the last rung has no decoder.
//! `video` re-exports every item, so call sites keep `video::` paths; the ladder that
//! acts on these answers stays there. Evidence: tests in this file.

use crate::video::{
    native_codec, VulkanDecodeDevice, VIDEO_CODEC_OP_DECODE_AV1, VIDEO_CODEC_OP_DECODE_H265,
};

/// May `auto` enter the VAAPI rung with this presenter? The selected device
/// must import dma-bufs and must not be NVIDIA. The `native-vaapi` pin bypasses
/// this gate; `None` preserves non-presenter callers.
#[cfg(target_os = "linux")]
pub(crate) fn vaapi_auto_ok(vk: Option<&VulkanDecodeDevice>) -> bool {
    vk.is_none_or(|v| v.dmabuf_import && v.vendor_id != crate::video_vk::VENDOR_NVIDIA)
}

/// May a V4L2 picture reach this presenter? Its buffers are dma-bufs, so the
/// selected device must import them. `None` preserves non-presenter callers.
#[cfg(target_os = "linux")]
pub(crate) fn v4l2_auto_ok(vk: Option<&VulkanDecodeDevice>) -> bool {
    vk.is_none_or(|v| v.dmabuf_import)
}

/// V4L2 decode on this machine as `quic::CODEC_*` masks. Empty off Linux.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct V4l2Summary {
    /// Codecs a decoder node takes and returns as an importable picture.
    pub codecs: u8,
    /// Subset of `codecs` with a linear 10-bit picture format.
    pub ten_bit: u8,
}

/// What the V4L2 rung adds for this presenter. A device with Vulkan Video
/// never needs it, so its video nodes (cameras among them) stay unopened.
fn v4l2_summary(vk: Option<&VulkanDecodeDevice>) -> V4l2Summary {
    #[cfg(target_os = "linux")]
    if v4l2_auto_ok(vk) && !vk.is_some_and(|v| v.video_decode) {
        return crate::video_v4l2::caps().summary();
    }
    let _ = vk;
    V4l2Summary::default()
}

/// One decode rung, named so evidence and admission can talk without a
/// per-platform [`Backend`](crate::video::Backend) variant. The CPU rung is in the
/// table only ([`native_evidence`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeRung {
    /// pf-vkdecode on the presenter's device (`video_vk_native`).
    Vulkan,
    /// pf-dxvadec driving `ID3D11VideoDecoder` (`video_d3d11_native`, Windows).
    D3d11va,
    /// pf-vaapi driving a dlopen'd libva (`video_vaapi_native`, Linux).
    Vaapi,
    /// A V4L2 decoder node, stateful or stateless (`video_v4l2`, Linux).
    V4l2,
    /// openh264 + rav1d (`video_software`).
    Software,
}

impl NativeRung {
    /// Log / `PUNKTFUNK_DECODER` name — the same strings as the `stats:` decode-path tag.
    pub fn name(self) -> &'static str {
        match self {
            NativeRung::Vulkan => "native-vulkan",
            NativeRung::D3d11va => "native-d3d11va",
            NativeRung::Vaapi => "native-vaapi",
            NativeRung::V4l2 => "native-v4l2",
            NativeRung::Software => "software",
        }
    }
}

/// Evidence for automatic rung ordering, keyed by rung and wire codec.
/// `verified` means the recorded coverage meets this project's priority bar;
/// the note names both the evidence and what it still lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RungEvidence {
    /// Whether `auto` may prioritize this pair over a usable rung below it.
    pub verified: bool,
    /// Hardware coverage and its remaining gap, emitted verbatim in the session log.
    pub note: &'static str,
}

/// Evidence table for automatic priority. An unknown pair is `verified: false` —
/// a new codec leg must not inherit a neighbour's confidence.
pub fn native_evidence(rung: NativeRung, wire: u8) -> RungEvidence {
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};
    let (verified, note) = match (rung, wire) {
        (NativeRung::Vulkan, CODEC_H264) => (
            true,
            "bit-exact vs libavcodec, 250/250 AUs on three drivers + 92-min soak (M2 WP-D)",
        ),
        (NativeRung::Vulkan, CODEC_HEVC) => (
            true,
            "bit-exact vs libavcodec incl. Main10/4:4:4, three drivers + HDR and Deck legs (M3)",
        ),
        (NativeRung::Vulkan, CODEC_AV1) => (
            true,
            "250/250 bit-identical to libavcodec on an RTX 5070 Ti (M7) - one vendor, no soak",
        ),
        (NativeRung::D3d11va, CODEC_H264 | CODEC_HEVC) => (
            true,
            "frame-hash parity on an RTX 4090 and an AMD iGPU + 30-min soak (M5)",
        ),
        // Decode-target must not alias a referenced surface (pf-dxvadec regression test).
        (NativeRung::D3d11va, CODEC_AV1) => (
            true,
            "250/250 delivered frames bit-identical to libavcodec on an RTX 3500 Ada AND an \
             Intel Arc (2026-08-07), after fixing a decode target that aliased a reference \
             surface on 268 of 274 frames - two vendors, no soak (M7)",
        ),
        // Keep `verified` false: flipping it would move `auto` off Vulkan Video on every Linux AMD/Intel client.
        (NativeRung::Vaapi, _) => (
            false,
            "7 legs bit-identical to libavcodec on RDNA3 (Mesa 26.0.3, 2026-08-08) and Intel \
             Xe-LP (iHD 26.1.2, 2026-09-24) - H.264, H.265, HEVC Main 10 and AV1, on both the \
             conformance vectors and our own host's low-delay streams - but never soaked, and \
             `verified` here would move `auto` off Vulkan Video on every Linux AMD/Intel \
             client (M6/M7)",
        ),
        // rav1d uses two frame contexts so a damaged reference returns an error instead of aborting.
        (NativeRung::Software, CODEC_H264 | CODEC_AV1) => (
            false,
            "openh264 has never run on glass; rav1d decodes 1080p and 4K60 AV1 there and \
             survives a mid-stream reference loss, but has no parity check or soak (M8)",
        ),
        _ => (false, "no hardware run recorded for this rung and codec"),
    };
    RungEvidence { verified, note }
}

/// Can this device run native Vulkan for this wire codec?
///
/// [`native_vulkan_gate`] without its `choice` half. Callers that ask what is
/// below them share this so "Vulkan is available here" cannot mean two things.
pub fn native_vulkan_usable(wire: u8, video_decode: bool, decode_video_caps: u32) -> bool {
    native_vulkan_gate("auto", wire, video_decode, decode_video_caps)
}

/// Whether `auto` may pick this native rung for this codec and device.
///
/// A verified rung is always admitted. An unverified rung yields only when
/// `below` is usable and verified for the same codec; yielding to software for
/// lack of evidence would discard hardware. Pins and demotion bypass this.
pub fn native_rung_admitted(rung: NativeRung, wire: u8, below: Option<NativeRung>) -> bool {
    native_evidence(rung, wire).verified
        || !below.is_some_and(|b| native_evidence(b, wire).verified)
}

/// Native Vulkan admission: `choice` is `native-vulkan` or the auto family
/// (`auto` / `` / `hardware`), the wire codec is one pf-vkdecode speaks
/// ([`native_codec`]), and the decode family advertises that codec op.
/// `video_decode` proves the extension stack, never the codec.
///
/// Other-backend pins refuse here. Init failure falls through, so admission
/// cannot cost the session its decoder. Stream shape is probed at
/// [`NativeVulkanDecoder::new`](crate::video_vk_native::NativeVulkanDecoder::new), not
/// here.
pub(crate) fn native_vulkan_gate(
    choice: &str,
    wire: u8,
    video_decode: bool,
    decode_video_caps: u32,
) -> bool {
    let Some((_, codec_op)) = native_codec(wire) else {
        return false;
    };
    let chosen = matches!(choice, "native-vulkan" | "auto" | "" | "hardware");
    chosen && video_decode && decode_video_caps & codec_op != 0
}

/// Human name of a `quic::CODEC_*` bit. `?` for an unknown bit — it must not print as a known codec.
pub fn wire_codec_name(wire: u8) -> &'static str {
    match wire {
        punktfunk_core::quic::CODEC_H264 => "H.264",
        punktfunk_core::quic::CODEC_HEVC => "HEVC",
        punktfunk_core::quic::CODEC_AV1 => "AV1",
        punktfunk_core::quic::CODEC_PYROWAVE => "PyroWave",
        _ => "?",
    }
}

/// `quic` codec bits this build can decode on the CPU — the last rung, so
/// the set a session is guaranteed to finish. Shared so the software map and
/// [`last_rung_verdict`] cannot drift.
pub fn software_decodable_codecs() -> u8 {
    punktfunk_core::quic::CODEC_H264 | punktfunk_core::quic::CODEC_AV1
}

/// Reconnect action when the last rung has no decoder for this codec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LastRungVerdict {
    /// Reconnect advertising these caps. Non-empty and excluding the exhausted codec, so the host must pick something else.
    Retry { caps: u8 },
    /// Nothing left to advertise. Reconnecting would negotiate the same dead end.
    Dead,
}

/// Why the last rung had no answer. [`last_rung_verdict`] needs more than "a codec failed".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RungLoss {
    /// This codec has no CPU rung (HEVC). Hardware already failed; retry may only offer codecs that have a CPU rung.
    Codec,
    /// The codec has a CPU rung; this picture shape is outside it (10-bit, 4:4:4).
    /// Do not filter by [`software_decodable_codecs`]: that would drop HEVC a shape retry could finish.
    Shape,
}

/// Reconnect rule when the last rung has no decoder. The codec is fixed at
/// Welcome, so the lever is the next Hello. Drops `negotiated`; for
/// [`RungLoss::Codec`] also drops codecs with no CPU rung. `caps` is what
/// the retry advertises, so the pump's `exclude_codecs` cannot disagree.
pub fn last_rung_verdict(negotiated: u8, advertised: u8, loss: RungLoss) -> LastRungVerdict {
    let survivors = advertised & !negotiated;
    let caps = match loss {
        RungLoss::Codec => survivors & software_decodable_codecs(),
        RungLoss::Shape => survivors,
    };
    // A retry the host cannot pick is not a retry: PyroWave is opt-in and
    // stays out of `resolve_codec`. Survivors that are PyroWave alone resolve
    // to nothing. Judge liveness on the pickable set; carry the rest along.
    const PICKABLE: u8 = punktfunk_core::quic::CODEC_H264
        | punktfunk_core::quic::CODEC_HEVC
        | punktfunk_core::quic::CODEC_AV1;
    if caps & PICKABLE == 0 {
        LastRungVerdict::Dead
    } else {
        LastRungVerdict::Retry { caps }
    }
}

/// Whether decode is pinned to the CPU rung (`PUNKTFUNK_DECODER` wins over Settings).
/// Same precedence as [`Decoder::new`](crate::video::Decoder::new): a second reading of
/// the same two inputs would drift.
pub fn decode_pinned_to_software(pref: &str) -> bool {
    resolve_decoder_pref(std::env::var("PUNKTFUNK_DECODER").ok().as_deref(), pref) == "software"
}

/// `PUNKTFUNK_DECODER` if it carries a value, else the stored setting. Pure
/// and shared so [`Decoder::new`](crate::video::Decoder::new) and
/// [`decode_pinned_to_software`] cannot drift.
///
/// Trimmed: a trailing space matched no [`native_vulkan_gate`] arm and fell
/// through to `auto`. Whitespace-only is absent, not a pin to `""` (the auto family).
pub(crate) fn resolve_decoder_pref(env: Option<&str>, pref: &str) -> String {
    env.map(str::trim)
        .filter(|v| !v.is_empty())
        .map_or_else(|| pref.to_string(), str::to_string)
}

/// `quic` codecs this build can decode. Advertised so the host never emits
/// one we cannot. Constants, not probes: asked before a device exists.
/// Device facts are [`decodable_codecs_for`].
///
/// AV1 here is a decoder existing, not one that can keep up — gated in
/// [`decodable_codecs_for`]. HEVC is hardware-only and still advertised:
/// refusing it up front would cost every working box to protect the few
/// that later fail. Exhaustion is [`last_rung_verdict`].
pub fn decodable_codecs() -> u8 {
    // Native Vulkan's three codecs union the CPU rung's. Written as a union so
    // removing a rung's codec leg drops the advertisement rather than keeping it.
    punktfunk_core::quic::CODEC_H264
        | punktfunk_core::quic::CODEC_HEVC
        | punktfunk_core::quic::CODEC_AV1
        | software_decodable_codecs()
}

/// PCI vendor of Intel GPUs.
pub(crate) const VENDOR_INTEL: u32 = 0x8086;

/// The decode ops the native Vulkan rung may use, from what the device advertises.
/// Intel's Mesa driver decodes H.264 and HEVC bit-exact with libavcodec but not AV1,
/// so Linux Intel keeps AV1 off Vulkan; its AV1 goes through the VAAPI rung.
pub fn usable_decode_ops(vendor_id: u32, advertised: u32) -> u32 {
    if cfg!(target_os = "linux") && vendor_id == VENDOR_INTEL {
        advertised & !VIDEO_CODEC_OP_DECODE_AV1
    } else {
        advertised
    }
}

/// Does the presenter's VAAPI node decode AV1, where its Vulkan does not? The presenter
/// asks once at setup. NVIDIA is never asked: the ladder never enters its VAAPI.
#[cfg(target_os = "linux")]
pub fn vaapi_av1_decodable(vendor_id: u32, vulkan_av1: bool) -> bool {
    !vulkan_av1
        && vendor_id != crate::video_vk::VENDOR_NVIDIA
        && crate::video_vaapi_native::av1_decodable(vendor_id)
}

/// Does the presenter's VAAPI node decode HEVC, where its Vulkan does not? Same
/// one-time question as [`vaapi_av1_decodable`].
#[cfg(target_os = "linux")]
pub fn vaapi_hevc_decodable(vendor_id: u32, vulkan_hevc: bool) -> bool {
    !vulkan_hevc
        && vendor_id != crate::video_vk::VENDOR_NVIDIA
        && crate::video_vaapi_native::hevc_decodable(vendor_id)
}

/// Can this machine decode AV1 in hardware? Device facts only, never a decoder existing:
/// Vulkan `DECODE_AV1` on the decode family; (Windows) D3D11 import so DXVA can run
/// Profile 0; (Linux) the presenter's VAAPI AV1 entry point, on a presenter the VAAPI
/// rung may feed. The CPU AV1 rung still exists; the wire promise is made once.
pub fn av1_hardware_decodable(vk: Option<&VulkanDecodeDevice>) -> bool {
    if vk.is_some_and(|v| v.video_decode && v.decode_video_caps & VIDEO_CODEC_OP_DECODE_AV1 != 0) {
        return true;
    }
    // Per-platform second answer, bound to a name: a cfg'd `return` is `needless_return` on Windows (`-D warnings`).
    #[cfg(windows)]
    let platform = vk.is_some_and(|v| v.d3d11_import);
    #[cfg(target_os = "linux")]
    let platform = vk.is_some_and(|v| v.vaapi_av1_decode && vaapi_auto_ok(Some(v)));
    platform
}

/// Does the Hello offer AV1 on this device? [`av1_hardware_decodable`] or a V4L2 node
/// that takes AV1 — the answer a settings UI must show.
pub fn av1_advertised(vk: Option<&VulkanDecodeDevice>) -> bool {
    av1_hardware_decodable(vk) || v4l2_summary(vk).codecs & punktfunk_core::quic::CODEC_AV1 != 0
}

/// Can this client decode 4:4:4 HEVC — the promise `VIDEO_CAP_444` makes.
///
/// Vulkan only: VAAPI/DXVA/CPU are 4:2:0. Advertising 4:4:4 without a Vulkan
/// 4:4:4 profile costs the whole codec (host grants 4:4:4 only on HEVC; no CPU HEVC).
/// Both 8- and 10-bit profiles are required: HDR may resolve 4:4:4 10-bit.
/// Not used for `VIDEO_CAP_10BIT`: all three hardware rungs decode 10-bit 4:2:0.
pub fn hevc_444_hardware_decodable(vk: Option<&VulkanDecodeDevice>) -> bool {
    #[cfg(any(target_os = "linux", windows))]
    {
        vk.is_some_and(|v| {
            crate::video_vk_native::hevc_shape_supported(v, CHROMA_444, 0)
                && crate::video_vk_native::hevc_shape_supported(v, CHROMA_444, 2)
        })
    }
    // No native Vulkan rung off the two desktop OSes, so nothing here can decode 4:4:4.
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = vk;
        false
    }
}

/// `chroma_format_idc` for 4:4:4 (H.265 7.4.3.2). Spelled once so the two depth probes cannot disagree.
const CHROMA_444: u8 = 3;

/// Can this client present a PQ stream — the promise `VIDEO_CAP_HDR` makes.
/// Decode is not the question (every hardware rung does 10-bit 4:2:0).
///
/// On Windows, D3D11VA is in every ladder and shows PQ as HDR10 pass-through
/// or the video processor's PQ→sRGB tonemap. That tonemap is unvalidated: the
/// Blt succeeds and paints garbage where it is missing. Elsewhere the CSC shader tonemaps PQ.
pub fn hdr_presentable(vk: Option<&VulkanDecodeDevice>) -> bool {
    #[cfg(windows)]
    {
        vk.is_none_or(|v| {
            !v.d3d11_import
                || v.d3d11_hdr10
                || crate::video_d3d11::pq_tonemap_supported(v.adapter_luid)
        })
    }
    #[cfg(not(windows))]
    {
        let _ = vk;
        true
    }
}

/// First AMD Windows driver seen rendering a PQ stream from the Vulkan rung correctly
/// (Adrenalin 25.9.1, driver store 32.0.21025). An older driver can paint it green;
/// D3D11VA on the same driver is unaffected. Fields as
/// [`umd_version_parts`](crate::video_types::umd_version_parts) splits them.
pub const AMD_VULKAN_HDR_DRIVER_FLOOR: [u16; 4] = [32, 0, 21025, 0];

/// Toast for an HDR session on the Vulkan rung whose AMD driver predates
/// [`AMD_VULKAN_HDR_DRIVER_FLOOR`]; `None` otherwise, including an unknown driver.
/// A warning only: D3D11VA costs frames, and a driver update fixes the Vulkan path.
pub fn amd_vulkan_hdr_driver_notice(
    driver: Option<[u16; 4]>,
    native_vulkan: bool,
    pq: bool,
) -> Option<String> {
    let v = driver?;
    if !native_vulkan || !pq || v >= AMD_VULKAN_HDR_DRIVER_FLOOR {
        return None;
    }
    Some(format!(
        "This AMD graphics driver ({}.{}.{}.{}) can show HDR as a green picture. Update it to \
         AMD Software 25.9.1 or newer, or switch the decoder to Direct3D 11 in Settings.",
        v[0], v[1], v[2], v[3]
    ))
}

/// `PUNKTFUNK_NATIVE_SCANOUT=1`: the Wayland presenter hands pictures to the compositor as
/// the window's buffer, so the Vulkan decoder keeps its pictures copyable. Opt-in: KWin
/// composites the buffer, where the swapchain's own can be scanned out.
pub fn native_scanout_wanted() -> bool {
    cfg!(target_os = "linux")
        && matches!(
            std::env::var("PUNKTFUNK_NATIVE_SCANOUT").as_deref(),
            Ok("1" | "flip")
        )
}

/// Can this machine's decoders take an access unit of several slices? Intel's Windows
/// Vulkan Video driver (32.0.101.8993) over-writes a heap table while recording the
/// decode of any multi-slice HEVC AU; FFmpeg faults at the same instruction. An Intel
/// GPU on Windows asks the host for one slice per frame instead.
pub fn multi_slice_decodable(vendor_id: Option<u32>) -> bool {
    !(cfg!(windows) && vendor_id == Some(VENDOR_INTEL))
}

/// Can a 10-bit stream be decoded here? The CPU rung is 8-bit, so this needs a
/// hardware rung with a 10-bit path: Vulkan Video, the platform rung, V4L2 with
/// a linear 10-bit format, or PyroWave. A software pin has only the last one.
/// PyroWave counts only when `preferred_codec` asks for it: the host never picks
/// it unasked, so its depth would buy an HEVC Main10 stream nothing here decodes.
pub fn ten_bit_decodable(
    vk: Option<&VulkanDecodeDevice>,
    decoder_pref: &str,
    preferred_codec: u8,
) -> bool {
    ten_bit_decodable_with(vk, decoder_pref, preferred_codec, v4l2_summary(vk))
}

/// [`ten_bit_decodable`] over an explicit V4L2 answer; tests need no device node.
pub(crate) fn ten_bit_decodable_with(
    vk: Option<&VulkanDecodeDevice>,
    decoder_pref: &str,
    preferred_codec: u8,
    v4l2: V4l2Summary,
) -> bool {
    // No device facts: keep the promise and let the rungs decide.
    let Some(v) = vk else {
        return !decode_pinned_to_software(decoder_pref);
    };
    #[cfg(target_os = "linux")]
    let platform = vaapi_auto_ok(Some(v)) && (v.vaapi_hevc_decode || v.vaapi_av1_decode);
    #[cfg(not(target_os = "linux"))]
    let platform = v.d3d11_import;
    let hardware = v.video_decode || platform || v4l2.ten_bit != 0;
    let pyrowave = v.pyrowave_decode && preferred_codec == punktfunk_core::quic::CODEC_PYROWAVE;
    pyrowave || (hardware && !decode_pinned_to_software(decoder_pref))
}

/// Desktop `video_caps` from the user switches, testable without a GPU.
/// Callers AND `want_444` with [`hevc_444_hardware_decodable`], `hdr_enabled`
/// with [`hdr_presentable`] and pass [`multi_slice_decodable`] as `multi_slice`
/// (Amlogic MediaCodec wedges on multi-slice AUs too). `ten_bit_sdr` asks for
/// Main10 under SDR.
pub fn video_caps_for(
    hdr_enabled: bool,
    ten_bit_sdr: bool,
    want_444: bool,
    multi_slice: bool,
) -> u8 {
    let mut caps = 0;
    if multi_slice {
        caps |= punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE;
    }
    if hdr_enabled {
        caps |= punktfunk_core::quic::VIDEO_CAP_10BIT | punktfunk_core::quic::VIDEO_CAP_HDR;
    }
    if ten_bit_sdr {
        caps |= punktfunk_core::quic::VIDEO_CAP_10BIT;
    }
    if want_444 {
        caps |= punktfunk_core::quic::VIDEO_CAP_444;
    }
    caps
}

/// [`decodable_codecs`] plus PyroWave when the compute probe passed, minus
/// codecs `decoder_pref` makes unreachable. Advertisement only: `resolve_codec`
/// never auto-picks PyroWave.
pub fn decodable_codecs_for(vk: Option<&VulkanDecodeDevice>, decoder_pref: &str) -> u8 {
    let v4l2 = v4l2_summary(vk);
    let bits = decodable_codecs_with(vk, decoder_pref, v4l2);
    tracing::info!(
        vulkan = format_args!(
            "{:#x}",
            vk.filter(|v| v.video_decode)
                .map_or(0, |v| v.decode_video_caps)
        ),
        vaapi_hevc = vk.is_some_and(|v| v.vaapi_hevc_decode),
        vaapi_av1 = vk.is_some_and(|v| v.vaapi_av1_decode),
        v4l2 = format_args!("{:#04x}", v4l2.codecs),
        v4l2_ten_bit = format_args!("{:#04x}", v4l2.ten_bit),
        advertised = format_args!("{bits:#04x}"),
        "decode ladder"
    );
    bits
}

/// [`decodable_codecs_for`] over an explicit V4L2 answer; tests need no device node.
pub(crate) fn decodable_codecs_with(
    vk: Option<&VulkanDecodeDevice>,
    decoder_pref: &str,
    v4l2: V4l2Summary,
) -> u8 {
    let mut bits = decodable_codecs();
    // AV1 is hardware-gated. Without this the CPU rung's existence advertises
    // AV1 to a machine that would decode it in software, and negotiation has no fallback.
    if bits & punktfunk_core::quic::CODEC_AV1 != 0
        && !av1_hardware_decodable(vk)
        && v4l2.codecs & punktfunk_core::quic::CODEC_AV1 == 0
    {
        tracing::info!(
            "AV1 not advertised: no hardware AV1 decode on this device (a software \
             decoder exists, but a 4K AV1 stream is not survivable on it)"
        );
        bits &= !punktfunk_core::quic::CODEC_AV1;
    }
    // Software pin has no HEVC. Guarded on something remaining: a Hello with
    // zero codecs reads as HEVC-only (`resolve_codec`'s pre-negotiation default).
    if bits & punktfunk_core::quic::CODEC_HEVC != 0
        && bits & !punktfunk_core::quic::CODEC_HEVC != 0
        && decode_pinned_to_software(decoder_pref)
    {
        tracing::info!(
            "HEVC not advertised: decode is pinned to software and there is no software \
             HEVC decoder in this build"
        );
        bits &= !punktfunk_core::quic::CODEC_HEVC;
    }
    // HEVC has no software rung, so the promise needs a hardware decoder. Advertised
    // blind, the host builds an HEVC session the client tears down and re-dials
    // without — every launch.
    if bits & punktfunk_core::quic::CODEC_HEVC != 0
        && !hevc_hardware_decodable(vk)
        && v4l2.codecs & punktfunk_core::quic::CODEC_HEVC == 0
    {
        tracing::info!("HEVC not advertised: no hardware HEVC decoder on this device");
        bits &= !punktfunk_core::quic::CODEC_HEVC;
    }
    #[cfg(all(any(target_os = "linux", windows), feature = "pyrowave"))]
    if vk.map(|v| v.pyrowave_decode).unwrap_or(false) {
        return bits | punktfunk_core::quic::CODEC_PYROWAVE;
    }
    #[cfg(not(all(any(target_os = "linux", windows), feature = "pyrowave")))]
    let _ = vk;
    bits
}

/// Can this adapter decode HEVC in hardware? Vulkan `DECODE_H265`, else the platform
/// rung: a DXVA HEVC profile on the presenter's adapter, or its VAAPI node where
/// `auto` may feed it. Device facts only, never a decoder existing.
fn hevc_hardware_decodable(vk: Option<&VulkanDecodeDevice>) -> bool {
    let Some(v) = vk else {
        return false;
    };
    #[cfg(windows)]
    let platform =
        v.d3d11_import && crate::video_d3d11_native::adapter_decodes_hevc(v.adapter_luid);
    #[cfg(target_os = "linux")]
    let platform = v.vaapi_hevc_decode && vaapi_auto_ok(Some(v));
    (v.video_decode && v.decode_video_caps & VIDEO_CODEC_OP_DECODE_H265 != 0) || platform
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::video::{migrate_decoder_pref, QueueLock, VIDEO_CODEC_OP_DECODE_H264};
    use crate::video_vk_native::NativeCodec;
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};

    /// Only an HDR session on the Vulkan rung with a driver below the floor warns.
    #[test]
    fn an_old_amd_driver_warns_only_for_vulkan_hdr() {
        let old = Some([32, 0, 12011, 4002]);
        let msg = amd_vulkan_hdr_driver_notice(old, true, true).expect("old driver, Vulkan, HDR");
        assert!(msg.contains("32.0.12011.4002"), "{msg}");
        assert!(msg.contains("25.9.1"), "{msg}");
        assert_eq!(
            amd_vulkan_hdr_driver_notice(old, false, true),
            None,
            "D3D11VA"
        );
        assert_eq!(amd_vulkan_hdr_driver_notice(old, true, false), None, "SDR");
        assert_eq!(
            amd_vulkan_hdr_driver_notice(None, true, true),
            None,
            "unknown driver"
        );
        let floor = Some(AMD_VULKAN_HDR_DRIVER_FLOOR);
        assert_eq!(amd_vulkan_hdr_driver_notice(floor, true, true), None);
        assert!(amd_vulkan_hdr_driver_notice(Some([32, 0, 21024, 65535]), true, true).is_some());
        assert_eq!(
            amd_vulkan_hdr_driver_notice(Some([32, 0, 31041, 1004]), true, true),
            None
        );
    }

    /// Advertising 4:4:4 on a device that cannot decode it costs HEVC: there is
    /// no CPU HEVC, and the host grants 4:4:4 on HEVC only.
    #[test]
    fn the_444_bit_needs_the_setting_and_a_device_that_can_decode_it() {
        const V444: u8 = punktfunk_core::quic::VIDEO_CAP_444;
        assert_eq!(
            video_caps_for(true, false, false, true) & V444,
            0,
            "a 4:4:4 promise this device cannot keep costs HEVC entirely"
        );
        assert_ne!(video_caps_for(true, false, true, true) & V444, 0);
        assert_eq!(video_caps_for(true, false, false, true) & V444, 0);
        assert_eq!(video_caps_for(false, false, false, true) & V444, 0);

        // 4:4:4 must not disturb 10-bit/HDR (those are not probe-gated).
        const HDR_BITS: u8 =
            punktfunk_core::quic::VIDEO_CAP_10BIT | punktfunk_core::quic::VIDEO_CAP_HDR;
        for want_444 in [false, true] {
            assert_eq!(
                video_caps_for(true, false, want_444, true) & HDR_BITS,
                HDR_BITS
            );
            assert_eq!(video_caps_for(false, false, want_444, true) & HDR_BITS, 0);
            assert_ne!(
                video_caps_for(false, false, want_444, true)
                    & punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE,
                0
            );
            assert_eq!(
                video_caps_for(false, false, want_444, false)
                    & punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE,
                0,
                "a decoder that wedges on slices keeps the bit off"
            );
        }
    }

    /// Intel on Windows is the one desktop decoder that asks for single-slice AUs.
    #[test]
    fn multi_slice_is_refused_only_for_intel_on_windows() {
        assert!(multi_slice_decodable(None));
        assert!(multi_slice_decodable(Some(0x10DE)));
        assert!(multi_slice_decodable(Some(0x1002)));
        assert_eq!(multi_slice_decodable(Some(VENDOR_INTEL)), !cfg!(windows));
    }

    /// 10-bit SDR advertises the depth bit alone — never HDR — and is subsumed by HDR.
    #[test]
    fn ten_bit_sdr_advertises_depth_without_hdr() {
        const TEN: u8 = punktfunk_core::quic::VIDEO_CAP_10BIT;
        const HDR: u8 = punktfunk_core::quic::VIDEO_CAP_HDR;
        assert_eq!(video_caps_for(false, true, false, true) & (TEN | HDR), TEN);
        assert_eq!(video_caps_for(false, false, false, true) & (TEN | HDR), 0);
        assert_eq!(
            video_caps_for(true, true, false, true) & (TEN | HDR),
            TEN | HDR
        );
        assert_eq!(
            video_caps_for(false, true, false, true) & punktfunk_core::quic::VIDEO_CAP_444,
            0
        );
        assert_ne!(
            video_caps_for(false, true, false, true) & punktfunk_core::quic::VIDEO_CAP_MULTI_SLICE,
            0
        );
    }

    /// No presenter Vulkan device ⇒ no 4:4:4. The `Some` arm needs a GPU.
    #[test]
    fn no_vulkan_device_means_no_444_promise() {
        assert!(!hevc_444_hardware_decodable(None));
    }

    /// An exhausted codec reconnects onto one with a CPU rung, never onto itself.
    #[test]
    fn an_exhausted_codec_reconnects_only_onto_one_with_a_cpu_rung() {
        let sw = software_decodable_codecs();
        assert_eq!(sw, CODEC_H264 | CODEC_AV1, "M8's CPU rung set");
        assert_eq!(sw & CODEC_HEVC, 0, "software HEVC is what M8 dropped");

        assert_eq!(
            last_rung_verdict(CODEC_HEVC, CODEC_H264 | CODEC_HEVC, RungLoss::Codec),
            LastRungVerdict::Retry { caps: CODEC_H264 }
        );
        // Survivors stay on the table — the host picks; we only ever remove.
        assert_eq!(
            last_rung_verdict(
                CODEC_HEVC,
                CODEC_H264 | CODEC_HEVC | CODEC_AV1,
                RungLoss::Codec
            ),
            LastRungVerdict::Retry {
                caps: CODEC_H264 | CODEC_AV1
            }
        );
        assert_eq!(
            last_rung_verdict(CODEC_HEVC, CODEC_HEVC, RungLoss::Codec),
            LastRungVerdict::Dead
        );
        for advertised in 0u8..16 {
            for negotiated in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
                if let LastRungVerdict::Retry { caps } =
                    last_rung_verdict(negotiated, advertised, RungLoss::Codec)
                {
                    assert_eq!(caps & negotiated, 0, "{negotiated:#x} re-offered");
                    // A codec with no CPU rung must not be offered again.
                    assert_eq!(caps & !software_decodable_codecs(), 0);
                    assert_ne!(caps, 0, "Retry must carry something to advertise");
                }
            }
        }
        // PyroWave never demotes; if it reached this rule the answer must be Dead.
        assert_eq!(
            last_rung_verdict(CODEC_PYROWAVE, CODEC_PYROWAVE, RungLoss::Codec),
            LastRungVerdict::Dead
        );
    }

    /// A shape the CPU rung cannot decode is not "this codec has no CPU rung":
    /// a 4:4:4 H.264 session must retry onto HEVC.
    #[test]
    fn a_shape_refusal_may_retry_onto_a_codec_with_no_cpu_rung() {
        assert_eq!(
            last_rung_verdict(CODEC_H264, CODEC_H264 | CODEC_HEVC, RungLoss::Shape),
            LastRungVerdict::Retry { caps: CODEC_HEVC }
        );
        // Same inputs as `Codec`: hardware H.264 exhausted and no CPU H.264 — HEVC is the same losing bet.
        assert_eq!(
            last_rung_verdict(CODEC_H264, CODEC_H264 | CODEC_HEVC, RungLoss::Codec),
            LastRungVerdict::Dead
        );
        // PyroWave opt-in survives a shape refusal, but never alone: `resolve_codec` would pick nothing.
        assert_eq!(
            last_rung_verdict(
                CODEC_H264,
                CODEC_H264 | CODEC_HEVC | CODEC_PYROWAVE,
                RungLoss::Shape
            ),
            LastRungVerdict::Retry {
                caps: CODEC_HEVC | CODEC_PYROWAVE
            }
        );
        assert_eq!(
            last_rung_verdict(CODEC_H264, CODEC_H264 | CODEC_PYROWAVE, RungLoss::Shape),
            LastRungVerdict::Dead
        );
        // A shape refusal still never re-offers the codec that raised it.
        for advertised in 0u8..16 {
            for negotiated in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
                if let LastRungVerdict::Retry { caps } =
                    last_rung_verdict(negotiated, advertised, RungLoss::Shape)
                {
                    assert_eq!(caps & negotiated, 0, "{negotiated:#x} re-offered");
                    assert_ne!(caps, 0, "Retry must carry something to advertise");
                }
            }
        }
    }

    /// A software pin has no HEVC rung, so HEVC must leave the advertisement before Hello.
    #[test]
    fn a_software_pin_takes_hevc_off_the_advertisement() {
        // Same precedence as `Decoder::new` (env first). Skip if the override is set.
        if std::env::var_os("PUNKTFUNK_DECODER").is_some() {
            return;
        }
        assert!(decode_pinned_to_software("software"));
        assert!(!decode_pinned_to_software("auto"));
        assert!(!decode_pinned_to_software("vulkan"));
        assert!(!decode_pinned_to_software(""));
    }

    /// Stored `vulkan` / `vaapi` / `d3d11va` map onto native pins. An unknown
    /// named rung is a hard error; the `native-*` names must not move.
    #[test]
    fn a_pre_m10_decoder_preference_migrates_onto_its_native_rung() {
        for (stored, want) in [
            ("vulkan", "native-vulkan"),
            ("vaapi", "native-vaapi"),
            ("d3d11va", "native-d3d11va"),
        ] {
            assert_eq!(migrate_decoder_pref(stored), want, "stored {stored:?}");
        }
        // Everything else passes through, including an unknown value — it must
        // reach the ladder unchanged rather than become a silent hardware pin.
        for pass in [
            "auto",
            "",
            "hardware",
            "software",
            "native-vulkan",
            "native-vaapi",
            "native-d3d11va",
            "something-else",
        ] {
            assert_eq!(migrate_decoder_pref(pass), pass, "passthrough {pass:?}");
        }
        // Migrated names are the pin constants the ladder compares against.
        assert_eq!(migrate_decoder_pref("vulkan"), "native-vulkan");
        #[cfg(target_os = "linux")]
        assert_eq!(
            migrate_decoder_pref("vaapi"),
            crate::video_vaapi_native::DECODER_PIN
        );
        #[cfg(windows)]
        assert_eq!(
            migrate_decoder_pref("d3d11va"),
            crate::video_d3d11_native::DECODER_PIN
        );
        // Migrated `vulkan` is a name `native_vulkan_gate` admits.
        assert!(native_vulkan_gate(
            &migrate_decoder_pref("vulkan"),
            CODEC_H264,
            true,
            VIDEO_CODEC_OP_DECODE_H264
        ));
    }

    pub(crate) fn decode_device(vendor_id: u32, device_name: &str) -> VulkanDecodeDevice {
        VulkanDecodeDevice {
            get_instance_proc_addr: 0,
            instance: 0,
            physical_device: 0,
            device: 0,
            vendor_id,
            device_name: device_name.into(),
            graphics_qf: 0,
            decode_qf: 0,
            decode_video_caps: 0,
            instance_extensions: Vec::new(),
            device_extensions: Vec::new(),
            f_sampler_ycbcr: true,
            f_timeline_semaphore: true,
            f_synchronization2: true,
            f_shader_int16: false,
            f_storage_buffer8: false,
            f_subgroup_size_control: false,
            f_compute_full_subgroups: false,
            f_shader_float16: false,
            api_version: 0,
            queue_families: Vec::new(),
            pyrowave_decode: false,
            video_decode: true,
            d3d11_import: false,
            dmabuf_import: true,
            vaapi_av1_decode: false,
            vaapi_hevc_decode: false,
            d3d11_hdr10: false,
            d3d11_nv12: false,
            d3d11_p010: false,
            adapter_luid: None,
            queue_lock: std::sync::Arc::new(QueueLock::new()),
        }
    }

    /// Where Vulkan has no AV1, the presenter's VAAPI answer advertises it, but only on a
    /// presenter the VAAPI rung may feed.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_vaapi_av1_is_advertised_where_the_vaapi_rung_runs() {
        let mut intel = decode_device(0x8086, "Intel(R) Graphics (RKL GT1)");
        intel.decode_video_caps = usable_decode_ops(
            0x8086,
            VIDEO_CODEC_OP_DECODE_H265 | VIDEO_CODEC_OP_DECODE_AV1,
        );
        assert!(
            !av1_hardware_decodable(Some(&intel)),
            "Vulkan AV1 is masked on Intel"
        );
        intel.vaapi_av1_decode = true;
        assert!(av1_hardware_decodable(Some(&intel)));
        assert_ne!(decodable_codecs_for(Some(&intel), "auto") & CODEC_AV1, 0);
        intel.dmabuf_import = false;
        assert!(
            !av1_hardware_decodable(Some(&intel)),
            "VAAPI frames need dmabuf import"
        );

        let mut nvidia = decode_device(0x10DE, "NVIDIA GeForce RTX 3070 Ti");
        nvidia.vaapi_av1_decode = true;
        assert!(!av1_hardware_decodable(Some(&nvidia)));
    }

    #[test]
    fn linux_intel_keeps_av1_off_the_vulkan_rung() {
        let all =
            VIDEO_CODEC_OP_DECODE_H264 | VIDEO_CODEC_OP_DECODE_H265 | VIDEO_CODEC_OP_DECODE_AV1;
        let intel = usable_decode_ops(0x8086, all);
        let h26x = VIDEO_CODEC_OP_DECODE_H264 | VIDEO_CODEC_OP_DECODE_H265;
        assert_eq!(intel & h26x, h26x);
        let av1_kept = intel & VIDEO_CODEC_OP_DECODE_AV1 != 0;
        assert_eq!(av1_kept, !cfg!(target_os = "linux"));
        assert_eq!(usable_decode_ops(0x1002, all), all);
        assert_eq!(usable_decode_ops(0x10DE, all), all);
    }

    /// `auto` enters the VAAPI rung only on a presenter that imports its
    /// dmabufs and is not NVIDIA, so a Vulkan refusal cannot land on frames
    /// the selected device cannot display.
    #[cfg(target_os = "linux")]
    #[test]
    fn auto_never_enters_vaapi_on_an_nvidia_presenter() {
        assert!(!vaapi_auto_ok(Some(&decode_device(
            0x10DE,
            "NVIDIA GeForce RTX 3070 Ti"
        ))));
        assert!(vaapi_auto_ok(Some(&decode_device(
            0x1002,
            "AMD RADV NAVI32"
        ))));
        assert!(vaapi_auto_ok(Some(&decode_device(0x8086, "Intel Arc"))));
        // Without dmabuf import the exported surfaces can never reach the screen.
        let mut no_import = decode_device(0x1002, "AMD RADV NAVI32");
        no_import.dmabuf_import = false;
        assert!(!vaapi_auto_ok(Some(&no_import)));
        // No Vulkan decode device means no presenter facts to refuse on.
        assert!(vaapi_auto_ok(None));
    }

    /// AV1 is advertised on a hardware fact, never on a decoder existing.
    /// Negotiation happens once; there is no falling back afterwards.
    #[test]
    fn av1_is_advertised_only_where_hardware_can_decode_it() {
        assert!(!av1_hardware_decodable(None));

        // `video_decode` alone is not AV1: plenty of devices decode H.264/H.265 only.
        let mut dev = decode_device(0x10de, "no-av1");
        dev.decode_video_caps = VIDEO_CODEC_OP_DECODE_H264 | VIDEO_CODEC_OP_DECODE_H265;
        #[cfg(not(windows))]
        assert!(
            !av1_hardware_decodable(Some(&dev)),
            "H.264+H.265 decode support says nothing about AV1"
        );

        // The AV1 operation bit is the yes.
        let mut dev = decode_device(0x1002, "vangogh-ish");
        dev.decode_video_caps =
            VIDEO_CODEC_OP_DECODE_H264 | VIDEO_CODEC_OP_DECODE_H265 | VIDEO_CODEC_OP_DECODE_AV1;
        assert!(av1_hardware_decodable(Some(&dev)));

        // No decode queue: caps bits are not a claim.
        let mut dev = decode_device(0x1002, "no-decode-queue");
        dev.decode_video_caps = VIDEO_CODEC_OP_DECODE_AV1;
        dev.video_decode = false;
        #[cfg(not(windows))]
        assert!(!av1_hardware_decodable(Some(&dev)));
    }

    /// HEVC has no CPU rung, so it is advertised only where hardware decodes
    /// it: Vulkan, the platform rung, or a V4L2 node. An ARM board with none
    /// of them starts on H.264 instead of re-dialling.
    #[cfg(target_os = "linux")]
    #[test]
    fn hevc_is_advertised_only_where_hardware_decodes_it() {
        if std::env::var_os("PUNKTFUNK_DECODER").is_some() {
            return;
        }
        let none = V4l2Summary::default();
        let mut soc = decode_device(0x5143, "Turnip Adreno (TM) 750");
        soc.video_decode = false;
        assert_eq!(
            decodable_codecs_with(Some(&soc), "auto", none),
            CODEC_H264,
            "no hardware decoder: only the codec with a CPU rung"
        );
        let iris = V4l2Summary {
            codecs: CODEC_H264 | CODEC_HEVC,
            ten_bit: 0,
        };
        assert_eq!(
            decodable_codecs_with(Some(&soc), "auto", iris),
            CODEC_H264 | CODEC_HEVC
        );
        let av1_too = V4l2Summary {
            codecs: CODEC_H264 | CODEC_HEVC | CODEC_AV1,
            ten_bit: CODEC_HEVC,
        };
        assert_eq!(
            decodable_codecs_with(Some(&soc), "auto", av1_too),
            CODEC_H264 | CODEC_HEVC | CODEC_AV1
        );

        // Vulkan and VAAPI answer as before.
        let mut radv = decode_device(0x1002, "AMD RADV NAVI32");
        radv.decode_video_caps = VIDEO_CODEC_OP_DECODE_H264 | VIDEO_CODEC_OP_DECODE_H265;
        assert_ne!(
            decodable_codecs_with(Some(&radv), "auto", none) & CODEC_HEVC,
            0
        );
        let mut old_intel = decode_device(0x8086, "Intel(R) HD Graphics 530");
        old_intel.video_decode = false;
        assert_eq!(
            decodable_codecs_with(Some(&old_intel), "auto", none) & CODEC_HEVC,
            0
        );
        old_intel.vaapi_hevc_decode = true;
        assert_ne!(
            decodable_codecs_with(Some(&old_intel), "auto", none) & CODEC_HEVC,
            0
        );
        old_intel.dmabuf_import = false;
        assert_eq!(
            decodable_codecs_with(Some(&old_intel), "auto", none) & CODEC_HEVC,
            0,
            "VAAPI frames need dmabuf import"
        );
    }

    /// 10-bit is a promise about a hardware rung; the CPU rung refuses it.
    #[test]
    fn ten_bit_is_advertised_only_with_a_hardware_ten_bit_path() {
        if std::env::var_os("PUNKTFUNK_DECODER").is_some() {
            return;
        }
        let none = V4l2Summary::default();
        let vulkan = decode_device(0x10DE, "NVIDIA GeForce RTX 3070 Ti");
        assert!(ten_bit_decodable_with(Some(&vulkan), "auto", 0, none));
        assert!(
            !ten_bit_decodable_with(Some(&vulkan), "software", 0, none),
            "a software pin makes the hardware unreachable"
        );

        let mut soc = decode_device(0x5143, "Turnip Adreno (TM) 750");
        soc.video_decode = false;
        assert!(!ten_bit_decodable_with(Some(&soc), "auto", 0, none));
        let eight_bit_only = V4l2Summary {
            codecs: CODEC_H264 | CODEC_HEVC,
            ten_bit: 0,
        };
        assert!(!ten_bit_decodable_with(
            Some(&soc),
            "auto",
            0,
            eight_bit_only
        ));
        let p010 = V4l2Summary {
            codecs: CODEC_H264 | CODEC_HEVC,
            ten_bit: CODEC_HEVC,
        };
        assert!(ten_bit_decodable_with(Some(&soc), "auto", 0, p010));

        // PyroWave carries its own depth, but only a player who picked it gets a
        // PyroWave stream. On auto the host builds HEVC Main10 for an 8-bit V4L2 node.
        soc.pyrowave_decode = true;
        assert!(!ten_bit_decodable_with(
            Some(&soc),
            "auto",
            0,
            eight_bit_only
        ));
        assert!(ten_bit_decodable_with(
            Some(&soc),
            "software",
            CODEC_PYROWAVE,
            none
        ));
        // No device facts: the promise stays, as before.
        assert!(ten_bit_decodable_with(None, "auto", 0, none));
    }

    /// A pin with stray whitespace is still a pin. `"native-vulkan "` matched
    /// no gate arm and fell through to `auto`. Same two inputs as `decode_pinned_to_software`.
    #[test]
    fn a_decoder_pin_survives_the_whitespace_a_shell_script_adds() {
        assert_eq!(
            resolve_decoder_pref(Some("native-vulkan "), "auto"),
            "native-vulkan",
            "a trailing space must not turn a pin into an unrecognised value"
        );
        assert_eq!(
            resolve_decoder_pref(Some("  software\t"), "auto"),
            "software"
        );
        // Trimmed to nothing is absent, not a pin to `""` (the auto family).
        assert_eq!(
            resolve_decoder_pref(Some("   "), "native-vaapi"),
            "native-vaapi"
        );
        assert_eq!(
            resolve_decoder_pref(Some(""), "native-vaapi"),
            "native-vaapi"
        );
        assert_eq!(resolve_decoder_pref(None, "native-vaapi"), "native-vaapi");
        // The trimmed value is what the gate admits.
        assert!(
            native_vulkan_gate(
                &resolve_decoder_pref(Some("native-vulkan "), "auto"),
                punktfunk_core::quic::CODEC_HEVC,
                true,
                VIDEO_CODEC_OP_DECODE_H265,
            ),
            "the whole point: the trimmed pin reaches the gate and is admitted"
        );
    }

    #[test]
    fn native_vulkan_gate_admits_pin_and_auto_family_per_codec_on_a_capable_family() {
        // Pin the spec values, not the implementation constants: a typo'd bit
        // would refuse every real driver and native would never engage.
        assert_eq!(
            VIDEO_CODEC_OP_DECODE_H264, 0x1,
            "VK_VIDEO_CODEC_OPERATION_DECODE_H264_BIT_KHR"
        );
        assert_eq!(
            VIDEO_CODEC_OP_DECODE_H265, 0x2,
            "VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR"
        );
        assert_eq!(
            VIDEO_CODEC_OP_DECODE_AV1, 0x4,
            "VK_VIDEO_CODEC_OPERATION_DECODE_AV1_BIT_KHR"
        );
        const H264_OP: u32 = VIDEO_CODEC_OP_DECODE_H264;
        const H265_OP: u32 = VIDEO_CODEC_OP_DECODE_H265;
        const AV1_OP: u32 = VIDEO_CODEC_OP_DECODE_AV1;
        for choice in ["native-vulkan", "auto", "", "hardware"] {
            // Pin and auto family admit both codecs pf-vkdecode speaks.
            assert!(
                native_vulkan_gate(choice, CODEC_H264, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, CODEC_HEVC, true, H265_OP),
                "{choice:?}"
            );
            // A family that runs both still admits each codec.
            assert!(
                native_vulkan_gate(choice, CODEC_H264, true, H264_OP | H265_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, CODEC_HEVC, true, H264_OP | H265_OP),
                "{choice:?}"
            );
            // Each codec needs its own bit.
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, true, H265_OP),
                "{choice:?}"
            );
            // AV1 is in the auto family. The pin is not a licence to skip the device leg.
            assert!(
                native_vulkan_gate(choice, CODEC_AV1, true, AV1_OP),
                "{choice:?}"
            );
            assert!(
                native_vulkan_gate(choice, CODEC_AV1, true, H264_OP | H265_OP | AV1_OP),
                "{choice:?}"
            );
            // An AV1 session on a family without the AV1 op would create a session the family cannot run.
            assert!(
                !native_vulkan_gate(choice, CODEC_AV1, true, H264_OP | H265_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_AV1, false, AV1_OP),
                "{choice:?}"
            );
            // No Vulkan-Video-capable presenter device.
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, false, H264_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, false, H265_OP),
                "{choice:?}"
            );
            // Caps bit is the codec gate, not `video_decode`.
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, true, 0),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, true, 0),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, true, AV1_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, true, AV1_OP),
                "{choice:?}"
            );
        }
        // Never for an explicit other-backend pin. Legacy spellings never reach this gate.
        for choice in ["native-vaapi", "native-d3d11va", "software"] {
            assert!(
                !native_vulkan_gate(choice, CODEC_H264, true, H264_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_HEVC, true, H265_OP),
                "{choice:?}"
            );
            assert!(
                !native_vulkan_gate(choice, CODEC_AV1, true, AV1_OP),
                "{choice:?}"
            );
        }
        // Construction sites `expect()` this map: a codec admitted with no decoder would panic.
        assert_eq!(
            native_codec(CODEC_H264).map(|(c, _)| c),
            Some(NativeCodec::H264)
        );
        assert_eq!(
            native_codec(CODEC_HEVC).map(|(c, _)| c),
            Some(NativeCodec::H265)
        );
        // AV1 has a decoder here. Whether `auto` may pick it is the gate, not this map.
        assert_eq!(
            native_codec(CODEC_AV1),
            Some((NativeCodec::Av1, VIDEO_CODEC_OP_DECODE_AV1))
        );
        assert!(native_codec(CODEC_PYROWAVE).is_none());
        assert!(native_codec(0).is_none());
    }

    /// Which rung/codec pairs meet the automatic-priority confidence bar.
    /// A table nobody checks drifts into a table that says everything is fine.
    #[test]
    fn the_evidence_table_marks_automatic_priority_confidence() {
        for (rung, codec, what) in [
            (
                NativeRung::Vulkan,
                CODEC_H264,
                "native Vulkan H.264 (M2 WP-D)",
            ),
            (NativeRung::Vulkan, CODEC_HEVC, "native Vulkan H.265 (M3)"),
            (
                NativeRung::Vulkan,
                CODEC_AV1,
                "native Vulkan AV1 (M7, RTX 5070 Ti)",
            ),
            (NativeRung::D3d11va, CODEC_H264, "native D3D11VA H.264 (M5)"),
            (NativeRung::D3d11va, CODEC_HEVC, "native D3D11VA H.265 (M5)"),
            (
                NativeRung::D3d11va,
                CODEC_AV1,
                "native D3D11VA AV1 (M7, RTX 3500 Ada + Intel Arc, 2026-08-07)",
            ),
        ] {
            assert!(
                native_evidence(rung, codec).verified,
                "{what} has hardware parity recorded"
            );
        }
        for (rung, codec, why) in [
            (
                NativeRung::Vaapi,
                CODEC_H264,
                "single-vendor parity without a soak stays below the priority bar",
            ),
            (
                NativeRung::Vaapi,
                CODEC_HEVC,
                "single-vendor parity without a soak stays below the priority bar",
            ),
            (
                NativeRung::Vaapi,
                CODEC_AV1,
                "single-vendor parity without a soak stays below the priority bar",
            ),
            (
                NativeRung::Software,
                CODEC_H264,
                "openh264 never ran on glass",
            ),
            (
                NativeRung::Software,
                CODEC_AV1,
                "rav1d has decoded on glass but has no parity check and no soak",
            ),
        ] {
            assert!(!native_evidence(rung, codec).verified, "{why}");
        }
        let vaapi_hevc = native_evidence(NativeRung::Vaapi, CODEC_HEVC);
        assert!(!vaapi_hevc.verified);
        assert!(
            vaapi_hevc.note.contains("bit-identical"),
            "below-priority evidence is not the same as never having run"
        );
        // An unknown codec leg is unverified, never a neighbour's evidence.
        assert!(!native_evidence(NativeRung::Vulkan, CODEC_PYROWAVE).verified);
        assert!(!native_evidence(NativeRung::Software, CODEC_HEVC).verified);
        assert!(!native_evidence(NativeRung::D3d11va, 0).verified);
        // Every answer explains itself in the session log.
        for rung in [
            NativeRung::Vulkan,
            NativeRung::D3d11va,
            NativeRung::Vaapi,
            NativeRung::Software,
        ] {
            for codec in [CODEC_H264, CODEC_HEVC, CODEC_AV1, 0] {
                assert!(
                    !native_evidence(rung, codec).note.is_empty(),
                    "{} / {codec} must carry a note",
                    rung.name()
                );
            }
        }
    }

    /// A rung below the automatic-priority bar still runs when only CPU is below.
    /// Which rung `auto` may prioritize is [`native_rung_admitted`].
    #[test]
    fn every_rung_runs_and_the_unproven_ones_are_named() {
        let unproven = [
            (NativeRung::Vaapi, CODEC_H264),
            (NativeRung::Vaapi, CODEC_HEVC),
            (NativeRung::Vaapi, CODEC_AV1),
        ];
        for (rung, codec) in unproven {
            let e = native_evidence(rung, codec);
            assert!(
                !e.verified,
                "{} / {codec:#x} reached the automatic-priority bar without a deliberate \
                 evidence-table promotion",
                rung.name()
            );
            assert!(
                e.note.contains("never soaked"),
                "{} / {codec:#x}: the warning note must name the missing priority \
                 coverage, got {:?}",
                rung.name(),
                e.note
            );
        }
        // Proven pairs stay proven. Deleting the filter must not relabel them.
        for (rung, codec) in [
            (NativeRung::Vulkan, CODEC_H264),
            (NativeRung::Vulkan, CODEC_HEVC),
            (NativeRung::Vulkan, CODEC_AV1),
            (NativeRung::D3d11va, CODEC_H264),
            (NativeRung::D3d11va, CODEC_HEVC),
            (NativeRung::D3d11va, CODEC_AV1),
        ] {
            assert!(native_evidence(rung, codec).verified, "{}", rung.name());
        }
    }

    /// `auto` yields an unproven rung only to proven code. Wrong pixels leave
    /// only through the error streak, so the choice is made before the session runs.
    #[test]
    fn an_unproven_rung_yields_to_a_proven_one_and_to_nothing_else() {
        // Linux Intel/unknown: VAAPI first, proven Vulkan under it — `auto` takes none of them.
        for codec in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
            assert!(
                !native_rung_admitted(NativeRung::Vaapi, codec, Some(NativeRung::Vulkan)),
                "codec {codec:#x}: a never-run VAAPI rung must not go first when the \
                 device can run the proven Vulkan rung for it"
            );
            // Admitted when nothing proven is below it (after Vulkan, or when Vulkan cannot run this codec).
            assert!(
                native_rung_admitted(NativeRung::Vaapi, codec, None),
                "codec {codec:#x}: with only the CPU below, the unproven rung runs"
            );
            // Yields to proven code, not to the unverified CPU rung.
            assert!(native_rung_admitted(
                NativeRung::Vaapi,
                codec,
                Some(NativeRung::Software)
            ));
        }
        // A proven rung is admitted whatever is below it.
        for (rung, codec) in [
            (NativeRung::Vulkan, CODEC_H264),
            (NativeRung::Vulkan, CODEC_HEVC),
            (NativeRung::Vulkan, CODEC_AV1),
            (NativeRung::D3d11va, CODEC_H264),
            (NativeRung::D3d11va, CODEC_HEVC),
            (NativeRung::D3d11va, CODEC_AV1),
        ] {
            for below in [
                None,
                Some(NativeRung::Vulkan),
                Some(NativeRung::Vaapi),
                Some(NativeRung::Software),
            ] {
                assert!(
                    native_rung_admitted(rung, codec, below),
                    "{} / {codec:#x} is proven and must run",
                    rung.name()
                );
            }
        }
        // CPU is last everywhere, so it always runs. A codec it cannot decode is [`last_rung_verdict`].
        for codec in [CODEC_H264, CODEC_HEVC, CODEC_AV1] {
            assert!(native_rung_admitted(NativeRung::Software, codec, None));
        }
    }

    /// "Vulkan is below me" is a claim about this GPU. Without it, Linux Intel
    /// would bar VAAPI on a box whose Vulkan device cannot decode this codec.
    #[test]
    fn the_rung_below_must_be_one_this_device_can_actually_run() {
        const H264_OP: u32 = VIDEO_CODEC_OP_DECODE_H264;
        const AV1_OP: u32 = VIDEO_CODEC_OP_DECODE_AV1;
        // Mesa/Intel: a decode family that advertises this codec.
        assert!(native_vulkan_usable(CODEC_H264, true, H264_OP));
        // No Vulkan Video, or a family that runs some other codec: neither is a rung to fall onto.
        assert!(!native_vulkan_usable(CODEC_H264, false, H264_OP));
        assert!(!native_vulkan_usable(CODEC_H264, true, AV1_OP));
        assert!(!native_vulkan_usable(CODEC_H264, true, 0));
        // A codec no native rung speaks is not a Vulkan rung.
        assert!(!native_vulkan_usable(CODEC_PYROWAVE, true, u32::MAX));
        // Linux Intel, both ways: H.264 on an H.264-capable device takes Vulkan;
        // AV1 on that device has no proven rung below VAAPI, so VAAPI runs.
        let below =
            |wire, caps| native_vulkan_usable(wire, true, caps).then_some(NativeRung::Vulkan);
        assert!(!native_rung_admitted(
            NativeRung::Vaapi,
            CODEC_H264,
            below(CODEC_H264, H264_OP)
        ));
        assert!(native_rung_admitted(
            NativeRung::Vaapi,
            CODEC_AV1,
            below(CODEC_AV1, H264_OP)
        ));
    }

    /// Advertised codecs are our rungs, not a decoder-library registry. The
    /// Hello is a promise: moving the set would renegotiate every session.
    #[test]
    fn advertised_codecs_describe_our_rungs_and_not_libavcodecs_registry() {
        let bits = decodable_codecs();
        assert_eq!(
            bits,
            CODEC_H264 | CODEC_HEVC | CODEC_AV1,
            "the three codecs the native rungs speak"
        );
        assert_eq!(
            bits & CODEC_PYROWAVE,
            0,
            "pyrowave rides decodable_codecs_for"
        );
        // CPU codecs are a subset, except HEVC: the one advertised codec with no CPU rung.
        assert_eq!(
            software_decodable_codecs() & !bits,
            0,
            "a codec with a CPU rung but no advertisement would be unreachable"
        );
        assert_eq!(
            bits & !software_decodable_codecs(),
            CODEC_HEVC,
            "HEVC is the ONE advertised codec with no CPU rung (last_rung_verdict owns it)"
        );
    }
}
