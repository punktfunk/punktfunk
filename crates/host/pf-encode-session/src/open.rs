//! Opening a backend for one `SET_ENCODE`: [`open_backend`] is one `open` per
//! [`OpenSpec::backend`], [`open_listed`] walks the request's preference list and answers with
//! the wire reply. The thread that calls them, and where its frames come from, is each side's.
//!
//! PyroWave's private Vulkan instance goes through the box's implicit layers unless the caller
//! disabled them first: overlays hang in session 0, where there is no desktop to hook.

use pf_driver_proto::encode::{
    self as wire, EncoderCapsWire, SetEncodeReply, SetEncodeRequest, backend,
};
use pf_encode_win::{ChromaFormat, Codec, Encoder, EncoderCaps};
use pf_frame::HdrMeta;
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::Interface;

use crate::Fail;
use crate::targets::{InputKind, pixel_format};

pub use pf_driver_proto::encode::backend::NAMES as BACKEND_NAMES;

/// A development knob by name, read per open: the driver answers from the machine
/// environment, the capture worker from its own.
pub type Knob<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The adapter a session's device sits on, as the backends want it named: the LUID for
/// NVENC/AMF/QSV/Media Foundation, the PCI ids for PyroWave (LUIDs are invalid in session 0).
#[derive(Clone, Copy, Debug)]
pub struct AdapterId {
    pub luid: LUID,
    pub vendor_id: u32,
    pub device_id: u32,
}

impl AdapterId {
    /// Read the adapter behind `device` once.
    pub fn of(device: &ID3D11Device) -> Option<Self> {
        // SAFETY: plain queries on a live device; each result is checked before use.
        let desc = unsafe {
            device
                .cast::<IDXGIDevice>()
                .ok()?
                .GetAdapter()
                .ok()?
                .GetDesc()
                .ok()?
        };
        Some(Self {
            luid: desc.AdapterLuid,
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
        })
    }
}

/// A failed `SET_ENCODE` as the wire reply: `status` from the session's domain, the stage tag
/// in `name`.
pub fn fail_reply(status: u32, (error, name): Fail) -> SetEncodeReply {
    let mut reply = SetEncodeReply {
        status,
        error,
        ..bytemuck::Zeroable::zeroed()
    };
    let n = name.len().min(32);
    reply.name[..n].copy_from_slice(&name.as_bytes()[..n]);
    reply
}

/// `pf_frame::HdrMeta` from its 28 `repr(C)` bytes. All zero is the host's `None`: no mastering
/// volume, never a 0-nit one.
pub fn hdr_meta(bytes: &[u8; 28]) -> Option<HdrMeta> {
    if bytes.iter().all(|&b| b == 0) {
        return None;
    }
    // SAFETY: `HdrMeta` is `repr(C)`, 28 bytes of plain integers with no invalid bit pattern;
    // `read_unaligned` copies them out of the request's byte array.
    Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<HdrMeta>()) })
}

/// The request's `open` spec for backend `backend` of its list. The input comes from
/// [`InputKind::choose`], so a 4:4:4 session gets an input that carries it (or a backend whose
/// own caps already say 4:2:0) — never a P010 pick under a reply promising full chroma.
pub fn spec_for(req: &SetEncodeRequest, backend: u32, knob: Knob<'_>) -> Result<OpenSpec, Fail> {
    let (hdr, chroma444) = (req.hdr == 1, req.chroma == 1);
    // 10-bit SDR (depth 10, HDR off) picks a BT.709 P010 input on AMF; `choose` ignores it elsewhere.
    let ten_bit = req.bit_depth >= 10;
    let sdr_fp16 = req.flags & wire::SET_ENCODE_FLAG_SDR_FP16 != 0;
    // `None`: this backend reads no FP16 SDR desktop, so it never opens on one.
    let chosen =
        InputKind::choose(backend, hdr, ten_bit, chroma444, sdr_fp16).ok_or((-4, "fp16"))?;
    // `PFVD_AMF_NV12` / `PFVD_QSV_NV12` (read per open) skip the encoder's own colour
    // conversion and open on the YUV this side converts: the A/B for an encoder whose
    // conversion looks or runs worse.
    let skip = match backend {
        backend::AMF => knob("PFVD_AMF_NV12"),
        backend::QSV => knob("PFVD_QSV_NV12"),
        _ => None,
    };
    let kind = match chosen.fallback(backend) {
        Some(second) if skip.is_some() => second,
        _ => chosen,
    };
    Ok(OpenSpec {
        backend,
        codec: codec_from_wire(req.codec).ok_or((-4, "codec"))?,
        kind,
        width: req.width,
        height: req.height,
        fps: req.fps.max(1),
        bitrate_bps: u64::from(req.bitrate_kbps) * 1000,
        bit_depth: if req.bit_depth >= 10 { 10 } else { 8 },
        chroma: if chroma444 {
            ChromaFormat::Yuv444
        } else {
            ChromaFormat::Yuv420
        },
    })
}

fn caps_wire(c: EncoderCaps) -> EncoderCapsWire {
    EncoderCapsWire {
        supports_rfi: u32::from(c.supports_rfi),
        chroma_444: u32::from(c.chroma_444),
        intra_refresh: u32::from(c.intra_refresh),
        intra_refresh_recovery: u32::from(c.intra_refresh_recovery),
        intra_refresh_period: c.intra_refresh_period,
        blends_cursor: u32::from(c.blends_cursor),
    }
}

/// Walk the request's backend list in order; the first that opens wins. A backend that
/// refuses its input is tried down its [`InputKind::fallback`] chain before the next one.
/// `Err` is the last failure as the wire reply — no silent fallback past the list.
/// `open_backend` returns a backend whose session already exists, so `reply.caps` describes
/// the live encoder rather than its defaults — the host reads those caps once.
pub fn open_listed(
    req: &SetEncodeRequest,
    adapter: &AdapterId,
    device: &ID3D11Device,
    knob: Knob<'_>,
) -> Result<(Box<dyn Encoder>, OpenSpec, SetEncodeReply), SetEncodeReply> {
    let mut last: Fail = (-1, "nobackend");
    for &backend in req.backends.iter().take_while(|&&b| b != 0) {
        let first = match spec_for(req, backend, knob) {
            Ok(s) => s,
            Err(f) => {
                last = f;
                continue;
            }
        };
        let ladder = std::iter::successors(Some(first), |s| {
            s.kind.fallback(backend).map(|kind| OpenSpec { kind, ..*s })
        });
        for spec in ladder {
            match open_backend(&spec, adapter, device, knob) {
                Ok(mut enc) => {
                    if req.wire_chunk_bytes != 0 {
                        enc.set_wire_chunking(req.wire_chunk_bytes as usize);
                    }
                    if req.hdr == 1 {
                        enc.set_hdr_meta(hdr_meta(&req.hdr_meta));
                    }
                    let applied = enc.applied_bitrate_bps().unwrap_or(spec.bitrate_bps);
                    let mut reply = fail_reply(
                        wire::SET_ENCODE_OK,
                        (0, BACKEND_NAMES[backend as usize - 1]),
                    );
                    reply.backend_opened = backend;
                    reply.caps = caps_wire(enc.caps());
                    reply.applied_bitrate_kbps = (applied / 1000) as u32;
                    return Ok((enc, spec, reply));
                }
                Err(f) => last = f,
            }
        }
    }
    Err(fail_reply(wire::SET_ENCODE_NO_BACKEND, last))
}

/// Everything one backend `open` takes, in the backends' own vocabulary.
#[derive(Clone, Copy, Debug)]
pub struct OpenSpec {
    /// 1 NVENC, 2 AMF, 3 QSV, 4 PyroWave, 5 Media Foundation.
    pub backend: u32,
    pub codec: Codec,
    pub kind: InputKind,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u64,
    pub bit_depth: u8,
    pub chroma: ChromaFormat,
}

/// The wire codec numbering (1 H264, 2 HEVC, 3 AV1, 4 PyroWave).
pub fn codec_from_wire(codec: u32) -> Option<Codec> {
    Some(match codec {
        1 => Codec::H264,
        2 => Codec::H265,
        3 => Codec::Av1,
        4 => Codec::PyroWave,
        _ => return None,
    })
}

/// One backend open on `adapter`. `Err` carries the stage tag; the backend's message is logged.
/// `device` is the one the source's frames carry: NVENC opens its session against it here so
/// the caller's caps read describes the hardware.
pub fn open_backend(
    spec: &OpenSpec,
    adapter: &AdapterId,
    device: &ID3D11Device,
    knob: Knob<'_>,
) -> Result<Box<dyn Encoder>, Fail> {
    // NVENC is the only arm that opens against the device or reads a knob.
    #[cfg(not(feature = "nvenc"))]
    let _ = (device, knob);
    let (w, h, fps, bps) = (spec.width, spec.height, spec.fps, spec.bitrate_bps);
    let (depth, chroma) = (spec.bit_depth, spec.chroma);
    let format = pixel_format(spec.kind);
    // P010 serves both HDR and 10-bit SDR; the kind is the only thing that tells them apart, so
    // the backend's colour signalling follows it, not the (identical) P010 pixel label.
    let hdr = matches!(
        spec.kind,
        InputKind::P010 | InputKind::Rgb10 | InputKind::Fp16 | InputKind::Planar { hdr: true, .. }
    );
    let luid = Some(adapter.luid);
    // NVENC, QSV and PyroWave are x86-64 only: a side built without their feature refuses
    // their ids here and the host falls through to Media Foundation.
    let opened: anyhow::Result<Box<dyn Encoder>> = match spec.backend {
        #[cfg(feature = "nvenc")]
        backend::NVENC => pf_encode_win::nvenc::NvencD3d11Encoder::open(
            spec.codec, format, w, h, fps, bps, depth, chroma, 1, luid,
        )
        .and_then(|mut e| {
            // The drive loop parks on handles, so the session opens async and hands its
            // completion events out through `ready_event` — no retrieve thread, no sampling.
            // `PFVD_NVENC_EVENTS=0` (read per open) falls back to the sync session and the
            // loop's bounded-poll arm: the A/B for a GPU whose async encode retires slower
            // than its sync one.
            e.use_completion_events(knob("PFVD_NVENC_EVENTS").as_deref() != Some("0"));
            // All three of these defer their session to the first frame, and the host reads the
            // caps in our reply once per session: open it here or it caches the defaults.
            e.prepare_d3d11(device, format, w, h)?;
            Ok(Box::new(e) as Box<dyn Encoder>)
        }),
        backend::AMF => pf_encode_win::amf::AmfEncoder::open(
            spec.codec, format, w, h, fps, bps, depth, chroma, hdr, luid,
        )
        .and_then(|mut e| {
            e.prepare(device)?;
            Ok(Box::new(e) as Box<dyn Encoder>)
        }),
        #[cfg(feature = "qsv")]
        backend::QSV => pf_encode_win::qsv::QsvEncoder::open(
            spec.codec, format, w, h, fps, bps, depth, chroma, hdr, luid,
        )
        .and_then(|mut e| {
            e.prepare(device)?;
            Ok(Box::new(e) as Box<dyn Encoder>)
        }),
        #[cfg(feature = "pyrowave")]
        backend::PYROWAVE => {
            // Implicit layers are the caller's to disable, before any thread could race it.
            pf_encode_win::pyrowave::PyroWaveEncoder::open(
                w,
                h,
                fps,
                bps,
                chroma,
                depth,
                hdr,
                adapter.vendor_id,
                adapter.device_id,
            )
            .map(|e| Box::new(e) as Box<dyn Encoder>)
        }
        backend::MEDIA_FOUNDATION => pf_encode_win::mf::MfEncoder::open(
            spec.codec, format, w, h, fps, bps, depth, chroma, luid,
        )
        .map(|e| Box::new(e) as Box<dyn Encoder>),
        _ => return Err((-1, "backend")),
    };
    opened.map_err(|e| {
        tracing::info!("encode: backend {} open FAILED: {e:#}", spec.backend);
        (-1, "open")
    })
}

pub fn qpc_now() -> u64 {
    let mut qpc = 0i64;
    // SAFETY: plain FFI; `qpc` is a valid local out-param.
    let _ = unsafe { QueryPerformanceCounter(&mut qpc) };
    qpc as u64
}

pub fn qpc_frequency() -> u64 {
    let mut hz = 0i64;
    // SAFETY: plain FFI; `hz` is a valid local out-param.
    let _ = unsafe { QueryPerformanceFrequency(&mut hz) };
    (hz as u64).max(1)
}

pub fn qpc_to_ns(qpc: u64, hz: u64) -> u64 {
    (u128::from(qpc) * 1_000_000_000 / u128::from(hz)) as u64
}
