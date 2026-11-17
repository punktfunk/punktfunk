//! Whether a GPU's own RGB→YUV conversion, the one AMF runs on a BGRA input, writes the range
//! the encoder asks for. RDNA4 writes studio samples whatever it is told, under the full-range
//! flag the encoder sets (RX 9070 XT measured, RX 9060 XT reported); RDNA2 and RDNA3 write what
//! they are told. A session on such a GPU advertises and encodes studio, so every client shows
//! what arrives, and the encoder keeps reading the desktop's own surface.
//!
//! The answer is measured, not looked up: one half-black, half-white picture through AMF on the
//! render adapter, decoded back with Windows' own H.264 decoder.

use crate::amf::AmfEncoder;
use crate::mf::{com_init_mta, mf_startup};
use crate::{ChromaFormat, Codec, Encoder};
use anyhow::{anyhow, bail, Context, Result};
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::mem::ManuallyDrop;
use std::sync::Mutex;
use std::time::Duration;
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, IMFSample, IMFTransform, MFCreateMediaType, MFCreateMemoryBuffer,
    MFCreateSample, MFMediaType_Video, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};

/// The probe picture, black on the left half and white on the right. 256×128 clears every
/// VCN's smallest H.264 frame.
pub(crate) const W: u32 = 256;
pub(crate) const H: u32 = 128;

/// How long one measurement may take before the GPU counts as studio: a wedged driver must not
/// hold a session's start.
const BUDGET: Duration = Duration::from_secs(5);

/// The black and white the probe decoded back, as 8-bit luma.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Levels {
    pub black: u8,
    pub white: u8,
}

impl Levels {
    /// Full range decodes black near 0 and white near 255; studio near 16 and 235.
    pub fn full_range(self) -> bool {
        self.black <= 8 && self.white >= 247
    }
}

/// A LUID as `(LowPart, HighPart)`; `None` is the OS default adapter.
type AdapterKey = Option<(u32, i32)>;

/// `true` when AMF's RGB conversion on `adapter_luid` writes full range. Measured once per
/// adapter for the process. Undecided — no AMF, no decoder, no answer in [`BUDGET`] — is
/// `false`: studio shows right on every GPU, full only where it was seen.
pub fn rgb_writes_full_range(adapter_luid: Option<LUID>) -> bool {
    static SEEN: Mutex<Vec<(AdapterKey, bool)>> = Mutex::new(Vec::new());
    let key: AdapterKey = adapter_luid.map(|l| (l.LowPart, l.HighPart));
    // Held across the measurement, so a second session waits for the first one's answer.
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, full)) = seen.iter().find(|(k, _)| *k == key) {
        return full;
    }
    let full = match measure_bounded(adapter_luid) {
        Ok(l) => {
            tracing::info!(
                black = l.black,
                white = l.white,
                full_range = l.full_range(),
                "AMF RGB conversion range measured on the render GPU"
            );
            l.full_range()
        }
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "AMF RGB conversion range not measured: this GPU's 8-bit streams use studio range"
            );
            false
        }
    };
    seen.push((key, full));
    full
}

/// [`measure`] on its own thread: the decoder needs the multithreaded apartment, which a
/// caller's thread may already have refused, and a stuck driver gives up only the thread.
fn measure_bounded(adapter_luid: Option<LUID>) -> Result<Levels> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("pf-amf-range".into())
        .spawn(move || {
            let _ = tx.send(measure(adapter_luid));
        })
        .context("spawn the range probe")?;
    rx.recv_timeout(BUDGET)
        .map_err(|_| anyhow!("no answer within {BUDGET:?}"))?
}

/// Encode the probe picture through AMF as a session does on BGRA, decode it back and read the
/// middle of each half.
pub fn measure(adapter_luid: Option<LUID>) -> Result<Levels> {
    let device = pf_frame::dxgi::probe_device(adapter_luid, Default::default())
        .ok_or_else(|| anyhow!("no D3D11 device on the render adapter"))?;
    let texture = bars_texture(&device)?;
    let mut enc = AmfEncoder::open(
        Codec::H264,
        PixelFormat::Bgra,
        W,
        H,
        30,
        2_000_000,
        8,
        ChromaFormat::Yuv420,
        false,
        adapter_luid,
    )?;
    let mut aus = Vec::new();
    for pts in 1..=2 {
        enc.submit(&CapturedFrame {
            provenance: Default::default(),
            width: W,
            height: H,
            pts_ns: pts,
            format: PixelFormat::Bgra,
            payload: FramePayload::D3d11(pf_frame::dxgi::D3d11Frame {
                texture: texture.clone(),
                device: device.clone(),
                pyro: None,
            }),
            cursor: None,
        })?;
        aus.extend(enc.poll()?);
    }
    enc.flush()?;
    while let Some(au) = enc.poll()? {
        aus.push(au);
    }
    let first = aus
        .first()
        .ok_or_else(|| anyhow!("AMF returned no access unit"))?;
    let luma = decode_h264_luma(&first.data)?;
    Ok(Levels {
        black: patch(&luma, W / 4, H / 2),
        white: patch(&luma, 3 * W / 4, H / 2),
    })
}

/// The probe picture as a BGRA texture on `device`.
pub(crate) fn bars_texture(device: &ID3D11Device) -> Result<ID3D11Texture2D> {
    let px: Vec<u8> = (0..H)
        .flat_map(|_| {
            (0..W).flat_map(|x| {
                if x < W / 2 {
                    [0, 0, 0, 255]
                } else {
                    [255, 255, 255, 255]
                }
            })
        })
        .collect();
    let desc = D3D11_TEXTURE2D_DESC {
        Width: W,
        Height: H,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: px.as_ptr().cast(),
        SysMemPitch: W * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: `desc` and `init` are fully initialised locals and `px` outlives the call, which
    // copies the pixels in.
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }
        .context("CreateTexture2D (range probe)")?;
    texture.ok_or_else(|| anyhow!("CreateTexture2D returned no texture"))
}

/// Mean of the 16×16 luma block centred on (`cx`, `cy`).
pub(crate) fn patch(luma: &[u8], cx: u32, cy: u32) -> u8 {
    let sum: u32 = (cy - 8..cy + 8)
        .flat_map(|y| (cx - 8..cx + 8).map(move |x| u32::from(luma[(y * W + x) as usize])))
        .sum();
    (sum / 256) as u8
}

/// One H.264 IDR through Windows' software H.264 decoder: its luma plane, `W`×`H`.
pub(crate) fn decode_h264_luma(au: &[u8]) -> Result<Vec<u8>> {
    mf_startup()?;
    com_init_mta();
    // SAFETY: every MF object here is owned by this function and used on this thread. Locked
    // buffer pointers are written or read only within the length the buffer reports, and
    // `MFT_OUTPUT_DATA_BUFFER`'s `ManuallyDrop` members are reclaimed with `take` on every path.
    unsafe {
        let mft: IMFTransform =
            CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                .context("create the Windows H.264 decoder")?;
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, (u64::from(W) << 32) | u64::from(H))?;
        mft.SetInputType(0, &input, 0)
            .context("decoder input type")?;
        set_nv12_output(&mft)?;
        mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
        let len = u32::try_from(au.len()).context("access unit size")?;
        let buffer = MFCreateMemoryBuffer(len)?;
        let mut p = std::ptr::null_mut();
        buffer.Lock(&mut p, None, None)?;
        std::ptr::copy_nonoverlapping(au.as_ptr(), p, au.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(len)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(0)?;
        mft.ProcessInput(0, &sample, 0).context("decoder input")?;
        mft.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        // A format change comes first, then the picture.
        for _ in 0..4 {
            let info = mft.GetOutputStreamInfo(0)?;
            let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            let ours = if provides {
                None
            } else {
                let out = MFCreateMemoryBuffer(info.cbSize.max(W * H * 3 / 2))?;
                let s = MFCreateSample()?;
                s.AddBuffer(&out)?;
                Some(s)
            };
            let mut out = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(ours),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let call = mft.ProcessOutput(0, &mut out, &mut status);
            let picture: Option<IMFSample> = ManuallyDrop::take(&mut out[0].pSample);
            let _events = ManuallyDrop::take(&mut out[0].pEvents);
            match call {
                Ok(()) => {
                    let picture =
                        picture.ok_or_else(|| anyhow!("the decoder returned no sample"))?;
                    return luma_of(&mft, &picture);
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => set_nv12_output(&mft)?,
                Err(e) => return Err(e).context("decoder output"),
            }
        }
        bail!("the decoder returned no picture")
    }
}

/// Pick the decoder's NV12 output type.
///
/// # Safety
/// `mft` is a live transform owned by the calling thread.
unsafe fn set_nv12_output(mft: &IMFTransform) -> Result<()> {
    for i in 0.. {
        // SAFETY: the caller's contract; the type is owned here and handed back to the MFT.
        let t = unsafe { mft.GetOutputAvailableType(0, i) }.context("decoder output types")?;
        // SAFETY: an attribute read on a live media type.
        if unsafe { t.GetGUID(&MF_MT_SUBTYPE) }? == MFVideoFormat_NV12 {
            // SAFETY: as above.
            return unsafe { mft.SetOutputType(0, &t, 0) }.context("decoder NV12 output");
        }
    }
    unreachable!("the range ends only with an error")
}

/// The luma plane of a decoded NV12 sample, `W`×`H`, at the output type's stride.
///
/// # Safety
/// `mft` and `picture` are live and owned by the calling thread.
unsafe fn luma_of(mft: &IMFTransform, picture: &IMFSample) -> Result<Vec<u8>> {
    // SAFETY: the caller's contract. The locked pointer is read only inside the length the
    // same call reported, and the buffer is unlocked before it drops.
    unsafe {
        let stride = mft
            .GetOutputCurrentType(0)
            .and_then(|t| t.GetUINT32(&MF_MT_DEFAULT_STRIDE))
            .map_or(W, |s| s as i32 as u32);
        let buffer = picture.ConvertToContiguousBuffer()?;
        let mut p: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut p, None, Some(&mut len))?;
        let need = (stride * (H - 1) + W) as usize;
        let luma = if p.is_null() || (len as usize) < need || stride < W {
            None
        } else {
            let all = std::slice::from_raw_parts(p, len as usize);
            Some(
                (0..H)
                    .flat_map(|y| all[(y * stride) as usize..(y * stride + W) as usize].iter())
                    .copied()
                    .collect::<Vec<u8>>(),
            )
        };
        let _ = buffer.Unlock();
        luma.ok_or_else(|| anyhow!("decoded picture is {len} bytes at stride {stride}"))
    }
}
