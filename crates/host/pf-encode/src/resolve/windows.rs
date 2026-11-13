//! Windows backend selection: NVIDIA → NVENC, AMD → AMF, Intel → QSV, and an adapter with only
//! a hardware MFT → Media Foundation. `auto` uses the selected render adapter so encode matches
//! capture. The pf-vdisplay driver opens the encoder; the host asks these questions to
//! negotiate and to plan capture.

use super::*;
use crate::env_knobs::Knobs;

/// What resolution found on Windows. Each cell is filled the first time an answer needs it.
#[derive(Debug, Default)]
pub(crate) struct Probes {
    /// [`Resolved::backend`].
    pub(crate) backend: OnceLock<WindowsBackend>,
    /// The resolved backend's codec probe ([`probe_codec_support`]), kept only once it finds a
    /// codec.
    pub(crate) codecs: OnceLock<CodecSupport>,
    /// Set when the `codecs` probe found nothing, so the miss warns once.
    pub(crate) codecs_missed: OnceLock<()>,
}

/// The pf-vdisplay driver holds the only Windows encoder: it opens the backend on the pooled
/// device inside WUDFHost and publishes access units into the session's AU section, which
/// the source's `pf_capture::RemoteEncoder` wraps as the loop's `Encoder`. Reaching here means a
/// session resolved to a non-IDD-push capture source, which no longer exists.
pub(crate) fn open(
    _: &Resolved,
    _: &OpenParams,
    _: &Knobs,
) -> Result<(Box<dyn Encoder>, &'static str)> {
    anyhow::bail!(
        "on Windows the pf-vdisplay driver encodes; the host opens no local video encoder \
         (the session must come from the IDD-push capture source)"
    )
}

/// GameStream `SERVER_CODEC_MODE_SUPPORT` for an unprobed backend.
const GPU_SUPERSET: u8 = punktfunk_core::quic::CODEC_H264
    | punktfunk_core::quic::CODEC_HEVC
    | punktfunk_core::quic::CODEC_AV1;

/// [`Resolved::wire_caps`] on Windows, over one resolution of the backend.
pub(crate) fn wire_caps(r: &Resolved) -> u8 {
    let backend = r.backend();
    // PyroWave: its own Vulkan device by render-GPU id; the H.26x backend is irrelevant, and
    // software keeps the bit off. Encoder open confirms the plane and fence imports
    // (`pf-encode-win` `refused_import`); a refusal ends the session.
    let pyro = if r.inputs.features.pyrowave && backend != WindowsBackend::Software {
        punktfunk_core::quic::CODEC_PYROWAVE
    } else {
        0
    };
    let base = if backend == WindowsBackend::Software {
        punktfunk_core::quic::CODEC_H264
    } else if r.is_probed() {
        r.codecs()
            .and_then(codec_support_wire_mask)
            .unwrap_or(GPU_SUPERSET)
    } else {
        GPU_SUPERSET
    };
    base | pyro
}

/// [`Resolved::advertised`] on Windows: the probe, where the backend has one.
pub(crate) fn advertised(r: &Resolved) -> Option<CodecSupport> {
    r.is_probed().then(|| r.codecs()).flatten()
}

/// HEVC 4:4:4 on the resolved backend. Only NVENC probes: VCN has no 4:4:4 encode
/// (`design/native-amf-encoder.md`), VPL has no 4:4:4 query wired, and no MFT encodes it. A
/// probe that could not run reads `false`, same as a refusal, so every answer is kept.
pub(crate) fn hevc_444(r: &Resolved) -> Option<bool> {
    Some(match r.backend() {
        #[cfg(feature = "nvenc")]
        WindowsBackend::Nvenc => {
            nvenc::probe_can_encode_444(Codec::H265, pf_gpu::resolve_render_adapter_luid())
        }
        _ => false,
    })
}

/// 10-bit `codec` on the resolved backend. A backend compiled out of this build answers
/// `false`, and Media Foundation is 8-bit 4:2:0 only: it rejects P010 at open. A probe that
/// could not run reads `false`, same as a refusal, so every answer is kept.
pub(crate) fn ten_bit(r: &Resolved, codec: Codec) -> Option<bool> {
    Some(match r.backend() {
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
    })
}

/// [`Resolved::is_gpu`] on Windows.
pub(crate) fn is_gpu(r: &Resolved) -> bool {
    !matches!(r.backend(), WindowsBackend::Software)
}

/// Only NVENC ingests RGB and converts to 4:4:4 itself.
pub(crate) fn ingests_rgb_444(r: &Resolved) -> bool {
    r.backend() == WindowsBackend::Nvenc
}

/// [`Resolved::carries_sdr10`] on Windows.
pub(crate) fn sdr10(r: &Resolved, codec: Codec) -> bool {
    // The driver's PyroWave writes 16-bit planes from the FP16 SDR wide-colour desktop; the
    // handshake admits it only with that source.
    if codec == Codec::PyroWave {
        return true;
    }
    // NVENC widens 8→10 from packed RGB. AMF and QSV take a BT.709 P010 the driver produces
    // (`EncodeInput::P010Sdr`, or `P010Wcg` from wide colour). HEVC Main10 and AV1 alike;
    // `can_encode_10bit` still gates on the real probe.
    match r.backend() {
        WindowsBackend::Nvenc => true,
        WindowsBackend::Amf | WindowsBackend::Qsv => matches!(codec, Codec::H265 | Codec::Av1),
        _ => false,
    }
}

/// [`Resolved::rgb_full_range`] on Windows: AMF's conversion, measured on the render adapter
/// unless `PUNKTFUNK_AMF_FULL_RANGE` says.
pub(crate) fn rgb_full_range(r: &Resolved, _codec: Codec) -> RgbFullRange {
    if r.backend() != WindowsBackend::Amf {
        return RgbFullRange::BOTH;
    }
    // AMF converts the 8-bit desktop only: an HDR session's P010 comes converted.
    let sdr = Knobs::from_env().amf_full_range.unwrap_or_else(|| {
        pf_encode_win::amf_range::rgb_writes_full_range(pf_gpu::resolve_render_adapter_luid())
    });
    RgbFullRange { sdr, hdr: true }
}

/// [`Resolved::vcn_firmware`] on Windows: AMF keeps its firmware to itself.
pub(crate) fn vcn_firmware(_: &Resolved) -> Option<VcnFirmware> {
    None
}

impl Resolved {
    /// The Windows backend: `auto` → the selected adapter's vendor; a contradicting pin is
    /// overridden ([`resolve_windows_backend`]). Resolved once per inputs; the session plan and
    /// the driver encoder read it.
    pub fn backend(&self) -> WindowsBackend {
        *self
            .probes
            .backend
            .get_or_init(|| windows_resolved_backend(&self.inputs.pref))
    }

    /// True if the codec advertisement comes from a real GPU probe ([`Self::codecs`]) rather
    /// than the static superset. AMF always; QSV with `qsv`; NVENC with `nvenc`.
    fn is_probed(&self) -> bool {
        match self.backend() {
            WindowsBackend::Amf => true,
            WindowsBackend::Qsv => self.inputs.features.qsv,
            WindowsBackend::Nvenc => self.inputs.features.nvenc,
            // MFT enumeration is the probe, and it needs no feature.
            WindowsBackend::MediaFoundation => true,
            WindowsBackend::Software => false,
        }
    }

    /// The resolved backend's codec probe. Ask only when [`Self::is_probed`]. AV1 and HEVC must
    /// be probed, not assumed. `None` while the probe finds nothing; it runs again next ask.
    fn codecs(&self) -> Option<CodecSupport> {
        probe_until_found(
            &self.probes.codecs,
            &self.probes.codecs_missed,
            "Windows",
            || probe_codec_support(self.backend()),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
}

/// Explicit `PUNKTFUNK_ENCODER` pin. `None` for `auto`, unset, and unknown.
fn windows_pinned_backend(pref: &str) -> Option<WindowsBackend> {
    match pref {
        "nvenc" | "hw" | "nvidia" | "cuda" => Some(WindowsBackend::Nvenc),
        "amf" | "amd" => Some(WindowsBackend::Amf),
        "qsv" | "intel" => Some(WindowsBackend::Qsv),
        "mf" | "mediafoundation" => Some(WindowsBackend::MediaFoundation),
        "sw" | "software" | "openh264" => Some(WindowsBackend::Software),
        _ => None,
    }
}

/// The backend for `pref` on the selected adapter. The vendor query runs only to reconcile a
/// pin, so `auto` stays one inventory walk; the MFT walk runs only with no known vendor.
fn windows_resolved_backend(pref: &str) -> WindowsBackend {
    let pinned = windows_pinned_backend(pref);
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
        None if mf::probe_has_hardware_encoder(pf_gpu::resolve_render_adapter_luid()) => {
            WindowsBackend::MediaFoundation
        }
        None => WindowsBackend::Software,
    })
}

/// Encode-GPU vendor from the **selected** render adapter — the same one
/// capture and the IddCx pin sit on. Do not scan DXGI adapter 0: on hybrid
/// boxes that is often the iGPU while textures live on the dGPU. Unknown vendor → first known.
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

/// The probe behind [`Resolved::codecs`], for the backend it resolved.
///
/// AMD: native factory probe (the path the session opens). QSV: native VPL
/// Query. NVIDIA: the driver's GUID list.
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
    if caps.h264 || caps.h265 || caps.av1 {
        tracing::info!(
            ?backend,
            h264 = caps.h264,
            h265 = caps.h265,
            av1 = caps.av1,
            "Windows encode capabilities probed"
        );
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};

    const FULL: Features = Features {
        nvenc: true,
        vulkan_encode: false,
        pyrowave: true,
        qsv: true,
    };

    fn resolved(backend: WindowsBackend, features: Features) -> Resolved {
        let r = Resolved::new(ResolveInputs {
            pref: String::new(),
            gpu: String::new(),
            vulkan_row: false,
            nvidia: false,
            features,
        });
        r.probes
            .backend
            .set(backend)
            .expect("a row fills each cell once");
        r
    }

    /// `wire_caps` reads one resolution (sweep R5-08): software is H.264 without PyroWave, a
    /// probed set is its mask, and a backend this build has no probe for is the superset.
    #[test]
    fn wire_caps_reads_one_resolution() {
        assert_eq!(
            resolved(WindowsBackend::Software, FULL).wire_caps(),
            CODEC_H264
        );
        let amf = resolved(WindowsBackend::Amf, FULL);
        amf.probes
            .codecs
            .set(CodecSupport {
                h264: true,
                h265: true,
                av1: false,
            })
            .expect("unset");
        assert_eq!(amf.wire_caps(), CODEC_H264 | CODEC_HEVC | CODEC_PYROWAVE);
        let qsv = resolved(
            WindowsBackend::Qsv,
            Features {
                qsv: false,
                pyrowave: false,
                ..FULL
            },
        );
        assert_eq!(qsv.wire_caps(), CODEC_H264 | CODEC_HEVC | CODEC_AV1);
        assert!(qsv.advertised().is_none());
    }
}
