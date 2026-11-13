//! Backend resolution: which encoder a session opens and what it can encode. The host calls
//! [`resolve`] once per entry and asks the [`Resolved`] it returns; [`crate::open_video`] opens
//! the backend that value names, so the open and the advertisement read the same answer.
//! `linux.rs` and `windows.rs` answer per OS; any other target answers from the stub arm below.
//! The rules: `docs/adr/0002-encode-backend-resolution.md`.

use crate::*;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

// One module per OS, each answering the same `pub(crate)` questions; Linux's free public
// functions re-export from here.
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "windows")]
use windows as imp;

pub(crate) use imp::open;
#[cfg(target_os = "linux")]
pub use imp::{anv_video_encode_flag, open_software_h264};

/// No GPU encode backend on this target: nothing is probed and nothing opens.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod imp {
    use super::*;
    use crate::env_knobs::Knobs;

    /// Nothing to probe here.
    #[derive(Debug, Default)]
    pub(crate) struct Probes;

    pub(crate) fn open(
        _: &Resolved,
        _: &OpenParams,
        _: &Knobs,
    ) -> Result<(Box<dyn Encoder>, &'static str)> {
        anyhow::bail!("video encode requires Linux or Windows")
    }

    /// The unprobed advertisement: H.264 for a software pin, else HEVC.
    pub(crate) fn wire_caps(r: &Resolved) -> u8 {
        if is_software(&r.inputs.pref) {
            punktfunk_core::quic::CODEC_H264
        } else {
            punktfunk_core::quic::CODEC_HEVC
        }
    }

    pub(crate) fn advertised(_: &Resolved) -> Option<CodecSupport> {
        None
    }

    pub(crate) fn hevc_444(_: &Resolved) -> Option<bool> {
        Some(false)
    }

    pub(crate) fn ten_bit(_: &Resolved, _codec: Codec) -> Option<bool> {
        Some(false)
    }

    pub(crate) fn sdr10(_: &Resolved, _codec: Codec) -> bool {
        false
    }

    pub(crate) fn rgb_full_range(_: &Resolved, _codec: Codec) -> RgbFullRange {
        RgbFullRange::BOTH
    }

    pub(crate) fn vcn_firmware(_: &Resolved) -> Option<VcnFirmware> {
        None
    }

    /// No resolver: only a software pin is a CPU path.
    pub(crate) fn is_gpu(r: &Resolved) -> bool {
        !is_software(&r.inputs.pref)
    }

    pub(crate) fn ingests_rgb_444(_: &Resolved) -> bool {
        false
    }

    fn is_software(pref: &str) -> bool {
        matches!(pref, "software" | "sw" | "openh264")
    }
}

/// The colours a backend's own RGB→YUV conversion writes at full range when asked to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RgbFullRange {
    pub sdr: bool,
    pub hdr: bool,
}

impl RgbFullRange {
    pub const BOTH: Self = RgbFullRange {
        sdr: true,
        hdr: true,
    };

    /// The answer for a session at `hdr`.
    pub fn at(self, hdr: bool) -> bool {
        if hdr {
            self.hdr
        } else {
            self.sdr
        }
    }
}

/// The encode features this build compiled in, as data, so a table row can take one out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Features {
    pub(crate) nvenc: bool,
    pub(crate) vulkan_encode: bool,
    pub(crate) pyrowave: bool,
    pub(crate) qsv: bool,
}

impl Features {
    pub(crate) const BUILT: Features = Features {
        nvenc: cfg!(feature = "nvenc"),
        vulkan_encode: cfg!(feature = "vulkan-encode"),
        pyrowave: cfg!(feature = "pyrowave"),
        qsv: cfg!(feature = "qsv"),
    };
}

/// Everything a resolution depends on. The cache key is the whole value, so a change to any
/// field resolves afresh and no answer outlives its inputs.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ResolveInputs {
    /// The Encoder row (`PUNKTFUNK_ENCODER`, lower-cased) as the host config holds it.
    pub(crate) pref: String,
    /// [`pf_gpu::selection_key`]: the selected GPU, a manual pick included.
    pub(crate) gpu: String,
    /// The Vulkan encoding row (`PUNKTFUNK_VULKAN_ENCODE`, default on). Linux only.
    pub(crate) vulkan_row: bool,
    /// NVIDIA's device nodes exist, which `auto` reads. Linux only.
    pub(crate) nvidia: bool,
    pub(crate) features: Features,
}

impl ResolveInputs {
    /// The inputs in force now. Probes nothing: `selection_key` already stats the NVIDIA nodes.
    fn current() -> ResolveInputs {
        ResolveInputs {
            pref: pf_host_config::config().encoder_pref.clone(),
            gpu: pf_gpu::selection_key(),
            vulkan_row: cfg!(target_os = "linux")
                && pf_host_config::row_bool("PUNKTFUNK_VULKAN_ENCODE"),
            #[cfg(target_os = "linux")]
            nvidia: pf_gpu::nvidia_present(),
            #[cfg(not(target_os = "linux"))]
            nvidia: false,
            features: Features::BUILT,
        }
    }
}

/// One backend resolution and what its probes found. A probe runs the first time a method needs
/// it and is kept for these inputs; building the value runs none. Get one from [`resolve`].
#[derive(Debug)]
pub struct Resolved {
    inputs: ResolveInputs,
    #[cfg_attr(not(any(target_os = "linux", target_os = "windows")), allow(dead_code))]
    probes: imp::Probes,
    /// [`Resolved::can_encode_444`]'s verdict, so its line logs once per inputs.
    hevc_444: OnceLock<bool>,
    /// [`Resolved::can_encode_10bit`]'s, HEVC then AV1.
    ten_bit: [OnceLock<bool>; 2],
}

/// The HEVC/AV1 slot of a per-codec cell; `None` for a codec no cell holds.
fn codec_slot(codec: Codec) -> Option<usize> {
    match codec {
        Codec::H265 => Some(0),
        Codec::Av1 => Some(1),
        _ => None,
    }
}

impl Resolved {
    /// A resolution of `inputs` with every probe still to run.
    pub(crate) fn new(inputs: ResolveInputs) -> Resolved {
        Resolved {
            inputs,
            probes: Default::default(),
            hevc_444: OnceLock::new(),
            ten_bit: Default::default(),
        }
    }

    /// `quic::CODEC_*` bits this host can emit on the native path, fed to
    /// [`punktfunk_core::quic::resolve_codec`]. Software is H.264 only; probed backends
    /// advertise what the GPU encodes. An empty probe means the GPU was unusable at probe time,
    /// not that it encodes nothing, so it falls back to the superset.
    pub fn wire_caps(&self) -> u8 {
        imp::wire_caps(self)
    }

    /// The codec the Encoder row names, as a `quic::CODEC_*` bit, for a client that states no
    /// preference. `0` when the row names a backend; only `pyrowave` names a codec.
    pub fn pinned_codec(&self) -> u8 {
        if self.inputs.pref == "pyrowave" {
            punktfunk_core::quic::CODEC_PYROWAVE
        } else {
            0
        }
    }

    /// The probed codec set the GameStream advertisement narrows to, the one [`Self::wire_caps`]
    /// reads. `None` when the backend is not probed: the caller keeps its static superset.
    pub fn advertised(&self) -> Option<CodecSupport> {
        imp::advertised(self)
    }

    /// Whether the backend can emit 4:4:4 for `codec`, asked before Welcome. 4:4:4 is HEVC and
    /// PyroWave only; VAAPI/AMF/QSV must be probed, never assumed. A verdict whose probe could
    /// not ask is `false` for this call and asked again next time.
    pub fn can_encode_444(&self, codec: Codec) -> bool {
        match codec {
            // Own RGB→YCbCr CSC from a full-chroma source — no GPU encode probe.
            // See `design/pyrowave-444-hdr.md`.
            Codec::PyroWave => cfg!(any(target_os = "linux", target_os = "windows")),
            Codec::H265 if cfg!(any(target_os = "linux", target_os = "windows")) => {
                if let Some(v) = self.hevc_444.get() {
                    return *v;
                }
                imp::hevc_444(self).is_some_and(|supported| {
                    *self.hevc_444.get_or_init(|| {
                        tracing::info!(supported, "HEVC 4:4:4 encode capability probed");
                        supported
                    })
                })
            }
            _ => false,
        }
    }

    /// Whether the backend can emit 10-bit for `codec` (HEVC Main10 / AV1), asked before
    /// Welcome. Without this gate `PUNKTFUNK_10BIT` would negotiate 10-bit and then emit 8-bit
    /// (label HDR / stream SDR). A verdict whose probe could not ask is `false` for this call
    /// and asked again next time.
    pub fn can_encode_10bit(&self, codec: Codec) -> bool {
        if !codec.supports_10bit() || !cfg!(any(target_os = "linux", target_os = "windows")) {
            return false;
        }
        if codec == Codec::PyroWave {
            // Wavelet is depth-agnostic; the CSC runs on the encoder's own Vulkan device, so
            // 10-bit needs no encode-profile probe — just the `pyrowave` backend existing on
            // this OS. See `design/pyrowave-444-hdr.md`.
            return cfg!(target_os = "windows") || self.inputs.features.pyrowave;
        }
        let Some(slot) = codec_slot(codec) else {
            return false;
        };
        if let Some(v) = self.ten_bit[slot].get() {
            return *v;
        }
        imp::ten_bit(self, codec).is_some_and(|supported| {
            *self.ten_bit[slot].get_or_init(|| {
                tracing::info!(codec = ?codec, supported, "10-bit encode capability probed");
                supported
            })
        })
    }

    /// GPU-resident frames (software is the only CPU path). Single source for
    /// [`pf_frame::OutputFormat`]'s `gpu` bit — capture must not re-derive it.
    pub fn is_gpu(&self) -> bool {
        imp::is_gpu(self)
    }

    /// Encoder half of the 4:4:4 capture gate: ingest RGB and CSC to 4:4:4.
    /// Only Windows NVENC. Linux 4:4:4 is capture-side (portal RGB → `yuv444p`).
    pub fn ingests_rgb_444(&self) -> bool {
        imp::ingests_rgb_444(self)
    }

    /// Encoder half of the 10-bit SDR gate: this backend writes a 10-bit stream from the 8-bit
    /// surface an SDR desktop captures. The GPU still has to pass [`Self::can_encode_10bit`].
    ///
    /// Windows NVENC widens the driver's BGRA itself; Windows AMF and QSV take a BT.709 P010 the
    /// driver produces, for HEVC and AV1. Linux direct-NVENC takes the plain 8-bit surface
    /// and asks NVENC for 10-bit output. Linux VAAPI carries HEVC and Vulkan Video carries AV1,
    /// both with the RGB→YUV matrix following the BT.709 colour, not depth.
    pub fn carries_sdr10(&self, codec: Codec) -> bool {
        imp::sdr10(self, codec)
    }

    /// The colours the backend's own RGB→YUV conversion writes at full range for `codec`.
    /// Short of both on Windows AMF where the GPU writes studio whatever is asked (RDNA4),
    /// measured once per adapter, and on Linux where the VCN firmware converts BT.709 at
    /// narrow range only ([`VcnFirmware::efc_two_pairs`]). Every other conversion is ours or
    /// follows the request.
    pub fn rgb_full_range(&self, codec: Codec) -> RgbFullRange {
        imp::rgb_full_range(self, codec)
    }

    /// The firmware of the video chip Vulkan Video encodes on. `None` off AMD, and where
    /// another backend encodes.
    pub fn vcn_firmware(&self) -> Option<VcnFirmware> {
        imp::vcn_firmware(self)
    }
}

/// The resolution for the inputs in force now. One value per inputs for the process: a change
/// to the Encoder row, the selected GPU or the Vulkan row resolves afresh.
pub fn resolve() -> Arc<Resolved> {
    static CACHE: LazyLock<Mutex<HashMap<ResolveInputs, Arc<Resolved>>>> =
        LazyLock::new(Default::default);
    resolve_in(&CACHE, ResolveInputs::current())
}

/// [`resolve`] over `cache`. Building probes nothing, so it runs under the lock.
fn resolve_in(
    cache: &Mutex<HashMap<ResolveInputs, Arc<Resolved>>>,
    inputs: ResolveInputs,
) -> Arc<Resolved> {
    let mut cache = cache.lock().unwrap();
    if let Some(r) = cache.get(&inputs) {
        return r.clone();
    }
    let r = Arc::new(Resolved::new(inputs.clone()));
    cache.insert(inputs, r.clone());
    r
}

/// `probe` until it finds a codec. `None` leaves `cell` empty, so the next ask probes again,
/// and warns once per `missed`; the caller advertises its superset meanwhile.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn probe_until_found(
    cell: &OnceLock<CodecSupport>,
    missed: &OnceLock<()>,
    what: &'static str,
    probe: impl FnOnce() -> CodecSupport,
) -> Option<CodecSupport> {
    if let Some(caps) = cell.get() {
        return Some(*caps);
    }
    let caps = probe();
    if caps.h264 || caps.h265 || caps.av1 {
        return Some(*cell.get_or_init(|| caps));
    }
    missed.get_or_init(|| {
        tracing::warn!(
            probe = what,
            "encode probe found no codec — advertising the superset and probing again on the \
             next ask"
        );
    });
    None
}

/// `quic::CODEC_*` bits of a [`CodecSupport`] probe, or `None` when it found
/// nothing — GPU unusable at probe time, not "zero codecs". Caller falls back
/// to the static superset.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) fn codec_support_wire_mask(caps: CodecSupport) -> Option<u8> {
    let mut m = 0u8;
    if caps.h264 {
        m |= punktfunk_core::quic::CODEC_H264;
    }
    if caps.h265 {
        m |= punktfunk_core::quic::CODEC_HEVC;
    }
    if caps.av1 {
        m |= punktfunk_core::quic::CODEC_AV1;
    }
    (m != 0).then_some(m)
}

// Ungated: the pure reconciliation table is tested on every CI leg, not just Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsBackend {
    Nvenc,
    Amf,
    Qsv,
    /// Media Foundation: any vendor's hardware MFT. Second rung under the native SDKs on
    /// x64, and the only hardware encoder on an Adreno adapter.
    MediaFoundation,
    Software,
}

/// PCI vendor a Windows hardware backend can open on (`None` for software).
/// Capture, virtual display, and encoder share one adapter.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn windows_backend_vendor_id(backend: WindowsBackend) -> Option<u32> {
    match backend {
        WindowsBackend::Nvenc => Some(pf_gpu::VENDOR_NVIDIA),
        WindowsBackend::Amf => Some(pf_gpu::VENDOR_AMD),
        WindowsBackend::Qsv => Some(pf_gpu::VENDOR_INTEL),
        // Vendor-agnostic: every x64 vendor and Adreno ship an MFT, so an `mf` pin is
        // never contradicted by the selected adapter.
        WindowsBackend::MediaFoundation | WindowsBackend::Software => None,
    }
}

/// Pure half of the Windows backend resolution: reconcile an explicit `PUNKTFUNK_ENCODER` pin
/// with the selected adapter. A hardware pin whose vendor contradicts the selected GPU is
/// overridden (`derived` is lazy) — honoring it can only fail. Software has no vendor and is
/// always honored; with no selected GPU a pin is trusted as-is. Compiled everywhere so its
/// table runs on every CI leg.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn resolve_windows_backend(
    pinned: Option<WindowsBackend>,
    selected_vendor_id: Option<u32>,
    derived: impl FnOnce() -> WindowsBackend,
) -> WindowsBackend {
    match (pinned, selected_vendor_id) {
        (None, _) => derived(),
        (Some(pin), Some(vendor)) => match windows_backend_vendor_id(pin) {
            Some(required) if required != vendor => derived(),
            _ => pin,
        },
        (Some(pin), None) => pin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin whose vendor contradicts the selected GPU is overridden — never
    /// "pin + proceed" (that feeds the reset ladder a deterministic failure).
    #[test]
    fn encoder_pin_reconciles_against_the_selected_adapter() {
        use WindowsBackend::*;
        let derived = |b: WindowsBackend| move || b;
        let unreachable = || -> WindowsBackend { panic!("derived must not be consulted") };
        assert_eq!(
            resolve_windows_backend(Some(Qsv), Some(pf_gpu::VENDOR_NVIDIA), derived(Nvenc)),
            Nvenc
        );
        assert_eq!(
            resolve_windows_backend(Some(Qsv), Some(pf_gpu::VENDOR_INTEL), unreachable),
            Qsv
        );
        assert_eq!(
            resolve_windows_backend(Some(Nvenc), None, unreachable),
            Nvenc
        );
        assert_eq!(
            resolve_windows_backend(Some(Software), Some(pf_gpu::VENDOR_NVIDIA), unreachable),
            Software
        );
        assert_eq!(
            resolve_windows_backend(None, Some(pf_gpu::VENDOR_AMD), derived(Amf)),
            Amf
        );
        assert_eq!(
            resolve_windows_backend(None, None, derived(Software)),
            Software
        );
        // MF has no vendor, so no adapter can contradict the pin — that is the whole
        // point of the rung: it opens on NVIDIA, AMD, Intel, and Adreno alike.
        for vendor in [
            pf_gpu::VENDOR_NVIDIA,
            pf_gpu::VENDOR_AMD,
            pf_gpu::VENDOR_INTEL,
            pf_gpu::VENDOR_QUALCOMM,
        ] {
            assert_eq!(
                resolve_windows_backend(Some(MediaFoundation), Some(vendor), unreachable),
                MediaFoundation
            );
        }
        assert_eq!(
            resolve_windows_backend(
                None,
                Some(pf_gpu::VENDOR_QUALCOMM),
                derived(MediaFoundation)
            ),
            MediaFoundation
        );
        assert_eq!(windows_backend_vendor_id(MediaFoundation), None);
    }

    /// The cache key is the whole input: the same inputs share one resolution, and a change to
    /// the Encoder row or the selected GPU resolves afresh.
    #[test]
    fn the_same_inputs_share_one_resolution() {
        let cache = Mutex::new(HashMap::new());
        let inputs = |pref: &str| ResolveInputs {
            pref: pref.into(),
            gpu: "card0".into(),
            vulkan_row: true,
            nvidia: false,
            features: Features::BUILT,
        };
        let auto = resolve_in(&cache, inputs("auto"));
        assert!(Arc::ptr_eq(&auto, &resolve_in(&cache, inputs("auto"))));
        assert!(!Arc::ptr_eq(&auto, &resolve_in(&cache, inputs("vaapi"))));
        let other_gpu = ResolveInputs {
            gpu: "card1".into(),
            ..inputs("auto")
        };
        assert!(!Arc::ptr_eq(&auto, &resolve_in(&cache, other_gpu)));
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn codec_support_wire_mask_maps_the_probe_to_bits() {
        use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC};
        let all = CodecSupport {
            h264: true,
            h265: true,
            av1: true,
        };
        assert_eq!(
            codec_support_wire_mask(all),
            Some(CODEC_H264 | CODEC_HEVC | CODEC_AV1)
        );
        let hevc_only = CodecSupport {
            h264: false,
            h265: true,
            av1: false,
        };
        assert_eq!(codec_support_wire_mask(hevc_only), Some(CODEC_HEVC));
        // All-false = GPU unusable, not "zero codecs" — `None` → static superset.
        let none = CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        };
        assert_eq!(codec_support_wire_mask(none), None);
    }

    /// A codec set that found nothing is not kept: the next ask probes again, and the miss
    /// warns once (sweep R5-03). A found set is kept and never probed again.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn a_probe_that_found_nothing_runs_again() {
        let (cell, missed) = (OnceLock::new(), OnceLock::new());
        let nothing = CodecSupport {
            h264: false,
            h265: false,
            av1: false,
        };
        let hevc = CodecSupport {
            h264: false,
            h265: true,
            av1: false,
        };
        assert!(probe_until_found(&cell, &missed, "test", || nothing).is_none());
        assert!(cell.get().is_none() && missed.get().is_some());
        let found = probe_until_found(&cell, &missed, "test", || hevc).expect("found");
        assert!(found.h265 && !found.h264);
        let kept = probe_until_found(&cell, &missed, "test", || panic!("a found set is kept"));
        assert!(kept.is_some_and(|c| c.h265));
    }
}
