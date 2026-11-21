//! VideoToolbox hardware encode on macOS: H.264 and HEVC on the media engine.
//!
//! A `FramePayload::PixelBuffer` goes to the session as it is; VideoToolbox retains it until
//! the frame is encoded, and reads the layout and range off the buffer itself. A CPU frame
//! (packed RGB or NV12) is copied into a pixel buffer first. VideoToolbox writes NAL units
//! behind lengths and keeps the parameter sets aside; `vt_au` turns that into the Annex-B the
//! wire carries, parameter sets in-band on each keyframe.
//!
//! The session asks for speed: no frame reordering, no periodic keyframe (one comes only on
//! request), and `RealTime` off, which on the media engine encodes each frame as fast as it can
//! instead of pacing it, and drops none. Low-latency rate control, and the long-term references
//! that come only with it, encode two to three times slower and drop frames under load, so loss
//! recovery is a keyframe.
//!
//! Output arrives on VideoToolbox's own thread in submit order. The callback only queues the
//! sample buffer; `poll` assembles it on the encode thread.

use crate::cpu_ingest::{CpuIngest, Target};
use crate::stall::poll_budget_ms;
use crate::vt_au::annex_b;
use crate::{ChromaFormat, Codec, CodecSupport, EncodedFrame, Encoder, EncoderCaps, OpenParams};
use anyhow::{anyhow, bail, ensure, Context, Result};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, Type,
};
use objc2_core_media::{
    kCMSampleAttachmentKey_NotSync, kCMTimeInvalid, kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
    CMFormatDescription, CMSampleBuffer, CMTime,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
};
use objc2_core_video::{
    kCVImageBufferColorPrimaries_ITU_R_2020, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
    kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelFormatType_32BGRA,
    kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
    kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVPixelBuffer, CVPixelBufferCreate,
    CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_ColorPrimaries, kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_YCbCrMatrix,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_HEVC_Main10_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTVideoEncoderList_CodecType, kVTVideoEncoderList_IsHardwareAccelerated,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTCopyVideoEncoderList, VTEncodeInfoFlags, VTSessionSetProperty,
};
use pf_frame::{CapturedFrame, FramePayload, HdrMeta, PixelFormat};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The 4:4:4 HEVC profiles have no VideoToolbox constant; these are the values its encoder
/// list reports.
const HEVC_MAIN_444: &str = "HEVC_Main444_AutoLevel";
const HEVC_MAIN_444_10: &str = "HEVC_Main44410_AutoLevel";

/// The profile a session opens at: H.264 High, HEVC Main, Main 10 or the 4:4:4 pair.
fn profile(codec: Codec, ten_bit: bool, chroma: ChromaFormat) -> Result<CFRetained<CFString>> {
    // SAFETY: the profile constants are immutable CFStrings VideoToolbox exports.
    let (high, main, main10) = unsafe {
        (
            kVTProfileLevel_H264_High_AutoLevel,
            kVTProfileLevel_HEVC_Main_AutoLevel,
            kVTProfileLevel_HEVC_Main10_AutoLevel,
        )
    };
    Ok(match (codec, ten_bit, chroma) {
        (Codec::H264, _, ChromaFormat::Yuv420) => high.retain(),
        (Codec::H265, false, ChromaFormat::Yuv420) => main.retain(),
        (Codec::H265, true, ChromaFormat::Yuv420) => main10.retain(),
        (Codec::H265, false, ChromaFormat::Yuv444) => CFString::from_static_str(HEVC_MAIN_444),
        (Codec::H265, true, ChromaFormat::Yuv444) => CFString::from_static_str(HEVC_MAIN_444_10),
        _ => bail!("VideoToolbox encodes H.264 4:2:0 and HEVC only, not {codec:?} {chroma:?}"),
    })
}

fn codec_type(codec: Codec) -> Result<u32> {
    match codec {
        Codec::H264 => Ok(kCMVideoCodecType_H264),
        Codec::H265 => Ok(kCMVideoCodecType_HEVC),
        _ => bail!("VideoToolbox encodes H.264 and HEVC only, not {codec:?}"),
    }
}

/// What a session is opened for. A reset reopens from it.
#[derive(Clone, Copy, Debug)]
struct Config {
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u64,
    ten_bit: bool,
    hdr: bool,
    chroma: ChromaFormat,
    /// The range a CPU NV12 frame is copied at; a pixel buffer carries its own.
    full_range: bool,
}

impl Config {
    fn from_params(p: &OpenParams) -> Config {
        Config {
            codec: p.codec,
            width: p.width,
            height: p.height,
            fps: p.fps,
            bitrate_bps: p.bitrate_bps,
            ten_bit: p.bit_depth >= 10 && p.codec == Codec::H265,
            hdr: p.hdr,
            chroma: p.chroma,
            full_range: p.full_range,
        }
    }
}

/// One encoded frame, or its failure, as the output callback queued it.
struct Output {
    seq: u64,
    status: i32,
    dropped: bool,
    sample: Option<Sample>,
}

/// A retained `CMSampleBuffer` crossing from VideoToolbox's thread to the encode thread.
struct Sample(CFRetained<CMSampleBuffer>);

// SAFETY: a `CMSampleBuffer` retains and releases atomically, and the encode thread reads it only
// after VideoToolbox has handed it over complete; nothing writes it after the callback.
unsafe impl Send for Sample {}

/// The queue the output callback fills and `poll` drains.
#[derive(Default)]
struct Shared {
    q: Mutex<VecDeque<Output>>,
    ready: Condvar,
}

impl Shared {
    fn push(&self, out: Output) {
        self.q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(out);
        self.ready.notify_one();
    }

    /// The oldest output, waiting up to `budget` for one.
    fn take(&self, budget: Duration) -> Option<Output> {
        let q = self.q.lock().unwrap_or_else(|e| e.into_inner());
        let (mut q, _) = self
            .ready
            .wait_timeout_while(q, budget, |q| q.is_empty())
            .unwrap_or_else(|e| e.into_inner());
        q.pop_front()
    }
}

/// The compression callback. `refcon` is the session's [`Shared`]; `frame` is the sequence
/// number `submit` passed. Never panics: it runs on VideoToolbox's thread inside C.
unsafe extern "C-unwind" fn on_output(
    refcon: *mut c_void,
    frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: `refcon` is the `Arc<Shared>` pointer `Session::open` leaked; `Session::drop`
    // reclaims it only after `invalidate` returns, past the last callback.
    let shared = unsafe { &*(refcon as *const Shared) };
    // SAFETY: a non-null `sample` is a live sample buffer for the duration of the callback; the
    // retain keeps it past it.
    let sample = NonNull::new(sample).map(|s| Sample(unsafe { CFRetained::retain(s) }));
    shared.push(Output {
        seq: frame as usize as u64,
        status,
        dropped: flags.contains(VTEncodeInfoFlags::FrameDropped),
        sample,
    });
}

fn set(session: &VTCompressionSession, key: &CFString, value: &CFType) -> i32 {
    // SAFETY: a live session, a property key VideoToolbox exports and a CF value of the type that
    // key documents.
    unsafe { VTSessionSetProperty(session, key, Some(value)) }
}

fn number(v: i64) -> CFRetained<CFNumber> {
    CFNumber::new_i64(v)
}

/// A live compression session and the queue its callback fills.
struct Session {
    vt: CFRetained<VTCompressionSession>,
    shared: Arc<Shared>,
    /// The `Arc<Shared>` the callback holds as its refcon, reclaimed on drop.
    refcon: *const Shared,
}

impl Session {
    /// Create a hardware session for `cfg` and configure it; the profile, colour and rate must
    /// all take, or the open fails.
    fn open(cfg: &Config) -> Result<Session> {
        let codec = codec_type(cfg.codec)?;
        let profile = profile(cfg.codec, cfg.ten_bit, cfg.chroma)?;
        // SAFETY: an immutable key VideoToolbox exports.
        let spec = CFDictionary::<CFString, CFBoolean>::from_slices(
            &[unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder }],
            &[CFBoolean::new(true)],
        );
        let shared = Arc::new(Shared::default());
        let refcon = Arc::into_raw(shared.clone());
        let mut out: *mut VTCompressionSession = ptr::null_mut();
        // SAFETY: plain create with a specification dictionary of CFBoolean values, no source
        // attributes, and a callback whose refcon outlives the session (reclaimed in `Drop`, or
        // below when the create fails and no callback can come).
        let status = unsafe {
            VTCompressionSession::create(
                None,
                cfg.width as i32,
                cfg.height as i32,
                codec,
                Some(spec.as_opaque()),
                None,
                None,
                Some(on_output),
                refcon as *mut c_void,
                NonNull::from(&mut out),
            )
        };
        let Some(vt) = NonNull::new(out).filter(|_| status == 0) else {
            // SAFETY: the create failed, so VideoToolbox holds no copy of the refcon.
            drop(unsafe { Arc::from_raw(refcon) });
            bail!("VTCompressionSessionCreate: OSStatus {status}");
        };
        let session = Session {
            // SAFETY: a successful create hands over one retain.
            vt: unsafe { CFRetained::from_raw(vt) },
            shared,
            refcon,
        };
        session.configure(cfg, &profile)?;
        Ok(session)
    }

    fn configure(&self, cfg: &Config, profile: &CFString) -> Result<()> {
        // SAFETY: immutable CFString constants VideoToolbox and CoreVideo export.
        let props: Vec<(&CFString, CFRetained<CFType>)> = unsafe {
            let colour = if cfg.hdr {
                [
                    kCVImageBufferColorPrimaries_ITU_R_2020,
                    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
                    kCVImageBufferYCbCrMatrix_ITU_R_2020,
                ]
            } else {
                [
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                ]
            };
            let yes: CFRetained<CFType> = CFBoolean::new(true).into();
            let no: CFRetained<CFType> = CFBoolean::new(false).into();
            vec![
                (
                    kVTCompressionPropertyKey_ProfileLevel,
                    profile.retain().into(),
                ),
                (kVTCompressionPropertyKey_RealTime, no.clone()),
                (
                    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
                    yes,
                ),
                (kVTCompressionPropertyKey_AllowFrameReordering, no),
                // The longest interval VideoToolbox takes: zero would let it pick its own.
                (
                    kVTCompressionPropertyKey_MaxKeyFrameInterval,
                    number(i64::from(i32::MAX)).into(),
                ),
                (
                    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
                    number(0).into(),
                ),
                (
                    kVTCompressionPropertyKey_ExpectedFrameRate,
                    number(i64::from(cfg.fps)).into(),
                ),
                (
                    kVTCompressionPropertyKey_ColorPrimaries,
                    colour[0].retain().into(),
                ),
                (
                    kVTCompressionPropertyKey_TransferFunction,
                    colour[1].retain().into(),
                ),
                (
                    kVTCompressionPropertyKey_YCbCrMatrix,
                    colour[2].retain().into(),
                ),
            ]
        };
        for (key, value) in &props {
            let status = set(&self.vt, key, value);
            ensure!(status == 0, "set {key}: OSStatus {status}");
        }
        ensure!(
            self.set_bitrate(cfg.bitrate_bps, cfg.fps),
            "set the bitrate"
        );
        // SAFETY: a configured live session.
        let status = unsafe { self.vt.prepare_to_encode_frames() };
        ensure!(
            status == 0,
            "VTCompressionSessionPrepareToEncodeFrames: OSStatus {status}"
        );
        Ok(())
    }

    /// The average at `bps`, and a peak of the same rate over the VBV window
    /// ([`crate::vbv_frames_env`] frame intervals), so each frame fits its slot as the pacer
    /// expects.
    fn set_bitrate(&self, bps: u64, fps: u32) -> bool {
        let window = crate::vbv_frames_env() / f64::from(fps.max(1));
        let bytes = (bps as f64 / 8.0 * window).round().max(1.0) as i64;
        let limits = CFArray::<CFType>::from_retained_objects(&[
            number(bytes).into(),
            CFNumber::new_f64(window).into(),
        ]);
        // SAFETY: immutable property keys VideoToolbox exports.
        let (avg, limit) = unsafe {
            (
                kVTCompressionPropertyKey_AverageBitRate,
                kVTCompressionPropertyKey_DataRateLimits,
            )
        };
        let a = set(&self.vt, avg, &number(bps.min(i64::MAX as u64) as i64));
        let l = set(&self.vt, limit, &limits);
        if a != 0 || l != 0 {
            tracing::warn!(avg = a, limits = l, bps, "VideoToolbox refused the bitrate");
        }
        a == 0 && l == 0
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: a live session. Completing first emits every pending frame, so the callback has
        // run its last before `invalidate` returns and the refcon can be reclaimed.
        unsafe {
            let _ = self.vt.complete_frames(kCMTimeInvalid);
            self.vt.invalidate();
            drop(Arc::from_raw(self.refcon));
        }
    }
}

/// A submitted frame, oldest first, until its output is polled.
#[derive(Clone, Copy, Debug)]
struct Pending {
    seq: u64,
    pts_ns: u64,
}

/// A CPU frame's pixel buffer, kept while its layout stays the same.
struct Staging {
    format: u32,
    buffer: CFRetained<CVPixelBuffer>,
}

pub(crate) struct VtEncoder {
    cfg: Config,
    session: Session,
    pending: VecDeque<Pending>,
    seq: u64,
    /// The last presentation time handed to VideoToolbox, which refuses one that does not grow.
    last_pts: Option<i64>,
    force_kf: bool,
    hdr_meta: Option<HdrMeta>,
    cpu: CpuIngest,
    staging: Option<Staging>,
    /// The sample bytes of the AU being assembled, kept across frames.
    scratch: Vec<u8>,
}

// SAFETY: the encoder is driven from one thread at a time. The session's callback reaches only
// `Shared`, which is behind its own lock; every other field is plain data or a retained CF object
// this encoder alone uses.
unsafe impl Send for VtEncoder {}

impl VtEncoder {
    pub(crate) fn open(p: &OpenParams) -> Result<VtEncoder> {
        let cfg = Config::from_params(p);
        let session = Session::open(&cfg).context("open a VideoToolbox session")?;
        tracing::info!(
            codec = ?cfg.codec,
            width = cfg.width,
            height = cfg.height,
            fps = cfg.fps,
            ten_bit = cfg.ten_bit,
            chroma_444 = cfg.chroma.is_444(),
            "VideoToolbox encoder open"
        );
        Ok(VtEncoder {
            cfg,
            session,
            pending: VecDeque::new(),
            seq: 0,
            last_pts: None,
            force_kf: true,
            hdr_meta: None,
            cpu: CpuIngest::default(),
            staging: None,
            scratch: Vec::new(),
        })
    }

    /// `frame`'s CPU pixels in a pixel buffer VideoToolbox takes: packed RGB as BGRA, NV12 at the
    /// session's range.
    fn stage_cpu(&mut self, frame: &CapturedFrame) -> Result<CFRetained<CVPixelBuffer>> {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let nv12 = frame.format == PixelFormat::Nv12;
        let format = match (nv12, self.cfg.full_range) {
            (true, true) => kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            (true, false) => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            (false, _) => kCVPixelFormatType_32BGRA,
        };
        if self.staging.as_ref().is_none_or(|s| s.format != format) {
            self.staging = Some(Staging {
                format,
                buffer: new_pixel_buffer(w, h, format)?,
            });
        }
        let buffer = self.staging.as_ref().expect("staged above").buffer.clone();
        if nv12 {
            let FramePayload::Cpu(bytes) = &frame.payload else {
                bail!("stage a CPU frame: the payload holds no CPU pixels");
            };
            let need = w * h * 3 / 2;
            ensure!(
                bytes.len() >= need,
                "CPU NV12 {w}x{h} needs {need} bytes, got {}",
                bytes.len()
            );
            copy_into(
                &buffer,
                &[(&bytes[..w * h], w, h), (&bytes[w * h..need], w, h / 2)],
            )?;
        } else {
            let s = self.cpu.stage(frame, Target::BFirst)?;
            ensure!(
                matches!(s.fmt, PixelFormat::Bgra | PixelFormat::Bgrx),
                "VideoToolbox takes packed 8-bit RGB or NV12 CPU frames, not {:?}",
                frame.format
            );
            copy_into(&buffer, &[(s.bytes, s.row, h)])?;
        }
        Ok(buffer)
    }

    /// The oldest output turned into the AU the wire carries.
    fn assemble(&mut self, meta: Pending, out: Output) -> Result<EncodedFrame> {
        ensure!(
            out.seq == meta.seq,
            "VideoToolbox returned frame {} where {} was owed",
            out.seq,
            meta.seq
        );
        ensure!(
            out.status == 0,
            "VideoToolbox encode: OSStatus {}",
            out.status
        );
        let Some(Sample(sample)) = out.sample.filter(|_| !out.dropped) else {
            bail!("VideoToolbox dropped frame {}", meta.seq);
        };
        // SAFETY: a complete sample buffer VideoToolbox handed over; each getter returns a
        // retained object or `None`, and the copy writes exactly the length the buffer reports.
        let (keyframe, format) = unsafe {
            let data = sample
                .data_buffer()
                .ok_or_else(|| anyhow!("sample has no data"))?;
            let len = data.data_length();
            self.scratch.resize(len, 0);
            let dst = NonNull::new(self.scratch.as_mut_ptr().cast()).expect("vec pointer");
            let status = data.copy_data_bytes(0, len, dst);
            ensure!(status == 0, "CMBlockBufferCopyDataBytes: OSStatus {status}");
            let format = sample
                .format_description()
                .ok_or_else(|| anyhow!("sample has no format description"))?;
            (is_sync(&sample), format)
        };
        let mut head: Vec<Vec<u8>> = Vec::new();
        // SAFETY: `format` stays retained while its units are copied out.
        let len_size = unsafe {
            parameter_sets(self.cfg.codec, &format, &mut |nal| {
                if keyframe {
                    head.push(nal.to_vec());
                }
            })?
        };
        if let (true, Codec::H265, true, Some(meta)) =
            (keyframe, self.cfg.codec, self.cfg.hdr, self.hdr_meta)
        {
            // The helper writes an Annex-B unit; `annex_b` adds the start code itself.
            head.push(pf_frame::hdr::hevc_hdr_sei_nal(&meta)[4..].to_vec());
        }
        let head: Vec<&[u8]> = head.iter().map(Vec::as_slice).collect();
        Ok(EncodedFrame {
            data: annex_b(&head, &self.scratch, len_size)?,
            pts_ns: meta.pts_ns,
            keyframe,
            recovery_anchor: false,
            recovery_point: false,
            recovery_close: false,
            chunk_aligned: false,
        })
    }
}

/// Whether the sample is a sync sample: no attachment, or one without `NotSync`.
///
/// # Safety
/// `sample` is a complete sample buffer.
unsafe fn is_sync(sample: &CMSampleBuffer) -> bool {
    // SAFETY: the caller's sample; the attachments array holds one dictionary per sample.
    let Some(attachments) = (unsafe { sample.sample_attachments_array(false) }) else {
        return true;
    };
    // SAFETY: CMSampleBuffer documents each element as a CFDictionary of CFString keys.
    let attachments: CFRetained<CFArray<CFDictionary<CFString, CFType>>> =
        unsafe { CFRetained::cast_unchecked(attachments) };
    let Some(first) = attachments.get(0) else {
        return true;
    };
    // SAFETY: an immutable key CoreMedia exports.
    let not_sync = first.get(unsafe { kCMSampleAttachmentKey_NotSync });
    !not_sync
        .and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
        .unwrap_or(false)
}

/// Hand each parameter set of `format` to `each`, and return the NAL length field size.
///
/// # Safety
/// `format` is a live H.264 or HEVC video format description; `each` must not keep the slice.
unsafe fn parameter_sets(
    codec: Codec,
    format: &CMFormatDescription,
    each: &mut dyn FnMut(&[u8]),
) -> Result<usize> {
    let get = match codec {
        Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        _ => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    };
    let (mut count, mut len_size) = (0usize, 0i32);
    // SAFETY: null outputs are allowed; the count and length size are written.
    let status = unsafe {
        get(
            format,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut count,
            &mut len_size,
        )
    };
    ensure!(
        status == 0,
        "read the parameter set count: OSStatus {status}"
    );
    for i in 0..count {
        let (mut p, mut len) = (ptr::null::<u8>(), 0usize);
        // SAFETY: `i < count`; the unit points into `format`, which the caller keeps alive.
        let status = unsafe {
            get(
                format,
                i,
                &mut p,
                &mut len,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        ensure!(
            status == 0 && !p.is_null(),
            "read parameter set {i}: OSStatus {status}"
        );
        // SAFETY: VideoToolbox reported `len` bytes at `p`, valid while `format` lives.
        each(unsafe { std::slice::from_raw_parts(p, len) });
    }
    Ok(len_size as usize)
}

/// An IOSurface-backed pixel buffer, so VideoToolbox reads it without a copy of its own.
fn new_pixel_buffer(w: usize, h: usize, format: u32) -> Result<CFRetained<CVPixelBuffer>> {
    // An empty dictionary asks for default IOSurface properties.
    let surface = CFDictionary::<CFString, CFType>::empty();
    let attrs = CFDictionary::<CFString, CFDictionary>::from_slices(
        // SAFETY: an immutable key CoreVideo exports.
        &[unsafe { kCVPixelBufferIOSurfacePropertiesKey }],
        &[surface.as_opaque()],
    );
    let mut out: *mut CVPixelBuffer = ptr::null_mut();
    // SAFETY: plain create with an attributes dictionary of the documented types.
    let status = unsafe {
        CVPixelBufferCreate(
            None,
            w,
            h,
            format,
            Some(attrs.as_opaque()),
            NonNull::from(&mut out),
        )
    };
    let buffer = NonNull::new(out).filter(|_| status == 0);
    // SAFETY: a successful create hands over one retain.
    buffer
        .map(|b| unsafe { CFRetained::from_raw(b) })
        .ok_or_else(|| anyhow!("CVPixelBufferCreate {w}x{h}: CVReturn {status}"))
}

/// Copy each `(bytes, row pitch, rows)` plane into the buffer's plane of the same index.
fn copy_into(buffer: &CVPixelBuffer, planes: &[(&[u8], usize, usize)]) -> Result<()> {
    // SAFETY: the buffer is locked across the copies, and each row write stays within both the
    // source plane and the destination's bytes-per-row times its height.
    unsafe {
        let status = CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags(0));
        ensure!(
            status == 0,
            "CVPixelBufferLockBaseAddress: CVReturn {status}"
        );
        let mut result = Ok(());
        let (bw, bh) = (
            CVPixelBufferGetWidth(buffer),
            CVPixelBufferGetHeight(buffer),
        );
        for (i, &(src, pitch, rows)) in planes.iter().enumerate() {
            let dst = CVPixelBufferGetBaseAddressOfPlane(buffer, i).cast::<u8>();
            let dst_pitch = CVPixelBufferGetBytesPerRowOfPlane(buffer, i);
            if dst.is_null() || src.len() < pitch * rows || rows > bh {
                result = Err(anyhow!("plane {i} does not fit the {bw}x{bh} pixel buffer"));
                break;
            }
            for y in 0..rows {
                ptr::copy_nonoverlapping(
                    src.as_ptr().add(y * pitch),
                    dst.add(y * dst_pitch),
                    pitch.min(dst_pitch),
                );
            }
        }
        CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags(0));
        result
    }
}

impl Encoder for VtEncoder {
    fn submit(&mut self, frame: &CapturedFrame) -> Result<()> {
        ensure!(
            frame.width == self.cfg.width && frame.height == self.cfg.height,
            "captured frame {}x{} != encoder {}x{}",
            frame.width,
            frame.height,
            self.cfg.width,
            self.cfg.height
        );
        let staged;
        let buffer: &CVPixelBuffer = match &frame.payload {
            FramePayload::PixelBuffer(b) => &b.0,
            FramePayload::Cpu(_) => {
                staged = self.stage_cpu(frame)?;
                &staged
            }
            FramePayload::Pixelless => bail!("VideoToolbox got a pixel-less frame"),
        };
        // A repeated frame keeps its own time on the AU; VideoToolbox gets one that grows.
        let pts =
            (frame.pts_ns.min(i64::MAX as u64) as i64).max(self.last_pts.map_or(0, |t| t + 1));
        self.last_pts = Some(pts);
        let props = std::mem::take(&mut self.force_kf).then(|| {
            CFDictionary::<CFString, CFBoolean>::from_slices(
                // SAFETY: an immutable key VideoToolbox exports.
                &[unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame }],
                &[CFBoolean::new(true)],
            )
        });
        let seq = self.seq;
        // SAFETY: a live session and pixel buffer; VideoToolbox retains the buffer until it is
        // encoded. The refcon is a plain number, never dereferenced.
        let status = unsafe {
            self.session.vt.encode_frame(
                buffer,
                CMTime::new(pts, 1_000_000_000),
                CMTime::new(1, self.cfg.fps as i32),
                props.as_deref().map(|d| d.as_opaque()),
                seq as usize as *mut c_void,
                ptr::null_mut(),
            )
        };
        if status != 0 {
            self.force_kf |= props.is_some();
            bail!("VTCompressionSessionEncodeFrame: OSStatus {status}");
        }
        self.seq += 1;
        self.pending.push_back(Pending {
            seq,
            pts_ns: frame.pts_ns,
        });
        Ok(())
    }

    fn caps(&self) -> EncoderCaps {
        EncoderCaps {
            chroma_444: self.cfg.chroma.is_444(),
            ..EncoderCaps::default()
        }
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    /// Written in-band ahead of each HEVC keyframe's slices while the session is HDR.
    fn set_hdr_meta(&mut self, meta: Option<HdrMeta>) {
        self.hdr_meta = meta;
    }

    /// Wait up to [`poll_budget_ms`] for the oldest frame's output. Expiry is `Ok(None)`.
    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        let Some(&meta) = self.pending.front() else {
            return Ok(None);
        };
        let budget = Duration::from_millis(u64::from(poll_budget_ms(self.cfg.fps)));
        let Some(out) = self.session.shared.take(budget) else {
            return Ok(None);
        };
        self.pending.pop_front();
        self.assemble(meta, out)
            .inspect_err(|_| self.force_kf = true)
            .map(Some)
    }

    /// Reopen the session in place: in-flight frames are forfeit and the next one is a keyframe.
    fn reset(&mut self) -> bool {
        match Session::open(&self.cfg) {
            Ok(session) => {
                self.session = session;
                self.pending.clear();
                self.force_kf = true;
                true
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "VideoToolbox session did not reopen");
                false
            }
        }
    }

    fn reconfigure_bitrate(&mut self, bps: u64) -> bool {
        if !self.session.set_bitrate(bps, self.cfg.fps) {
            self.session.set_bitrate(self.cfg.bitrate_bps, self.cfg.fps);
            return false;
        }
        self.cfg.bitrate_bps = bps;
        true
    }

    fn applied_bitrate_bps(&self) -> Option<u64> {
        Some(self.cfg.bitrate_bps)
    }

    fn flush(&mut self) -> Result<()> {
        // SAFETY: a live session; completes every pending frame before it returns.
        let status = unsafe { self.session.vt.complete_frames(kCMTimeInvalid) };
        ensure!(
            status == 0,
            "VTCompressionSessionCompleteFrames: OSStatus {status}"
        );
        Ok(())
    }
}

/// The codecs VideoToolbox encodes in hardware on this Mac, from its encoder list. AV1 is not
/// wired: no Mac this was built on lists a hardware AV1 encoder.
pub(crate) fn hardware_codecs() -> CodecSupport {
    let mut found = CodecSupport {
        h264: false,
        h265: false,
        av1: false,
    };
    let mut list: *const CFArray = ptr::null();
    // SAFETY: plain FFI; a successful call hands over one retain of an array of dictionaries.
    let list = unsafe {
        let status = VTCopyVideoEncoderList(None, NonNull::from(&mut list));
        match NonNull::new(list as *mut CFArray<CFDictionary<CFString, CFType>>) {
            Some(l) if status == 0 => CFRetained::from_raw(l),
            _ => return found,
        }
    };
    // SAFETY: immutable keys VideoToolbox exports.
    let (codec_key, hw_key) = unsafe {
        (
            kVTVideoEncoderList_CodecType,
            kVTVideoEncoderList_IsHardwareAccelerated,
        )
    };
    for encoder in list.iter() {
        let hw = encoder
            .get(hw_key)
            .and_then(|v| v.downcast_ref::<CFBoolean>().map(CFBoolean::as_bool))
            .unwrap_or(false);
        let codec = encoder
            .get(codec_key)
            .and_then(|v| v.downcast_ref::<CFNumber>().and_then(CFNumber::as_i64));
        match codec {
            Some(c) if hw && c == i64::from(kCMVideoCodecType_H264) => found.h264 = true,
            Some(c) if hw && c == i64::from(kCMVideoCodecType_HEVC) => found.h265 = true,
            _ => {}
        }
    }
    found
}

/// Whether VideoToolbox opens a hardware `codec` session at this depth and chroma, as an open
/// would, at a small size.
pub(crate) fn opens(codec: Codec, ten_bit: bool, chroma: ChromaFormat) -> bool {
    let cfg = Config {
        codec,
        width: 1280,
        height: 720,
        fps: 60,
        bitrate_bps: 10_000_000,
        ten_bit,
        hdr: ten_bit,
        chroma,
        full_range: false,
    };
    match Session::open(&cfg) {
        Ok(_) => true,
        Err(e) => {
            tracing::debug!(?codec, ten_bit, ?chroma, error = %format!("{e:#}"), "VideoToolbox refused a profile");
            false
        }
    }
}

#[cfg(test)]
#[path = "videotoolbox_tests.rs"]
mod tests;
