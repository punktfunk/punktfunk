//! Windows backend selection: NVIDIA → NVENC, AMD → AMF, Intel → QSV, and an adapter with only
//! a hardware MFT → Media Foundation. `auto` uses the selected render adapter so encode matches
//! capture. The pf-vdisplay driver opens the encoder; the host asks these questions to
//! negotiate and to plan capture.

use super::*;

/// The pf-vdisplay driver holds the only Windows encoder: it opens the backend on the pooled
/// device inside WUDFHost and publishes access units into the session's AU section, which
/// `pf_capture::open_driver_encoder` wraps as the loop's `Encoder`. Reaching here means a
/// session resolved to a non-IDD-push capture source, which no longer exists.
pub(crate) fn open(_: &OpenParams) -> Result<(Box<dyn Encoder>, &'static str)> {
    anyhow::bail!(
        "on Windows the pf-vdisplay driver encodes; the host opens no local video encoder \
         (the session must come from the IDD-push capture source)"
    )
}

/// GameStream `SERVER_CODEC_MODE_SUPPORT` for an unprobed backend.
const GPU_SUPERSET: u8 = punktfunk_core::quic::CODEC_H264
    | punktfunk_core::quic::CODEC_HEVC
    | punktfunk_core::quic::CODEC_AV1;

/// [`crate::host_wire_caps`] on Windows.
pub(crate) fn wire_caps() -> u8 {
    // PyroWave: its own Vulkan device by render-GPU id; the H.26x backend is irrelevant, and
    // software keeps the bit off. Interop is confirmed at encoder open
    // (`pyrowave_device_confirm_interop_support`); a failed open renegotiates to HEVC.
    let pyro =
        if cfg!(feature = "pyrowave") && windows_resolved_backend() != WindowsBackend::Software {
            punktfunk_core::quic::CODEC_PYROWAVE
        } else {
            0
        };
    let base = if windows_resolved_backend() == WindowsBackend::Software {
        punktfunk_core::quic::CODEC_H264
    } else if windows_backend_is_probed() {
        codec_support_wire_mask(windows_codec_support()).unwrap_or(GPU_SUPERSET)
    } else {
        GPU_SUPERSET
    };
    base | pyro
}

/// HEVC 4:4:4 on the resolved backend. Only NVENC probes: VCN has no 4:4:4 encode
/// (`design/native-amf-encoder.md`), VPL has no 4:4:4 query wired, and no MFT encodes it.
pub(crate) fn hevc_444() -> bool {
    match windows_resolved_backend() {
        #[cfg(feature = "nvenc")]
        WindowsBackend::Nvenc => {
            nvenc::probe_can_encode_444(Codec::H265, pf_gpu::resolve_render_adapter_luid())
        }
        _ => false,
    }
}

/// 10-bit `codec` on the resolved backend. A backend compiled out of this build answers
/// `false`, and Media Foundation is 8-bit 4:2:0 only: it rejects P010 at open.
pub(crate) fn ten_bit(codec: Codec) -> bool {
    match windows_resolved_backend() {
        #[cfg(feature = "nvenc")]
        WindowsBackend::Nvenc => {
            nvenc::probe_can_encode_10bit(codec, pf_gpu::resolve_render_adapter_luid())
        }
        WindowsBackend::Amf => {
            amf::probe_can_encode_10bit(codec, pf_gpu::resolve_render_adapter_luid())
        }
        #[cfg(feature = "qsv")]
        WindowsBackend::Qsv => {
            qsv::probe_can_encode_10bit(codec, pf_gpu::resolve_render_adapter_luid())
        }
        _ => false,
    }
}

/// [`crate::resolved_backend_is_gpu`] on Windows.
pub(crate) fn is_gpu() -> bool {
    !matches!(windows_resolved_backend(), WindowsBackend::Software)
}

/// Only NVENC ingests RGB and converts to 4:4:4 itself.
pub(crate) fn ingests_rgb_444() -> bool {
    windows_resolved_backend() == WindowsBackend::Nvenc
}

/// [`crate::backend_carries_sdr10`] on Windows.
pub(crate) fn sdr10(codec: Codec) -> bool {
    // The driver's PyroWave writes 16-bit planes from the FP16 SDR wide-colour desktop; the
    // handshake admits it only with that source.
    if codec == Codec::PyroWave {
        return true;
    }
    // NVENC widens 8→10 from packed RGB for HEVC + AV1. AMF and QSV take a BT.709 P010 the
    // driver's video processor produces (`EncodeInput::P010Sdr`) for HEVC Main10 only — their
    // AV1 10-bit SDR is unbuilt. `can_encode_10bit` still gates on the real probe.
    match windows_resolved_backend() {
        WindowsBackend::Nvenc => true,
        WindowsBackend::Amf | WindowsBackend::Qsv => codec == Codec::H265,
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
}

/// Explicit `PUNKTFUNK_ENCODER` pin. `None` for `auto`, unset, and unknown.
fn windows_pinned_backend() -> Option<WindowsBackend> {
    // Latched in HostConfig; do not re-read the env.
    match pf_host_config::config().encoder_pref.as_str() {
        "nvenc" | "hw" | "nvidia" | "cuda" => Some(WindowsBackend::Nvenc),
        "amf" | "amd" => Some(WindowsBackend::Amf),
        "qsv" | "intel" => Some(WindowsBackend::Qsv),
        "mf" | "mediafoundation" => Some(WindowsBackend::MediaFoundation),
        "sw" | "software" | "openh264" => Some(WindowsBackend::Software),
        _ => None,
    }
}

/// Has the selected adapter a hardware MFT? Cached per selected GPU, like
/// [`can_encode_444`]: [`windows_resolved_backend`] is uncached and runs on every
/// `/serverinfo` poll, where an `MFTEnum2` walk plus an adapter resolve would block the
/// async executor and log on each one.
fn mf_has_hardware_encoder() -> bool {
    static CACHE: ProbeCache<String, bool> = OnceLock::new();
    probe_cached(&CACHE, pf_gpu::selection_key(), || {
        mf::probe_has_hardware_encoder(pf_gpu::resolve_render_adapter_luid())
    })
}

/// Active Windows backend. `auto` → selected adapter's vendor; a contradicting
/// pin is overridden ([`resolve_windows_backend`]). Shared with GameStream.
pub fn windows_resolved_backend() -> WindowsBackend {
    let pinned = windows_pinned_backend();
    // Vendor query only to reconcile a pin — auto stays one inventory walk.
    let selected = if pinned.is_some() {
        pf_gpu::selected_gpu().map(|s| s.info.vendor_id)
    } else {
        None
    };
    resolve_windows_backend(pinned, selected, || match windows_gpu_vendor() {
        Some(GpuVendor::Nvidia) => WindowsBackend::Nvenc,
        Some(GpuVendor::Amd) => WindowsBackend::Amf,
        Some(GpuVendor::Intel) => WindowsBackend::Qsv,
        // No vendor with a native SDK. An adapter that still has a hardware MFT (Adreno)
        // is a GPU backend, and must resolve to one: `Software` here would also flip the
        // capturer to CPU staging, so the D3D11 input MF needs would never materialise.
        None if mf_has_hardware_encoder() => WindowsBackend::MediaFoundation,
        None => WindowsBackend::Software,
    })
}

/// True if the Windows codec advertisement comes from a real GPU probe
/// ([`windows_codec_support`]) rather than the static superset. AMF always;
/// QSV with `qsv`; NVENC with `nvenc`.
pub fn windows_backend_is_probed() -> bool {
    match windows_resolved_backend() {
        WindowsBackend::Amf => true,
        WindowsBackend::Qsv => cfg!(feature = "qsv"),
        WindowsBackend::Nvenc => cfg!(feature = "nvenc"),
        // MFT enumeration is the probe, and it needs no feature.
        WindowsBackend::MediaFoundation => true,
        WindowsBackend::Software => false,
    }
}

/// Encode-GPU vendor from the **selected** render adapter — the same one
/// capture and the IddCx pin sit on. Do not scan DXGI adapter 0: on hybrid
/// boxes that is often the iGPU while textures live on the dGPU. Uncached
/// (preference-dependent; session setup only). Unknown vendor → first known.
fn windows_gpu_vendor() -> Option<GpuVendor> {
    fn by_id(vendor_id: u32) -> Option<GpuVendor> {
        match vendor_id {
            pf_gpu::VENDOR_NVIDIA => Some(GpuVendor::Nvidia),
            pf_gpu::VENDOR_AMD => Some(GpuVendor::Amd),
            pf_gpu::VENDOR_INTEL => Some(GpuVendor::Intel),
            _ => None,
        }
    }
    let sel = pf_gpu::selected_gpu()?;
    by_id(sel.info.vendor_id)
        .or_else(|| pf_gpu::enumerate().iter().find_map(|g| by_id(g.vendor_id)))
}

/// Windows encode-codec probe, cached per (backend, selected GPU) so a
/// console preference change re-probes. Call only when
/// [`windows_backend_is_probed`]. AV1 and HEVC must be probed, not assumed.
///
/// AMD: native factory probe (the path the session opens). QSV: native VPL
/// Query. NVIDIA: the driver's GUID list.
pub fn windows_codec_support() -> CodecSupport {
    static CACHE: ProbeCache<String, CodecSupport> = OnceLock::new();
    let backend = windows_resolved_backend();
    let key = format!("{backend:?}:{}", pf_gpu::selection_key());
    probe_cached(&CACHE, key, || probe_codec_support(backend))
}

/// The uncached half of [`windows_codec_support`], for the backend it resolved.
fn probe_codec_support(backend: WindowsBackend) -> CodecSupport {
    let probe_one = |codec: Codec| -> bool {
        match backend {
            WindowsBackend::Amf => {
                amf::probe_can_encode(codec, pf_gpu::resolve_render_adapter_luid())
            }
            WindowsBackend::Qsv => {
                #[cfg(feature = "qsv")]
                {
                    qsv::probe_can_encode(codec, pf_gpu::resolve_render_adapter_luid())
                }
                #[cfg(not(feature = "qsv"))]
                {
                    false
                }
            }
            WindowsBackend::MediaFoundation => {
                mf::probe_can_encode(codec, pf_gpu::resolve_render_adapter_luid())
            }
            // NVENC answers from one GUID-list session below. Software is never
            // probed. Defensive `false` → static-superset fallback.
            WindowsBackend::Nvenc | WindowsBackend::Software => false,
        }
    };
    let caps = match backend {
        // One throwaway session lists every GUID. Featureless builds fall
        // through to `probe_one`'s all-false (= static superset).
        #[cfg(feature = "nvenc")]
        WindowsBackend::Nvenc => nvenc::probe_codec_support(pf_gpu::resolve_render_adapter_luid()),
        _ => CodecSupport {
            h264: probe_one(Codec::H264),
            h265: probe_one(Codec::H265),
            av1: probe_one(Codec::Av1),
        },
    };
    tracing::info!(
        ?backend,
        h264 = caps.h264,
        h265 = caps.h265,
        av1 = caps.av1,
        "Windows encode capabilities probed"
    );
    caps
}

/// Whether one more encode session fits the hardware budget. Display admission
/// declines rather than silently degrading a live sibling. NVENC is the only
/// hard cap today. See `design/windows-parallel-virtual-displays.md`.
pub fn can_open_another_session() -> bool {
    #[cfg(feature = "nvenc")]
    {
        nvenc::can_open_another_session()
    }
    #[cfg(not(feature = "nvenc"))]
    {
        true
    }
}
