//! Linux backend selection: the `PUNKTFUNK_ENCODER` resolver, the open ladder (direct-SDK
//! NVENC, Vulkan Video, native VAAPI, PyroWave, openh264) and every capability mirror that
//! must agree with it.

use super::*;

/// [`crate::open_video`]'s backend half.
pub(crate) fn open(p: &OpenParams) -> Result<(Box<dyn Encoder>, &'static str)> {
    open_video_backend_linux(pf_host_config::config().encoder_pref.as_str(), p)
}

/// [`crate::host_wire_caps`] on Linux. Resolves the backend once: this path is polled, and the
/// auto arm samples live GPU-preference state.
pub(crate) fn wire_caps() -> u8 {
    let backend = linux_resolved_backend();
    // PyroWave ORs onto the H.26x set whenever a GPU backend could open its Vulkan device;
    // software keeps it off. `resolve_codec` ignores the bit unless the client prefers it.
    let pyro = if cfg!(feature = "pyrowave") && backend != LinuxBackend::Software {
        punktfunk_core::quic::CODEC_PYROWAVE
    } else {
        0
    };
    // Shared with `/serverinfo`, so the two advertisements cannot drift.
    codec_support_wire_mask(linux_advertised_codec_support_for(backend)).unwrap_or(0) | pyro
}

/// HEVC 4:4:4 on the resolved backend: the direct SDK's `YUV444_ENCODE` cap. The native VAAPI
/// session is 4:2:0 only.
pub(crate) fn hevc_444() -> bool {
    #[cfg(feature = "nvenc")]
    {
        !linux_zero_copy_is_vaapi() && nvenc_cuda::probe_support().hevc_444
    }
    #[cfg(not(feature = "nvenc"))]
    {
        false
    }
}

/// 10-bit `codec` on the resolved backend. [`linux_zero_copy_is_vaapi`], not
/// [`linux_auto_is_vaapi`], so `encoder_pref` is honoured. AMD/Intel tries Vulkan then VAAPI, so
/// 10-bit is there if either says yes; NVIDIA asks the driver's `10BIT_ENCODE` cap.
pub(crate) fn ten_bit(codec: Codec) -> bool {
    if linux_zero_copy_is_vaapi() {
        return vulkan_10bit_available(codec) || vaapi_native::probe_can_encode(codec, true);
    }
    #[cfg(feature = "nvenc")]
    {
        let t = nvenc_cuda::probe_support().ten_bit;
        match codec {
            Codec::H265 => t.h265,
            Codec::Av1 => t.av1,
            _ => false,
        }
    }
    #[cfg(not(feature = "nvenc"))]
    {
        false
    }
}

/// [`crate::resolved_backend_is_gpu`] on Linux.
pub(crate) fn is_gpu() -> bool {
    linux_resolved_backend() != LinuxBackend::Software
}

/// Linux 4:4:4 is capture-side (portal RGB → `yuv444p`); no encoder ingests RGB for it.
pub(crate) fn ingests_rgb_444() -> bool {
    false
}

/// openh264 rate-control misconfigures if handed a hardware-session bitrate.
const SW_BITRATE_CEIL: u64 = 100_000_000;

/// [`open`] with the pref injected. `set_var` races `getenv` in parallel tests, and
/// `pf_host_config::config()` latches once. Shared dim/fps/chroma checks stay in the caller.
fn open_video_backend_linux(
    pref: &str,
    p: &OpenParams,
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
        ..
    } = *p;
    // Negotiated PyroWave bypasses `PUNKTFUNK_ENCODER` (that pref is a lab override).
    if codec == Codec::PyroWave {
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
            )
            .map(|e| (e, "pyrowave"));
        }
        #[cfg(not(feature = "pyrowave"))]
        anyhow::bail!(
            "session negotiated PyroWave but this host was built without --features \
             punktfunk-host/pyrowave (the advertisement bit should not have been set)"
        );
    }
    // Default VAAPI. With `vulkan-encode` + `PUNKTFUNK_VULKAN_ENCODE`, HEVC/AV1
    // opens Vulkan Video first; a failed open falls back
    // so the stream does not die. `format`/`bit_depth`/`chroma` are VAAPI-only
    // — Vulkan imports the dmabuf and does its own CSC.
    let open_amd_intel = || -> Result<(Box<dyn Encoder>, &'static str)> {
        // Vulkan when the device probe says yes (same profile query the open makes). Gamescope
        // has no embedded cursor — CSC blend is the only pointer path. A `no` goes to VAAPI here,
        // not a failed open. Vulkan gets `bit_depth`; the rule only picks the arm.
        #[cfg(feature = "vulkan-encode")]
        if amd_intel_opens_vulkan(codec, bit_depth == 10, hdr) {
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
        )
        .map(|e| (Box::new(e) as Box<dyn Encoder>, "vaapi-native"))
    };
    let open_nvidia = || open_nvenc(p).map(|e| (e, "nvenc"));
    // Same resolver the capability mirrors consult, so the alias table exists once.
    match resolve_linux_backend(pref, linux_auto_is_vaapi, cuda) {
        Some(LinuxBackend::Nvenc) => open_nvidia(),
        Some(LinuxBackend::AmdIntel) => open_amd_intel(),
        Some(LinuxBackend::Vulkan) => {
            #[cfg(feature = "vulkan-encode")]
            {
                if !matches!(codec, Codec::H265 | Codec::Av1) {
                    anyhow::bail!(
                        "the Vulkan Video encoder supports HEVC + AV1; the session negotiated {codec:?}"
                    );
                }
                vulkan_video::VulkanVideoEncoder::open(
                    codec,
                    format,
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    p.cursor_blend,
                    bit_depth,
                    hdr,
                )
                .map(|e| (Box::new(e) as Box<dyn Encoder>, "vulkan"))
            }
            #[cfg(not(feature = "vulkan-encode"))]
            {
                anyhow::bail!(
                    "PUNKTFUNK_ENCODER=vulkan requires a build with --features vulkan-encode"
                )
            }
        }
        // Explicit lab override; ignores the negotiated codec (every AU is intra).
        Some(LinuxBackend::Pyrowave) => {
            #[cfg(feature = "pyrowave")]
            {
                tracing::warn!(
                    ?codec,
                    "PUNKTFUNK_ENCODER=pyrowave forces the all-intra wavelet stream \
                     regardless of the negotiated codec — only a pyrowave-feature client \
                     that ALSO preferred CODEC_PYROWAVE can display it (lab override; \
                     normal sessions negotiate it instead)"
                );
                // Forced onto a session negotiated for another codec, whose
                // chroma may be HEVC 4:4:4 — PyroWave does not. Same worker
                // seam as the negotiated arm; this must not skip it.
                pyrowave_remote::open_preferring_worker(
                    width,
                    height,
                    fps,
                    bitrate_bps,
                    ChromaFormat::Yuv420,
                    bit_depth,
                    hdr,
                )
                .map(|e| (e, "pyrowave"))
            }
            #[cfg(not(feature = "pyrowave"))]
            {
                anyhow::bail!(
                    "PUNKTFUNK_ENCODER=pyrowave requires a build with --features punktfunk-host/pyrowave"
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
fn open_nvenc(p: &OpenParams) -> Result<Box<dyn Encoder>> {
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
            p.max_slices,
        )?) as Box<dyn Encoder>)
    }
    #[cfg(not(feature = "nvenc"))]
    {
        anyhow::bail!(
            "{} on NVIDIA needs the direct-SDK NVENC backend, which this build left out — \
             build with --features punktfunk-host/nvenc",
            p.codec.label()
        )
    }
}

/// Vulkan Video HEVC/AV1 on AMD/Intel. Default on.
/// The `PUNKTFUNK_VULKAN_ENCODE` row off is the VAAPI hatch.
/// A failed open falls back to VAAPI. See `design/linux-vulkan-video-encode.md`.
#[cfg(feature = "vulkan-encode")]
fn vulkan_encode_enabled() -> bool {
    pf_host_config::row_bool("PUNKTFUNK_VULKAN_ENCODE")
}

/// Whether a native planar source can carry this depth and colour: 8-bit SDR is NV12, HDR is
/// P010, and so is 10-bit SDR from a producer that composites it (`sdr10_native`). Any other
/// 10-bit SDR captures 8-bit packed RGB for the BT.709 widening CSC — native NV12 cannot be a
/// P010 source there.
const fn native_planar_depth_matches(bit_depth: u8, hdr: bool, sdr10_native: bool) -> bool {
    bit_depth < 10 || hdr || sdr10_native
}

/// Whether this session can ingest a producer's own NV12 without a host pass. Both AMD/Intel
/// lanes can: Vulkan Video imports it as its picture, the native libva session encodes it as
/// imported. AV1 is Vulkan Video's alone there, and neither has a pass to blend a pointer
/// into, so a `cursor_blend` session captures RGB. NVENC's raw lane copies the two planes into
/// its own slot. It blends a pointer into NV12 but not P010, so a P010 session (HDR, or
/// gamescope's 10-bit SDR) needs no blend.
pub fn linux_native_nv12_ok(
    codec: Codec,
    bit_depth: u8,
    hdr: bool,
    cursor_blend: bool,
    sdr10_native: bool,
) -> bool {
    if !native_planar_depth_matches(bit_depth, hdr, sdr10_native) {
        return false;
    }
    if !linux_zero_copy_is_vaapi() {
        let p010 = hdr || (bit_depth == 10 && sdr10_native);
        return !(p010 && cursor_blend) && codec != Codec::PyroWave && linux_nvenc_raw_dmabuf_ok();
    }
    if cursor_blend {
        return false;
    }
    match codec {
        Codec::H264 | Codec::H265 => true,
        #[cfg(feature = "vulkan-encode")]
        Codec::Av1 => vulkan_encode_enabled() && vulkan_encode_available(codec),
        _ => false,
    }
}

/// May capture plan the NVENC raw-dmabuf lane? Its identity-scoped health applies the
/// failure latch later, where the node id is known. Off without direct-SDK NVENC or on
/// the AMD/Intel plane.
pub fn linux_nvenc_raw_dmabuf_ok() -> bool {
    #[cfg(feature = "nvenc")]
    {
        !linux_zero_copy_is_vaapi()
            && pf_zerocopy::nvenc_raw_enabled()
            && pf_zerocopy::fused_convert_available()
    }
    #[cfg(not(feature = "nvenc"))]
    {
        false
    }
}

/// May an HDR capture stay zero-copy on NVIDIA (packed 10-bit PQ/BT.2020 CUDA)?
///
/// Only direct-SDK NVENC can: it registers `ARGB10`/`ABGR10` and CSCs in the
/// encoder. When it is compiled out, the capturer must not build the HDR
/// importer.
pub fn linux_hdr_cuda_ok() -> bool {
    #[cfg(feature = "nvenc")]
    {
        !linux_zero_copy_is_vaapi()
    }
    #[cfg(not(feature = "nvenc"))]
    {
        false
    }
}

/// Whether the resolved backend composites [`CapturedFrame::cursor`]. Answered
/// before capture opens: blend-capable backends take cursor-as-metadata; else
/// the compositor must embed the pointer.
///
/// `cuda_planned` is the caller's CUDA-payload prediction; `ten_bit` the
/// negotiated depth and `hdr` the colour verdict. A CPU payload is uploaded, and
/// never blended. The AMD/Intel arm asks [`amd_intel_opens_vulkan`], the rule the
/// open takes, so the prediction names the encoder the session gets.
pub fn cursor_blend_capable(codec: Codec, cuda_planned: bool, ten_bit: bool, hdr: bool) -> bool {
    // Negotiated PyroWave is selected before the pref; its CSC composites the cursor.
    if codec == Codec::PyroWave {
        return true;
    }
    let direct_nvenc = cfg!(feature = "nvenc");
    let backend = resolve_linux_backend(
        pf_host_config::config().encoder_pref.as_str(),
        linux_auto_is_vaapi,
        cuda_planned,
    );
    let vulkan_csc = {
        // Compute-CSC arm (the one that blends). Probe last: it opens a Vulkan instance.
        #[cfg(feature = "vulkan-encode")]
        {
            if matches!(backend, Some(LinuxBackend::AmdIntel)) {
                amd_intel_opens_vulkan(codec, ten_bit, hdr)
            } else {
                // An explicit Vulkan pref opens Vulkan at the negotiated depth.
                matches!(codec, Codec::H265 | Codec::Av1)
                    && vulkan_encode_enabled()
                    && vulkan_encode_available_at(codec, ten_bit)
            }
        }
        #[cfg(not(feature = "vulkan-encode"))]
        {
            let _ = (ten_bit, hdr); // they only ever narrow the Vulkan arm
            false
        }
    };
    cursor_blend_capable_for(backend, cuda_planned, direct_nvenc, vulkan_csc)
}

/// The depth the AMD/Intel arm asks Vulkan Video for, or `None` when it goes straight to VAAPI.
/// HEVC and AV1 take Vulkan at the negotiated depth, HDR or SDR alike: 10-bit SDR is HEVC Main10
/// or AV1 10-bit under BT.709 through `rgb2yuv10_709.comp`. H.264 has no Vulkan path. The
/// colour verdict rides along for the callers that already carry it; the depth is the rule.
#[cfg(feature = "vulkan-encode")]
fn amd_intel_vulkan_depth(codec: Codec, ten_bit: bool, _hdr: bool) -> Option<bool> {
    match codec {
        Codec::H265 | Codec::Av1 => Some(ten_bit),
        _ => None,
    }
}

/// Whether `open_amd_intel` tries Vulkan Video. The open and the cursor prediction both read it.
#[cfg(feature = "vulkan-encode")]
fn amd_intel_opens_vulkan(codec: Codec, ten_bit: bool, hdr: bool) -> bool {
    amd_intel_vulkan_depth(codec, ten_bit, hdr)
        .is_some_and(|ten| vulkan_encode_enabled() && vulkan_encode_available_at(codec, ten))
}

/// Dispatch-mirroring core of [`cursor_blend_capable`], device-free for tests.
/// `direct_nvenc` / `vulkan_csc` are the blend-capable arms, already gated.
fn cursor_blend_capable_for(
    backend: Option<LinuxBackend>,
    cuda_planned: bool,
    direct_nvenc: bool,
    vulkan_csc: bool,
) -> bool {
    match backend {
        Some(LinuxBackend::Pyrowave) => true,
        // Direct-SDK only (VkSlotBlend), and only a CUDA payload: an uploaded
        // CPU frame arrives after the blend point.
        Some(LinuxBackend::Nvenc) => cuda_planned && direct_nvenc,
        // Compute-CSC blends at either depth. Cursor sessions stay off native-NV12
        // / RGB-direct, so CSC eligibility (already carrying depth) is the answer.
        Some(LinuxBackend::AmdIntel) | Some(LinuxBackend::Vulkan) => vulkan_csc,
        // Capturer may composite inline; the encoder does not. Report encoder truth.
        Some(LinuxBackend::Software) | None => false,
    }
}

/// Can this GPU open a Vulkan Video encode session for `codec`? Cached per
/// (selected GPU, codec); probe runs outside the lock.
///
/// Only [`linux_native_nv12_ok`] consults this (no-fallback). Not wired into
/// [`open_video`]: non-NV12 already degrades to VAAPI on a failed open.
#[cfg(feature = "vulkan-encode")]
fn vulkan_encode_available(codec: Codec) -> bool {
    vulkan_encode_caps(codec).supported
}

/// Can Vulkan Video encode `codec` at this depth on the selected GPU? Per
/// codec: a device can advertise HEVC Main10 and still decline 10-bit AV1.
#[cfg(feature = "vulkan-encode")]
fn vulkan_encode_available_at(codec: Codec, ten_bit: bool) -> bool {
    let caps = vulkan_encode_caps(codec);
    caps.supported
        && if ten_bit {
            caps.ten_bit
        } else {
            caps.eight_bit
        }
}

/// Vulkan Video encode caps for `codec`, cached per (selected GPU, codec) so
/// a console GPU change re-probes. The probe opens its own Vulkan instance.
///
/// Cfg must include `vulkan-encode`: the return type lives in `vulkan_video`.
#[cfg(feature = "vulkan-encode")]
fn vulkan_encode_caps(codec: Codec) -> vulkan_video::VulkanEncodeCaps {
    static CACHE: ProbeCache<(String, &'static str), vulkan_video::VulkanEncodeCaps> =
        OnceLock::new();
    let key = (pf_gpu::selection_key(), codec.label());
    probe_cached(&CACHE, key, || probe_vulkan_encode_caps(codec))
}

/// The uncached half of [`vulkan_encode_caps`]: one probe, one log line.
#[cfg(feature = "vulkan-encode")]
fn probe_vulkan_encode_caps(codec: Codec) -> vulkan_video::VulkanEncodeCaps {
    let caps = vulkan_video::probe_encode_caps(codec);
    if caps.supported {
        tracing::info!(
            ?codec,
            eight_bit = caps.eight_bit,
            ten_bit = caps.ten_bit,
            "Vulkan Video encode probed — producer-native NV12 capture is eligible, and a 10-bit \
             session keeps this backend (real RFI + the compute CSC's cursor blend) where the \
             device accepts the profile"
        );
    } else {
        tracing::info!(
            ?codec,
            "Vulkan Video encode unavailable (no encode queue for this codec) — keeping the \
             packed-RGB capture negotiation (the native-NV12 path has no VAAPI fallback)"
        );
    }
    caps
}

/// NVIDIA-presence for the `auto` selector: these device nodes, no CUDA
/// context (that would allocate GPU state on every maybe-NVIDIA host).
fn nvidia_present() -> bool {
    std::path::Path::new("/dev/nvidiactl").exists() || std::path::Path::new("/dev/nvidia0").exists()
}

/// The `auto` Linux backend decision, shared by [`open_video`] and
/// [`linux_zero_copy_is_vaapi`]. Manual GPU preference picks that vendor's
/// backend (NVIDIA still needs the proprietary device nodes); else the
/// presence probe. A build without `nvenc` carries no NVIDIA arm at all, so it
/// always answers VAAPI — whose Vulkan Video leg the NVIDIA driver serves too.
///
/// Resolves **`auto` only** — ignores `encoder_pref`. Capability probes must
/// use [`linux_zero_copy_is_vaapi`], which layers the pref on top.
fn linux_auto_is_vaapi() -> bool {
    if !cfg!(feature = "nvenc") {
        return true;
    }
    if let Some(g) = pf_gpu::manual_selection() {
        if g.vendor_id == pf_gpu::VENDOR_NVIDIA {
            return !nvidia_present();
        }
        return true;
    }
    !nvidia_present()
}

/// Resolved Linux encode backend. One alias table for [`open_video_backend`]
/// and every capability/advertisement mirror. Labels come from the open
/// sites — this picks which arm runs, not what actually opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinuxBackend {
    Nvenc,
    AmdIntel,
    Vulkan,
    Pyrowave,
    Software,
}

/// Pure core. `None` = unknown pref: [`open_video_backend`] bails, capability
/// mirrors map it to auto via [`linux_resolved_backend`]. `auto_is_vaapi` is
/// lazy — `/serverinfo` polls these mirrors; explicit prefs must not probe.
fn resolve_linux_backend(
    pref: &str,
    auto_is_vaapi: impl FnOnce() -> bool,
    cuda: bool,
) -> Option<LinuxBackend> {
    Some(match pref {
        "nvenc" | "nvidia" | "cuda" => LinuxBackend::Nvenc,
        "vaapi" | "amd" | "intel" | "vaapi-native" => LinuxBackend::AmdIntel,
        "vulkan" | "vulkan-video" => LinuxBackend::Vulkan,
        "pyrowave" => LinuxBackend::Pyrowave,
        "software" | "sw" | "openh264" => LinuxBackend::Software,
        // A CUDA frame can only be consumed by NVENC; else [`linux_auto_is_vaapi`].
        "auto" | "" => {
            if cuda || !auto_is_vaapi() {
                LinuxBackend::Nvenc
            } else {
                LinuxBackend::AmdIntel
            }
        }
        _ => return None,
    })
}

/// Capability-mirror wrapper: unknown pref → auto. `cuda = false` because
/// these answer pre-session questions.
fn linux_resolved_backend() -> LinuxBackend {
    let pref = pf_host_config::config().encoder_pref.as_str();
    resolve_linux_backend(pref, linux_auto_is_vaapi, false).unwrap_or_else(|| {
        if linux_auto_is_vaapi() {
            LinuxBackend::AmdIntel
        } else {
            LinuxBackend::Nvenc
        }
    })
}

/// Dmabuf modifiers PyroWave's Vulkan device imports for the capture fourcc.
/// VAAPI LINEAR-only starves tiled Mutter+NVIDIA allocations.
#[cfg(feature = "pyrowave")]
pub fn pyrowave_capture_modifiers(fourcc: u32) -> Vec<u64> {
    pyrowave::capture_modifiers(fourcc)
}

/// Tiled dmabuf modifiers the session's encoder lane proved it can import for
/// the capture `fourcc` — what the gamescope producer's tiled offer narrows to.
/// Empty means LINEAR-only. Each lane returns its own proved list: Vulkan
/// Video answers come from the packed-usage probes, VAAPI answers from
/// `Display::import_dmabuf` + VPP, never one lane filtered by the other.
pub fn linux_capture_modifiers(codec: Codec, fourcc: u32, bit_depth: u8, hdr: bool) -> Vec<u64> {
    #[cfg(feature = "pyrowave")]
    if codec == Codec::PyroWave {
        return pyrowave_capture_modifiers(fourcc);
    }
    // A build without PyroWave has no module to ask; advertise LINEAR.
    #[cfg(not(feature = "pyrowave"))]
    if codec == Codec::PyroWave {
        return Vec::new();
    }
    // The rule `open_amd_intel` takes, so the capture answers come from the encoder that opens.
    #[cfg(not(feature = "vulkan-encode"))]
    let _ = (bit_depth, hdr);
    #[cfg(feature = "vulkan-encode")]
    let ten_bit = bit_depth >= 10;
    #[cfg(feature = "vulkan-encode")]
    let vulkan_lane = amd_intel_opens_vulkan(codec, ten_bit, hdr);
    #[cfg(not(feature = "vulkan-encode"))]
    let vulkan_lane = false;
    if vulkan_lane {
        #[cfg(feature = "vulkan-encode")]
        {
            return vulkan_video::vulkan_capture_modifiers(codec, fourcc, ten_bit);
        }
    }
    let candidates = vk_util::sampled_capture_modifiers(fourcc);
    vaapi_native::vaapi_capture_modifiers(fourcc, &candidates)
}

/// True if the Linux GPU backend is VAAPI rather than NVENC — so capture
/// picks dmabuf passthrough vs EGL→CUDA. Mirrors [`open_video`].
pub fn linux_zero_copy_is_vaapi() -> bool {
    linux_zero_copy_is_vaapi_for(linux_resolved_backend())
}
/// Zero-copy plane for an already-resolved backend, so a polled caller
/// (`host_wire_caps`) pays for resolution once.
fn linux_zero_copy_is_vaapi_for(backend: LinuxBackend) -> bool {
    match backend {
        LinuxBackend::Nvenc => false,
        LinuxBackend::AmdIntel => true,
        // Raw dmabuf on any vendor — never the EGL→CUDA import.
        LinuxBackend::Pyrowave => true,
        // Preserved `_` fallthrough, not endorsed. Vulkan-on-NVIDIA is a known
        // mismatch (EGL→CUDA for a dmabuf importer); software is latent (H.264).
        LinuxBackend::Vulkan | LinuxBackend::Software => linux_auto_is_vaapi(),
    }
}

/// NVIDIA encode-GUID list (cached once per process). Process-wide because
/// the direct backend opens on shared `cuda::context()` (device 0). Fail-open
/// contract: see [`nvenc_cuda::probe_support`].
#[cfg(feature = "nvenc")]
pub fn nvenc_codec_support() -> CodecSupport {
    use std::sync::OnceLock;
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
    probed.codecs
}

/// What the AMD/Intel plane can encode: the native VAAPI session for H.264 and
/// HEVC, Vulkan Video for HEVC and AV1 — the two arms [`open_video`] tries, so
/// nothing is advertised that would die at open. NVIDIA uses
/// [`nvenc_codec_support`]; callers gate on [`linux_zero_copy_is_vaapi`].
///
/// Cached per selected GPU, like [`windows_codec_support`]: a console
/// preference change moves the render node and the Vulkan device both probes
/// open.
pub fn vaapi_codec_support() -> CodecSupport {
    static CACHE: ProbeCache<String, CodecSupport> = OnceLock::new();
    probe_cached(&CACHE, pf_gpu::selection_key(), probe_vaapi_codec_support)
}

/// The uncached half of [`vaapi_codec_support`].
fn probe_vaapi_codec_support() -> CodecSupport {
    // The same query the open makes, at the depth it opens with. AV1 has no
    // native VAAPI path at all, so this is the only thing that can say yes.
    let vulkan = |c| {
        #[cfg(feature = "vulkan-encode")]
        {
            vulkan_encode_enabled() && vulkan_encode_available_at(c, false)
        }
        #[cfg(not(feature = "vulkan-encode"))]
        {
            let _: Codec = c;
            false
        }
    };
    let probe = |c| vaapi_native::probe_can_encode(c, false);
    let caps = CodecSupport {
        h264: probe(Codec::H264),
        h265: probe(Codec::H265) || vulkan(Codec::H265),
        av1: vulkan(Codec::Av1),
    };
    tracing::info!(
        h264 = caps.h264,
        h265 = caps.h265,
        av1 = caps.av1,
        "VAAPI encode capabilities probed"
    );
    caps
}

/// The codec set a Linux host may advertise: the resolved backend's probe,
/// narrowed to what that backend can open.
pub fn linux_advertised_codec_support() -> CodecSupport {
    linux_advertised_codec_support_for(linux_resolved_backend())
}

/// [`linux_advertised_codec_support`] for an already-resolved backend, so a
/// polled caller pays for resolution once.
fn linux_advertised_codec_support_for(backend: LinuxBackend) -> CodecSupport {
    let probed = match backend {
        // openh264, and it never probes.
        LinuxBackend::Software => None,
        _ if linux_zero_copy_is_vaapi_for(backend) => Some(vaapi_codec_support()),
        // Driver GUID list, like the VAAPI arm.
        #[cfg(feature = "nvenc")]
        LinuxBackend::Nvenc => Some(nvenc_codec_support()),
        _ => None,
    };
    narrow_to_openable(backend, probed)
}

/// A probe narrowed by what `backend` can open at all. An all-`false` probe
/// means the GPU was unusable at probe time, not that it encodes nothing, so
/// it fails open to the superset — narrowing can then only take codecs away.
/// Pure, so the ceilings are testable without a GPU.
fn narrow_to_openable(backend: LinuxBackend, probed: Option<CodecSupport>) -> CodecSupport {
    // A forced-vulkan pref is a ceiling, never a replacement: the arm encodes
    // HEVC/AV1 only (H.264 dies at open), and only in a build that has it.
    // A static HEVC|AV1 would add AV1 on GPUs whose probe withholds it.
    let ceiling = match backend {
        LinuxBackend::Software => CodecSupport {
            h264: true,
            h265: false,
            av1: false,
        },
        // The resolver knows the pref is vulkan; only this `cfg!` knows the
        // build can open it. Else: advertise-then-die-at-open.
        LinuxBackend::Vulkan => {
            let built = cfg!(feature = "vulkan-encode");
            CodecSupport {
                h264: false,
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

/// Can Vulkan Video encode `codec` at 10-bit on the selected GPU, and is it enabled? Both 10-bit
/// gates route here; the probe is cached per (GPU, codec).
#[cfg(feature = "vulkan-encode")]
fn vulkan_10bit_available(codec: Codec) -> bool {
    vulkan_encode_enabled() && vulkan_encode_available_at(codec, true)
}
#[cfg(not(feature = "vulkan-encode"))]
fn vulkan_10bit_available(_codec: Codec) -> bool {
    false
}
/// [`crate::backend_carries_sdr10`] on Linux.
pub(crate) fn sdr10(codec: Codec) -> bool {
    // PyroWave's own CSC widens packed RGB to 10-bit (`rgb2yuv10_709.comp` / the 4:4:4
    // twin) on its private Vulkan device — the resolved H.26x backend is irrelevant.
    if codec == Codec::PyroWave {
        return cfg!(feature = "pyrowave");
    }
    // Direct NVENC (HEVC + AV1) widens 8→10 from packed RGB. On AMD/Intel, Vulkan Video carries
    // HEVC Main10 and AV1 10-bit SDR (`rgb2yuv10_709.comp`) where the device offers the 10-bit
    // profile, and VAAPI carries HEVC Main10 under BT.709 as the fallback. The encoder degrades a
    // planar surface to 8-bit if some path delivers one.
    match linux_resolved_backend() {
        LinuxBackend::Nvenc => cfg!(feature = "nvenc"),
        LinuxBackend::AmdIntel => {
            codec == Codec::H265 || (codec == Codec::Av1 && vulkan_10bit_available(codec))
        }
        LinuxBackend::Vulkan => {
            matches!(codec, Codec::H265 | Codec::Av1) && vulkan_10bit_available(codec)
        }
        _ => false,
    }
}

/// Open the software H.264 encoder by name, bypassing the backend ladder.
///
/// [`open_video`] resolves a backend from `PUNKTFUNK_ENCODER`, which the host reads **once** and
/// latches — so a caller that decides later cannot steer it, and `auto` never picks software
/// anyway. The browser plane is exactly that caller: it serves a GPU-less host and wants this
/// encoder specifically, not whatever the ladder would have chosen.
pub fn open_software_h264(
    format: PixelFormat,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
) -> Result<Box<dyn Encoder>> {
    sw::OpenH264Encoder::open(format, width, height, fps, bitrate_bps.min(SW_BITRATE_CEIL))
        .map(|e| Box::new(e) as Box<dyn Encoder>)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_advertisement_never_offers_what_the_backend_cannot_open() {
        use LinuxBackend::*;
        let caps = |h264, h265, av1| CodecSupport { h264, h265, av1 };
        let probed = caps(true, true, false);
        // A GPU probe passes through on the vendor backends.
        assert_eq!(
            (
                narrow_to_openable(AmdIntel, Some(probed)).h264,
                narrow_to_openable(AmdIntel, Some(probed)).av1
            ),
            (true, false)
        );
        // A vulkan pin drops H.264 (it bails at open) and keeps what the probe found.
        let vulkan = narrow_to_openable(Vulkan, Some(caps(true, true, true)));
        assert_eq!(
            (vulkan.h264, vulkan.h265, vulkan.av1),
            (
                false,
                cfg!(feature = "vulkan-encode"),
                cfg!(feature = "vulkan-encode")
            )
        );
        // openh264 is H.264 whatever a GPU probe says.
        let sw = narrow_to_openable(Software, Some(caps(true, true, true)));
        assert_eq!((sw.h264, sw.h265, sw.av1), (true, false, false));
        // An unusable probe fails open to the superset, still narrowed.
        let unprobed = narrow_to_openable(AmdIntel, None);
        assert!(unprobed.h264 && unprobed.h265 && unprobed.av1);
        let empty = narrow_to_openable(AmdIntel, Some(caps(false, false, false)));
        assert!(empty.h264 && empty.h265 && empty.av1);
    }

    #[test]
    fn cursor_blend_capability_mirrors_the_dispatch() {
        use LinuxBackend::*;
        assert!(cursor_blend_capable_for(
            Some(Pyrowave),
            false,
            false,
            false
        ));
        assert!(cursor_blend_capable_for(Some(Nvenc), true, true, false));
        assert!(
            !cursor_blend_capable_for(Some(Nvenc), false, true, false),
            "a CPU payload is uploaded, past the blend point"
        );
        assert!(
            !cursor_blend_capable_for(Some(Nvenc), true, false, false),
            "a build without the `nvenc` feature has no NVENC at all"
        );
        assert!(cursor_blend_capable_for(Some(AmdIntel), false, false, true));
        assert!(
            !cursor_blend_capable_for(Some(AmdIntel), false, false, false),
            "no eligible Vulkan CSC arm (H.264, PUNKTFUNK_VULKAN_ENCODE=0, unsupported \
             device) resolves to native VAAPI, which cannot blend"
        );
        assert!(cursor_blend_capable_for(Some(Vulkan), false, false, true));
        assert!(!cursor_blend_capable_for(Some(Software), false, true, true));
        assert!(!cursor_blend_capable_for(None, false, true, true));
    }

    #[cfg(feature = "vulkan-encode")]
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
        assert_eq!(
            resolve_linux_backend("pyrowave", no_probe, false),
            Some(Pyrowave)
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

    /// Linux dispatch through the resolver, GPU-free via the software arm.
    /// Pref is injected — `set_var` races `getenv` in parallel tests.
    #[test]
    fn open_video_backend_dispatches_software() {
        let sw = OpenParams {
            codec: Codec::H264,
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
            max_slices: 4,
        };
        let (enc, label) =
            open_video_backend_linux("software", &sw).expect("software arm must open GPU-free");
        assert_eq!(label, "software");
        drop(enc);
        let h265 = OpenParams {
            codec: Codec::H265,
            ..sw
        };
        let err = match open_video_backend_linux("software", &h265) {
            // `expect_err` needs `Ok: Debug`; `Box<dyn Encoder>` isn't.
            Ok(_) => panic!("software emits H.264 only; an H.265 session must be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("H.264"), "{err:#}");
    }

    /// `auto` on an AMD/Intel box opens the native VAAPI session.
    #[test]
    #[ignore = "needs a real VAAPI device"]
    fn auto_opens_the_native_vaapi_session() {
        let p = OpenParams {
            codec: Codec::H264,
            format: PixelFormat::Bgra,
            width: 320,
            height: 240,
            fps: 60,
            bitrate_bps: 2_000_000,
            cuda: false,
            bit_depth: 8,
            hdr: false,
            chroma: ChromaFormat::Yuv420,
            cursor_blend: false,
            max_slices: 32,
        };
        let (enc, backend) = open_video_backend_linux("auto", &p).expect("open");
        assert_eq!(backend, "vaapi-native");
        assert!(enc.caps().supports_rfi);
    }
}
