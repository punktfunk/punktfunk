//! Last decoder-ladder rung: CPU, no FFmpeg. H.264 is openh264, AV1 is rav1d;
//! HEVC has no permissively licensed software decoder and is [`NoSoftwareRung`].
//! Both codecs accept 8-bit 4:2:0 only, read from in-band headers so a mid-stream
//! shape change is the same typed refusal. [`crate::video::last_rung_verdict`]
//! turns that into a reconnect that advertises codecs this build can decode.
//!
//! Output is owned packed I420 [`CpuPlanarFrame`]; the presenter does CSC.
//! H.264 colour, crop, IDR, and recovery-point SEI come from [`H264Planner`]
//! and [`RecoveryWatch`], not from openh264. Decoder contexts are `Send` not
//! `Sync` and used serially. [`Av1Data`] owns each copied AU until rav1d
//! takes or drops it. Ordinary decode errors ask for a keyframe;
//! [`NoSoftwareRung`] ends the rung.
//!
//! [`Av1Software::new`] refuses `n_fc < 2`: rav1d aborts the process on a
//! decode error with one frame context. Evidence: tests in this file.

use crate::video::{CpuPlanarFrame, RungLoss};
use crate::video_color::ColorDesc;
use anyhow::{anyhow, bail, Context as _, Result};
use pf_bitstream::h264::{H264Planner, PlanError};
use pf_vkdecode::RecoveryWatch;

/// Own enum, not `ffmpeg::codec::Id` — this rung must not speak FFmpeg.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwCodec {
    H264,
    Av1,
}

impl SwCodec {
    pub(crate) fn for_wire(codec: u8) -> Option<SwCodec> {
        match codec {
            punktfunk_core::quic::CODEC_H264 => Some(SwCodec::H264),
            punktfunk_core::quic::CODEC_AV1 => Some(SwCodec::Av1),
            _ => None,
        }
    }
}

/// Last-rung refusal: this build cannot decode the session's stream.
///
/// Distinct from every other decode `Err` so the session can `downcast_ref` it.
/// Survivable failures ask for an IDR; this one only reconnects with a codec
/// the client can decode. Travels through `anyhow` so ladder signatures stay
/// unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoSoftwareRung {
    /// The `quic::CODEC_*` bit with no CPU decoder.
    pub codec: u8,
    /// `None`: the codec has no CPU decoder (HEVC). `Some`: 8-bit 4:2:0 only,
    /// and this stream is not (10-bit, 4:4:4). One type because Welcome pins
    /// the codec, so a bad shape has the same escape as a missing codec:
    /// reconnect and drop it. Raised per AU from in-band headers — Welcome's
    /// [`crate::video::StreamFormat`] can say 8-bit for a stream that later is not.
    pub shape: Option<&'static str>,
}

impl NoSoftwareRung {
    /// [`last_rung_verdict`](crate::video::last_rung_verdict) input: `Codec` means
    /// hardware already failed; `Shape` means hardware was never asked.
    pub fn loss(&self) -> RungLoss {
        match self.shape {
            None => RungLoss::Codec,
            Some(_) => RungLoss::Shape,
        }
    }
}

impl std::fmt::Display for NoSoftwareRung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let codec = crate::video::wire_codec_name(self.codec);
        match self.shape {
            None => write!(
                f,
                "no software decoder for {codec} — this build decodes H.264 and AV1 on \
                 the CPU (there is no permissively licensed software HEVC decoder)"
            ),
            Some(shape) => write!(
                f,
                "the software {codec} decoder cannot decode this stream: {shape} \
                 (the CPU rung is 8-bit 4:2:0 only)"
            ),
        }
    }
}

impl std::error::Error for NoSoftwareRung {}

pub(crate) struct SoftwareDecoder {
    inner: Inner,
    /// Last colour the stream stated. Held across AUs the planner could not
    /// read so the picture does not snap back to the SDR default; seed is
    /// that default (`csc_rows` "unspecified").
    color: ColorDesc,
}

enum Inner {
    H264(H264Software),
    Av1(Av1Software),
}

impl SoftwareDecoder {
    /// `Err` with [`NoSoftwareRung`] means this build has no rung for `codec`,
    /// not that the rung failed to start.
    pub(crate) fn new(codec: u8) -> Result<SoftwareDecoder> {
        let Some(sw) = SwCodec::for_wire(codec) else {
            return Err(NoSoftwareRung { codec, shape: None }.into());
        };
        let inner = match sw {
            SwCodec::H264 => Inner::H264(H264Software::new()?),
            SwCodec::Av1 => Inner::Av1(Av1Software::new()?),
        };
        tracing::info!(
            codec = crate::video::wire_codec_name(codec),
            decoder = match sw {
                SwCodec::H264 => "openh264",
                SwCodec::Av1 => "rav1d",
            },
            "software decoder opened (CPU, planar output)"
        );
        Ok(SoftwareDecoder {
            inner,
            // Unspecified (2) + limited: `csc_rows` maps this to BT.709 limited,
            // the SDR default and E.2.1's answer for a silent VUI.
            color: ColorDesc {
                primaries: 2,
                transfer: 2,
                matrix: 2,
                full_range: false,
            },
        })
    }

    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<CpuPlanarFrame>> {
        match &mut self.inner {
            Inner::H264(h) => h.decode(au, &mut self.color),
            Inner::Av1(a) => a.decode(au, &mut self.color),
        }
    }
}

struct H264Software {
    decoder: openh264::decoder::Decoder,
    /// Colour, IDR, crop from the shared planner. Does not drive openh264 —
    /// a refused plan drops metadata for that AU, never the picture.
    /// Boxed: the planner DPB dwarfs the other `Backend` variants (pointer-sized).
    planner: Box<H264Planner>,
    /// Recovery-point SEI, same fold as the Vulkan rung. Intra-refresh never
    /// emits an IDR; without this the pump's post-loss freeze waits 500 ms
    /// and then forces the IDR the wave exists to avoid.
    recovery: RecoveryWatch,
    /// One warn per session when AUs will not plan: the picture still
    /// decodes; colour then stays at the last plannable AU.
    plan_warned: bool,
}

struct AuFacts {
    is_idr: bool,
    /// `None` = AU did not plan; caller keeps the last colour.
    color: Option<ColorDesc>,
    recovery: punktfunk_core::reanchor::LocalRecovery,
}

impl H264Software {
    fn new() -> Result<H264Software> {
        // Concealment off: an `Err` must surface so the pump can request a
        // keyframe. Invented macroblocks look clean and are not.
        let decoder =
            openh264::decoder::Decoder::new().map_err(|e| anyhow!("openh264 decoder: {e}"))?;
        Ok(H264Software {
            decoder,
            planner: Box::new(H264Planner::new()),
            recovery: RecoveryWatch::new(),
            plan_warned: false,
        })
    }

    fn decode(&mut self, au: &[u8], color: &mut ColorDesc) -> Result<Option<CpuPlanarFrame>> {
        // Plan before decode: a buffering decoder would otherwise pin this AU's
        // colour on the next picture.
        let facts = self.plan_facts(au)?;
        if let Some(c) = facts.color {
            *color = c;
        }
        let picture = self
            .decoder
            .decode(au)
            .map_err(|e| anyhow!("openh264 decode: {e}"))?;
        let Some(yuv) = picture else {
            return Ok(None);
        };
        use openh264::formats::YUVSource as _;
        let (w, h) = yuv.dimensions();
        let (sy, su, sv) = yuv.strides();
        let frame = CpuPlanarFrame::from_i420(
            w as u32,
            h as u32,
            [yuv.y(), yuv.u(), yuv.v()],
            [sy, su, sv],
            *color,
            facts.is_idr,
            facts.recovery,
        )?;
        Ok(Some(frame))
    }

    /// IDR, colour, recovery for this AU from the shared planner.
    ///
    /// `color: None` is normal before the first in-band IDR (no active SPS).
    /// `is_idr = false` and empty recovery then: a false `true` would lift a
    /// post-loss freeze onto a still-concealed picture.
    /// `Err` is only a shape this rung cannot decode ([`NoSoftwareRung`]), so
    /// the session reconnects instead of erroring every AU.
    fn plan_facts(&mut self, au: &[u8]) -> Result<AuFacts> {
        match self.planner.plan_au(au) {
            Ok(plan) => {
                // Envelope from the SPS this picture activated, so an in-band
                // Main 10 flip is caught here instead of failing openh264 every AU after.
                if let Some(shape) = unsupported_shape(
                    plan.picture.chroma_format_idc,
                    plan.picture.bit_depth_luma_minus8,
                ) {
                    return Err(NoSoftwareRung {
                        codec: punktfunk_core::quic::CODEC_H264,
                        shape: Some(shape),
                    }
                    .into());
                }
                // Fold recovery even if openh264 emits nothing: the watch counts
                // `frame_num`. Skipping one leaves the count owing; a late lift is safe.
                let mark = self.recovery.note_h264(
                    plan.picture.frame_num,
                    plan.picture.is_idr,
                    plan.picture.recovery_point,
                );
                Ok(AuFacts {
                    is_idr: plan.picture.is_idr,
                    color: Some(ColorDesc::from(&plan.picture.colour)),
                    recovery: punktfunk_core::reanchor::LocalRecovery {
                        sei_here: mark.sei_here,
                        is_recovery_point: mark.is_recovery_point,
                    },
                })
            }
            Err(e) => {
                // `NoActiveParamSet` before the first IDR is expected. Anything else
                // is outside the hardware envelope — one warn for the session.
                if !matches!(e, PlanError::NoActiveParamSet { .. }) && !self.plan_warned {
                    self.plan_warned = true;
                    tracing::warn!(
                        error = %e,
                        "software rung: AU did not plan — colour signalling and the \
                         keyframe flag now follow the last AU that did"
                    );
                }
                Ok(AuFacts {
                    is_idr: false,
                    color: None,
                    recovery: punktfunk_core::reanchor::LocalRecovery::NONE,
                })
            }
        }
    }
}

/// 8-bit 4:2:0 only. `Some` names what is outside, for [`NoSoftwareRung::shape`].
/// Shared by both legs; matches the build (openh264 has no high depth / 4:4:4,
/// rav1d is `bitdepth_8` only), not a policy that could drift from it.
fn unsupported_shape(chroma_format_idc: u8, bit_depth_minus8: u8) -> Option<&'static str> {
    if bit_depth_minus8 != 0 {
        return Some("10-bit or deeper");
    }
    if chroma_format_idc != punktfunk_core::quic::CHROMA_IDC_420 {
        return Some("chroma other than 4:2:0");
    }
    None
}

// rav1d is dav1d's C ABI as `extern "C"` Rust (`#[repr(C)]`, no `.so`).
// Each context/picture has one owner here; `Drop` closes it once.
use rav1d::include::dav1d::data::Dav1dData;
use rav1d::include::dav1d::dav1d::{Dav1dContext, Dav1dSettings};
use rav1d::include::dav1d::headers::{Dav1dSequenceHeader, DAV1D_PIXEL_LAYOUT_I420};
use rav1d::include::dav1d::picture::Dav1dPicture;
use rav1d::src::lib::{
    dav1d_close, dav1d_data_create, dav1d_data_unref, dav1d_default_settings,
    dav1d_get_frame_delay, dav1d_get_picture, dav1d_open, dav1d_parse_sequence_header,
    dav1d_picture_unref, dav1d_send_data,
};
use std::ptr::NonNull;

/// Floor on rav1d `n_fc`. One is fatal ([`Av1Software::new`]).
///
/// `n_fc = min(max_frame_delay, n_threads)`, so this floors both. Two, not
/// more: each extra context is another 4K working set, and `decode` drains
/// the frame in the same call.
const AV1_MIN_FRAME_CONTEXTS: i32 = 2;

struct Av1Software {
    /// `None` only between `Drop` taking it and close returning.
    ctx: Option<Dav1dContext>,
}

/// Owned `Dav1dData`, unref'd once on drop.
///
/// The send loop has several fallible exits between allocate and dav1d taking
/// the reference; hand-unref on each is a leak per failed AU. dav1d zeroes
/// the struct when it takes the ref, so a consumed value's `Drop` is a no-op.
struct Av1Data(Dav1dData);

impl Av1Data {
    fn create(au: &[u8]) -> Result<Av1Data> {
        let mut data = Dav1dData::default();
        // SAFETY: `data` is a live local; `dav1d_data_create` either writes an allocated
        // buffer of `au.len()` bytes into it and returns its start, or returns null.
        let buf = unsafe { dav1d_data_create(NonNull::new(&mut data as *mut Dav1dData), au.len()) };
        if buf.is_null() {
            bail!("rav1d: {}-byte AU allocation failed", au.len());
        }
        // SAFETY: `buf` is the start of the `au.len()`-byte allocation just returned, and
        // `au` is a distinct live slice of exactly that length.
        unsafe { std::ptr::copy_nonoverlapping(au.as_ptr(), buf, au.len()) };
        Ok(Av1Data(data))
    }
}

impl Drop for Av1Data {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live `Dav1dData` this value solely owns (no `Clone`, and
        // `Drop` runs once). `dav1d_data_unref` releases whatever reference is left — none
        // when a successful `dav1d_send_data` already took it — and rewrites the struct.
        unsafe { dav1d_data_unref(NonNull::new(&mut self.0)) };
    }
}

// SAFETY: `Dav1dContext` is a refcounted handle dav1d documents as usable from one
// thread at a time; this type owns it exclusively (no `Clone`, no `Sync`) and it lives
// on the pump thread with the rest of the decoder — the same promise every other backend
// in this crate makes for its device handles.
unsafe impl Send for Av1Software {}

impl Av1Software {
    fn new() -> Result<Av1Software> {
        let mut settings = av1_settings();
        // rav1d's own `n_fc` (`dav1d_get_frame_delay`), not a copy of our arithmetic.
        // `n_fc < 2` is `abort()` on the first bad AU; a `bail!` here is recoverable.
        let delay = frame_delay(&mut settings);
        if delay < AV1_MIN_FRAME_CONTEXTS {
            bail!(
                "rav1d would run with {delay} frame context(s) (n_threads={}, \
                 max_frame_delay={}); anything below {AV1_MIN_FRAME_CONTEXTS} takes the \
                 single-frame-context path, which ABORTS the process on any decode error",
                settings.n_threads,
                settings.max_frame_delay,
            );
        }
        let mut ctx: Option<Dav1dContext> = None;
        // SAFETY: both pointers are live locals for the duration of the call, which is
        // dav1d_open's whole contract: it reads `settings` and writes the context out.
        let r = unsafe {
            dav1d_open(
                NonNull::new(&mut ctx as *mut Option<Dav1dContext>),
                NonNull::new(&mut settings as *mut Dav1dSettings),
            )
        };
        if r.0 < 0 || ctx.is_none() {
            bail!("rav1d (dav1d) decoder open failed: {}", r.0);
        }
        Ok(Av1Software { ctx })
    }
}

/// Settings this leg opens rav1d with. Separate so `n_fc >= AV1_MIN_FRAME_CONTEXTS`
/// is testable without constructing a decoder.
fn av1_settings() -> Dav1dSettings {
    let mut settings = std::mem::MaybeUninit::<Dav1dSettings>::uninit();
    // SAFETY: `dav1d_default_settings` fully initializes the `Dav1dSettings` behind
    // the pointer it is given; the storage is a live local that outlives the call.
    let mut settings = unsafe {
        dav1d_default_settings(NonNull::new_unchecked(settings.as_mut_ptr()));
        settings.assume_init()
    };
    // Two frame contexts, not one. rav1d `abort()`s the process on any decode
    // error when `n_fc == 1` (panic in `extern "C"` `dav1d_send_data`). `n_fc`
    // is `min(max_frame_delay, n_threads)`, so `n_threads` is floored too.
    // `decode` drains the in-flight frame in the same call; no extra latency.
    settings.max_frame_delay = AV1_MIN_FRAME_CONTEXTS;
    // Tile/row workers add no delay and also floor `n_fc`. Cap 8: this rung
    // exists because the GPU failed; it must not take the machine.
    settings.n_threads = std::thread::available_parallelism()
        .map(|n| n.get().clamp(AV1_MIN_FRAME_CONTEXTS as usize, 8))
        .unwrap_or(AV1_MIN_FRAME_CONTEXTS as usize) as i32;
    // Hosts never signal film grain; synthesis is too expensive on this rung.
    settings.apply_grain = 0;
    settings
}

impl Av1Software {
    fn decode(&mut self, au: &[u8], color: &mut ColorDesc) -> Result<Option<CpuPlanarFrame>> {
        let ctx = self.ctx.context("rav1d context closed")?;
        if au.is_empty() {
            return Ok(None);
        }
        // Envelope from the AU's sequence header, before submit. A 10-bit frame
        // is `ENOPROTOOPT` from a `bitdepth_8` build; the pump would treat that
        // as survivable and request a keyframe forever. Hardware AV1 + HDR is
        // Main 10, so a mid-session demotion lands here on that stream.
        if let Some(shape) = self.unsupported_sequence(au) {
            return Err(NoSoftwareRung {
                codec: punktfunk_core::quic::CODEC_AV1,
                shape: Some(shape),
            }
            .into());
        }
        // Own copy: `dav1d_data_wrap` over `au` would lend the decoder a buffer
        // the pump reuses on the next AU.
        let mut data = Av1Data::create(au)?;
        // Partial send leaves bytes; `EAGAIN` means drain pictures, not "bad AU".
        // Keep the newest picture (`decode -> Option<Frame>`). An AV1 TU can show
        // several frames; newest matches the pump queue.
        let mut out: Option<CpuPlanarFrame> = None;
        loop {
            // SAFETY: `ctx` is the live context from `dav1d_open` (not yet closed) and
            // `data.0` is a live local dav1d is allowed to read from and write to. Its
            // reference is taken by dav1d on success; the guard's `Drop` releases only
            // what is left.
            let r = unsafe { dav1d_send_data(Some(ctx), NonNull::new(&mut data.0)) };
            let sent = r.0 >= 0;
            if !sent && dav1d_errno(r) != Some(libc::EAGAIN) {
                // `ENOPROTOOPT` with no sequence header on this AU. Typed: a generic
                // `Err` would look survivable and freeze the session on every following AU.
                if dav1d_errno(r) == Some(libc::ENOPROTOOPT) {
                    return Err(NoSoftwareRung {
                        codec: punktfunk_core::quic::CODEC_AV1,
                        shape: Some("10-bit or deeper"),
                    }
                    .into());
                }
                bail!("rav1d send_data: {}", r.0);
            }
            if sent && data.0.sz == 0 {
                break;
            }
            match self.take_picture(ctx, color)? {
                Some(f) => out = Some(f),
                // Neither produced nor consumed: a wedge, not back-pressure.
                None if !sent => bail!("rav1d: decoder accepted no data and produced no picture"),
                None => {}
            }
        }
        // Drain past the first `EAGAIN`. `get_picture` only blocks on a call whose
        // `drain` flag the previous call set, and `send_data` clears it — so the
        // first `EAGAIN` means "ask again". Stopping there with `n_fc = 2` leaves
        // this AU's frame in flight (two frames of latency).
        let mut idle = 0;
        while idle < 2 {
            match self.take_picture(ctx, color)? {
                Some(f) => {
                    out = Some(f);
                    idle = 0;
                }
                None => idle += 1,
            }
        }
        Ok(out)
    }

    /// Sequence header vs the 8-bit 4:2:0 envelope. `None` = this AU has no
    /// sequence header (AV1 twin of `NoActiveParamSet`): decode against the
    /// sequence a previous AU was already checked for. Hosts resend it on
    /// every key frame; demotion requests one, so the first AU here has it.
    fn unsupported_sequence(&self, au: &[u8]) -> Option<&'static str> {
        let mut seq = std::mem::MaybeUninit::<Dav1dSequenceHeader>::uninit();
        // SAFETY: `out` is a live local this call either fully writes or leaves untouched
        // (it writes only on success), and `au` is a live slice of exactly `au.len()`
        // bytes. Nothing is allocated or referenced: dav1d fills the struct by value.
        let r = unsafe {
            dav1d_parse_sequence_header(
                NonNull::new(seq.as_mut_ptr()),
                NonNull::new(au.as_ptr().cast_mut()),
                au.len(),
            )
        };
        if r.0 < 0 {
            return None; // no sequence header (ENOENT), or an AU we cannot read
        }
        // SAFETY: the call returned success, which is its contract for having written
        // the whole `Dav1dSequenceHeader`.
        let seq = unsafe { seq.assume_init() };
        if seq.hbd != 0 {
            return Some("10-bit or deeper");
        }
        if seq.layout != DAV1D_PIXEL_LAYOUT_I420 {
            return Some("chroma other than 4:2:0");
        }
        None
    }

    fn take_picture(
        &self,
        ctx: Dav1dContext,
        color: &mut ColorDesc,
    ) -> Result<Option<CpuPlanarFrame>> {
        let mut pic = Dav1dPicture::default();
        // SAFETY: `ctx` is live and `pic` is a live local dav1d writes the picture into.
        let r =
            unsafe { dav1d_get_picture(Some(ctx), NonNull::new(&mut pic as *mut Dav1dPicture)) };
        if dav1d_errno(r) == Some(libc::EAGAIN) {
            return Ok(None);
        }
        if r.0 < 0 {
            bail!("rav1d get_picture: {}", r.0);
        }
        // Picture is ours: unref on every exit, including the refusals. Convert
        // first, unref after, so a `return Err` cannot skip it.
        let converted = Self::convert(&pic, color);
        // SAFETY: `pic` is the live picture `dav1d_get_picture` just wrote; this releases
        // exactly the one reference it handed over, once.
        unsafe { dav1d_picture_unref(NonNull::new(&mut pic as *mut Dav1dPicture)) };
        converted.map(Some)
    }

    fn convert(pic: &Dav1dPicture, color: &mut ColorDesc) -> Result<CpuPlanarFrame> {
        // Same typed refusal as H.264: 4:4:4 planes treated as 4:2:0 decode and
        // display wrong. Belt: `unsupported_sequence` already refused submit;
        // kept so a `bitdepth_16` build still rejects layout.
        let shape = if pic.p.bpc != 8 {
            Some("10-bit or deeper")
        } else if pic.p.layout != DAV1D_PIXEL_LAYOUT_I420 {
            Some("chroma other than 4:2:0")
        } else {
            None
        };
        if let Some(shape) = shape {
            return Err(NoSoftwareRung {
                codec: punktfunk_core::quic::CODEC_AV1,
                shape: Some(shape),
            }
            .into());
        }
        let (w, h) = (pic.p.w.max(0) as u32, pic.p.h.max(0) as u32);
        // Colour from the sequence header (resent on change), so an in-band
        // SDR↔HDR flip is followed rather than latched.
        if let Some(seq) = pic.seq_hdr {
            // SAFETY: `seq_hdr` belongs to the picture we hold a reference to, so it is
            // live for this call; these are plain scalar field reads.
            let seq = unsafe { seq.as_ref() };
            *color = ColorDesc {
                primaries: seq.pri as u8,
                transfer: seq.trc as u8,
                matrix: seq.mtrx as u8,
                full_range: seq.color_range != 0,
            };
        }
        let keyframe = pic.frame_hdr.is_some_and(|f| {
            // SAFETY: same as `seq_hdr` above — owned by the live picture, scalar read.
            unsafe { f.as_ref() }.frame_type == rav1d::include::dav1d::headers::DAV1D_FRAME_TYPE_KEY
        });
        let (_, ch) = CpuPlanarFrame::chroma_dims(w, h);
        // dav1d has one chroma stride (`stride[1]`) for both planes.
        let strides = [
            pic.stride[0].max(0) as usize,
            pic.stride[1].max(0) as usize,
            pic.stride[1].max(0) as usize,
        ];
        let sizes = [
            h as usize * strides[0],
            ch as usize * strides[1],
            ch as usize * strides[2],
        ];
        let mut planes: [&[u8]; 3] = [&[], &[], &[]];
        for i in 0..3 {
            let p = pic.data[i].with_context(|| format!("rav1d: plane {i} is null"))?;
            // SAFETY: dav1d's picture contract is that plane `i` spans
            // `height_of_plane * |stride|` bytes from `data[i]`, and the picture holds a
            // reference to that allocation for as long as we do (unref'd by the caller,
            // after this conversion returns). Negative strides (bottom-up pictures) are
            // rejected above by the `max(0)` collapsing them to a zero size, which the
            // copy below then refuses.
            planes[i] = unsafe { std::slice::from_raw_parts(p.as_ptr().cast::<u8>(), sizes[i]) };
        }
        // AV1 has no recovery-point SEI; hosts do not emit S-frames. Re-anchor
        // is the wire's, as on every lane except H.264/H.265.
        CpuPlanarFrame::from_i420(
            w,
            h,
            planes,
            strides,
            *color,
            keyframe,
            punktfunk_core::reanchor::LocalRecovery::NONE,
        )
    }
}

impl Drop for Av1Software {
    fn drop(&mut self) {
        if self.ctx.is_none() {
            return;
        }
        // SAFETY: `self.ctx` is the one context `dav1d_open` produced and this is its
        // sole owner, so this runs exactly once; `dav1d_close` takes it through the
        // `&mut` and leaves `None`.
        unsafe { dav1d_close(NonNull::new(&mut self.ctx as *mut Option<Dav1dContext>)) };
    }
}

/// Negated errno from a `Dav1dResult`, or `None` on success.
///
/// Match `libc`'s codes, not literals: `EAGAIN` is 11 everywhere, but
/// `ENOPROTOOPT` is 92 on Linux and 123 on Windows. `Rav1dError` is
/// `pub(crate)` in rav1d; only `Dav1dResult` is re-exported.
fn dav1d_errno(r: rav1d::Dav1dResult) -> Option<i32> {
    (r.0 < 0).then_some(-r.0)
}

/// rav1d's own `n_fc` for these settings (`dav1d_get_frame_delay`).
/// Not a copy of its arithmetic, so [`Av1Software::new`]'s floor cannot
/// drift. Negative (settings rav1d would refuse) becomes 0, which fails
/// the floor too.
fn frame_delay(settings: &mut Dav1dSettings) -> i32 {
    // SAFETY: `settings` is a live local for the duration of the call, which is this
    // function's whole contract — it `ptr::read`s the struct and writes nothing back. The
    // read is a bitwise copy of a plain-data struct (`Rav1dSettings` has no `Drop`), so it
    // cannot release anything the caller still owns.
    let r = unsafe { dav1d_get_frame_delay(NonNull::new(settings as *mut Dav1dSettings)) };
    r.0.max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::csc_rows;

    /// Nine bars in x order: saturated primaries/secondaries, black, white,
    /// then the RANGE axis.
    ///
    /// `(192, 128, 64)` is the only bar that can fail a limited↔full mismatch.
    /// Saturated bars clamp at [0, 1], so the 709-FULL fixture with the wrong
    /// range has max error 0. 50% grey is 3 (inside ±4). Off-neutral mid-tone
    /// is 11, and it exercises chroma scale too.
    const BARS: [(u8, u8, u8); 9] = [
        (255, 255, 255),
        (255, 255, 0),
        (0, 255, 255),
        (0, 255, 0),
        (255, 0, 255),
        (255, 0, 0),
        (0, 0, 255),
        (0, 0, 0),
        (192, 128, 64),
    ];

    /// Presenter planar CSC on the CPU: sample three planes, apply `csc_rows`
    /// as `planar_csc.frag` (`rgb[i] = dot(r[i].xyz, yuv) + r[i].w`, clamp).
    /// 8-bit, no MSB packing — this rung's only shape.
    ///
    /// `csc_rows` is the one coefficient source the shader, Windows CB, and
    /// Apple Swift port all fill from. Nearest at bar centres, where 4:2:0
    /// siting and the linear filter do not matter.
    fn shader_rgb(f: &CpuPlanarFrame, x: u32, y: u32) -> [u8; 3] {
        let rows = csc_rows(f.color, 8, false);
        let (cw, _) = CpuPlanarFrame::chroma_dims(f.width, f.height);
        let luma = f.plane(0)[(y * f.width + x) as usize];
        let (cx, cy) = (x / 2, y / 2);
        let cb = f.plane(1)[(cy * cw + cx) as usize];
        let cr = f.plane(2)[(cy * cw + cx) as usize];
        let yuv = [luma as f32 / 255.0, cb as f32 / 255.0, cr as f32 / 255.0];
        core::array::from_fn(|i| {
            let v = rows[i][0] * yuv[0] + rows[i][1] * yuv[1] + rows[i][2] * yuv[2] + rows[i][3];
            (v.clamp(0.0, 1.0) * 255.0).round() as u8
        })
    }

    fn decode_one(codec: u8, au: &[u8]) -> CpuPlanarFrame {
        let mut dec = SoftwareDecoder::new(codec).expect("software decoder");
        dec.decode(au)
            .expect("decode")
            .expect("no frame out of the fixture")
    }

    /// Three H.264 colour-bar fixtures whose VUIs differ only in matrix and
    /// range (`tests/gen-bars.sh`): decode on this rung, convert with
    /// `csc_rows`, require the original RGB. Every assertion is a pixel that
    /// depends on colour signalling surviving the path.
    ///
    /// 601 vs 709 on saturated bars is tens of code points (red's green
    /// ~0 → ~40). Range needs [`BARS`]'s ninth sample — the eight saturated
    /// bars clamp a limited↔full mismatch to max error 0. A Cb/Cr swap or
    /// stride error shears or swaps bars.
    #[test]
    fn software_h264_reproduces_the_golden_bars_in_both_ranges() {
        let fixtures: [(&str, &[u8], ColorDesc); 3] = [
            (
                "601-limited",
                include_bytes!("../tests/bars-601-limited.h264"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 5, // BT.470BG: Linux host RGB-input NVENC
                    full_range: false,
                },
            ),
            (
                "709-limited",
                include_bytes!("../tests/bars-709-limited.h264"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    full_range: false,
                },
            ),
            (
                "709-full",
                include_bytes!("../tests/bars-709-full.h264"),
                ColorDesc {
                    primaries: 1,
                    transfer: 1,
                    matrix: 1,
                    full_range: true,
                },
            ),
        ];
        for (name, au, want_color) in fixtures {
            let f = decode_one(punktfunk_core::quic::CODEC_H264, au);
            assert_eq!(f.color, want_color, "{name}: signalling");
            assert_eq!((f.width, f.height), (288, 64), "{name}: dims");
            assert!(f.keyframe, "{name}: the fixture is a single IDR");
            for (i, (r, g, b)) in BARS.iter().enumerate() {
                let px = shader_rgb(&f, i as u32 * 32 + 16, 32);
                for (got, want) in px.iter().zip([r, g, b]) {
                    assert!(
                        got.abs_diff(*want) <= 4,
                        "{name} bar {i}: got {px:?}, want ({r},{g},{b})"
                    );
                }
            }
        }
    }

    /// The 601 and 709 pictures are different pixels. A rung that ignored
    /// signalling and converted both with one matrix would still pass a
    /// self-consistency check. Also guards the fixtures: identical bitstreams
    /// would make the colour test vacuously green.
    #[test]
    fn the_601_and_709_fixtures_really_do_carry_different_luma() {
        let f601 = decode_one(
            punktfunk_core::quic::CODEC_H264,
            include_bytes!("../tests/bars-601-limited.h264"),
        );
        let f709 = decode_one(
            punktfunk_core::quic::CODEC_H264,
            include_bytes!("../tests/bars-709-limited.h264"),
        );
        // Pure red: Y ≈ 76 under 601, ≈ 54 under 709 (then 16..235). Same
        // displayed colour, different code points — why the threshold is > 10.
        let (x, y) = (5 * 32 + 16, 32);
        let a = f601.plane(0)[(y * f601.width + x) as usize];
        let b = f709.plane(0)[(y * f709.width + x) as usize];
        assert!(
            a.abs_diff(b) > 10,
            "601 luma {a} vs 709 luma {b} — the fixtures do not differ, so the colour \
             test above proves nothing"
        );
        for (f, name) in [(&f601, "601"), (&f709, "709")] {
            let px = shader_rgb(f, x, y);
            assert!(
                px[0].abs_diff(255) <= 4 && px[1] <= 4 && px[2] <= 4,
                "{name}: red bar came out {px:?}"
            );
        }
    }

    /// HEVC has no CPU rung. Must be [`NoSoftwareRung`], not a string and not `Ok(None)`.
    #[test]
    fn hevc_is_refused_with_the_typed_no_rung_error() {
        let err = SoftwareDecoder::new(punktfunk_core::quic::CODEC_HEVC)
            .err()
            .expect("HEVC must not build a software decoder");
        let typed = err
            .downcast_ref::<NoSoftwareRung>()
            .expect("the refusal must survive as NoSoftwareRung through anyhow");
        assert_eq!(typed.codec, punktfunk_core::quic::CODEC_HEVC);
        assert_eq!(typed.shape, None, "the CODEC is missing, not a shape");
        assert!(err.to_string().contains("HEVC"), "{err}");
        assert!(SoftwareDecoder::new(punktfunk_core::quic::CODEC_H264).is_ok());
        assert!(SoftwareDecoder::new(punktfunk_core::quic::CODEC_AV1).is_ok());
    }

    /// A shape this rung cannot decode is the same typed refusal as a missing
    /// codec (reconnect). Else: `Err` every AU, or 8-bit maths on 10-bit samples.
    /// Covers Main 10 (in-band, so the active SPS not Welcome) and 4:4:4.
    #[test]
    fn a_shape_the_cpu_rung_cannot_decode_is_the_same_typed_refusal() {
        use punktfunk_core::quic::{CHROMA_IDC_420, CHROMA_IDC_444};
        assert_eq!(unsupported_shape(CHROMA_IDC_420, 0), None);
        assert_eq!(
            unsupported_shape(CHROMA_IDC_420, 2),
            Some("10-bit or deeper")
        );
        assert_eq!(
            unsupported_shape(CHROMA_IDC_444, 0),
            Some("chroma other than 4:2:0")
        );
        // Depth first when both are wrong: mis-scale is worse than mis-siting.
        assert_eq!(
            unsupported_shape(CHROMA_IDC_444, 2),
            Some("10-bit or deeper")
        );
        let e: anyhow::Error = NoSoftwareRung {
            codec: punktfunk_core::quic::CODEC_AV1,
            shape: Some("10-bit or deeper"),
        }
        .into();
        let typed = e.downcast_ref::<NoSoftwareRung>().expect("typed");
        assert_eq!(typed.shape, Some("10-bit or deeper"));
        assert!(e.to_string().contains("AV1"), "{e}");
        assert!(e.to_string().contains("8-bit 4:2:0 only"), "{e}");
    }

    /// The 709-full fixture must be able to fail the range axis. Saturated
    /// bars clamp a limited↔full mismatch; [`BARS`]'s mid-tone is what makes
    /// the range half of the test above non-vacuous.
    #[test]
    fn the_full_range_fixture_is_decoded_wrong_by_the_wrong_range() {
        let f = decode_one(
            punktfunk_core::quic::CODEC_H264,
            include_bytes!("../tests/bars-709-full.h264"),
        );
        let wrong = ColorDesc {
            full_range: false,
            ..f.color
        };
        let rows = csc_rows(wrong, 8, false);
        let (cw, _) = CpuPlanarFrame::chroma_dims(f.width, f.height);
        let mut worst = 0u8;
        for (i, (r, g, b)) in BARS.iter().enumerate() {
            let (x, y) = (i as u32 * 32 + 16, 32u32);
            let luma = f.plane(0)[(y * f.width + x) as usize];
            let cb = f.plane(1)[((y / 2) * cw + x / 2) as usize];
            let cr = f.plane(2)[((y / 2) * cw + x / 2) as usize];
            let yuv = [luma as f32 / 255.0, cb as f32 / 255.0, cr as f32 / 255.0];
            let px: [u8; 3] = core::array::from_fn(|c| {
                let v =
                    rows[c][0] * yuv[0] + rows[c][1] * yuv[1] + rows[c][2] * yuv[2] + rows[c][3];
                (v.clamp(0.0, 1.0) * 255.0).round() as u8
            });
            for (got, want) in px.iter().zip([r, g, b]) {
                worst = worst.max(got.abs_diff(*want));
            }
        }
        assert!(
            worst > 4,
            "decoding the FULL-range fixture as LIMITED was off by only {worst}, inside \
             the ±4 tolerance — the fixture no longer tests the range axis at all"
        );
    }

    /// A 10-bit AV1 stream must be [`NoSoftwareRung`], not a per-AU `Err`.
    /// rav1d `bitdepth_8` refuses with `ENOPROTOOPT`; a generic `anyhow` looks
    /// survivable (keyframe, next AU still 10-bit, freeze). Hardware AV1 + HDR
    /// is Main 10, so a mid-session demotion lands here.
    ///
    /// 38-byte 10-bit temporal unit inline; the sequence header is the test.
    #[test]
    fn a_10bit_av1_stream_is_refused_with_the_typed_no_rung_error() {
        const TU_10BIT: [u8; 38] = [
            0x12, 0x00, 0x0a, 0x0a, 0x00, 0x00, 0x00, 0x02, 0xaf, 0xff, 0x8d, 0x5f, 0x38, 0x08,
            0x32, 0x16, 0x10, 0x00, 0xba, 0x02, 0x0b, 0x2c, 0x51, 0x41, 0x00, 0x00, 0x08, 0x00,
            0x95, 0xd1, 0xe2, 0x7e, 0xac, 0x4f, 0x04, 0xad, 0xa4, 0x70,
        ];
        let mut dec = SoftwareDecoder::new(punktfunk_core::quic::CODEC_AV1).expect("av1 decoder");
        let err = dec
            .decode(&TU_10BIT)
            .err()
            .expect("a 10-bit AV1 AU must not decode on an 8-bit-only build");
        let typed = err.downcast_ref::<NoSoftwareRung>().expect(
            "the refusal must survive as NoSoftwareRung through anyhow — a generic \
                     error here is the permanent freeze this test exists to prevent",
        );
        assert_eq!(typed.codec, punktfunk_core::quic::CODEC_AV1);
        assert_eq!(typed.shape, Some("10-bit or deeper"));
        // `RungLoss::Shape`: the retry is not narrowed to codecs with a CPU rung.
        assert_eq!(typed.loss(), crate::video::RungLoss::Shape);
        // Same refusal on the next AU; a wedge here is the failure mode.
        assert!(dec
            .decode(&TU_10BIT)
            .err()
            .and_then(|e| e.downcast_ref::<NoSoftwareRung>().copied())
            .is_some());
    }

    /// Never open rav1d with one frame context.
    ///
    /// `n_fc == 1` makes rav1d `abort()` on any decode error: a panic inside
    /// `extern "C"` `dav1d_send_data`. Nothing in this crate can catch it.
    /// Asserted against rav1d's own `n_fc` (`dav1d_get_frame_delay`).
    #[test]
    fn the_av1_rung_never_opens_rav1d_with_a_single_frame_context() {
        let mut s = av1_settings();
        let n_fc = frame_delay(&mut s);
        assert!(
            n_fc >= AV1_MIN_FRAME_CONTEXTS,
            "rav1d would run with n_fc={n_fc} (n_threads={}, max_frame_delay={}) — one \
             frame context ABORTS the process on the first damaged AU",
            s.n_threads,
            s.max_frame_delay,
        );
        // Constructor refuses such a decoder: lose the rung, not the process.
        assert!(Av1Software::new().is_ok());
    }

    /// `n_fc = min(max_frame_delay, n_threads)`. `max_frame_delay = 2` alone
    /// is not enough: one decode thread sets `n_fc` back to 1 and restores
    /// the abort. Asserted against rav1d's own arithmetic.
    #[test]
    fn one_decode_thread_would_put_the_rung_back_on_the_aborting_path() {
        let mut one_thread = av1_settings();
        one_thread.n_threads = 1;
        assert_eq!(
            frame_delay(&mut one_thread),
            1,
            "n_fc is min(max_frame_delay, n_threads) — this is why n_threads has a floor"
        );
        // `av1_settings` floors `n_threads` so shipping stays off that path.
        assert!(av1_settings().n_threads >= AV1_MIN_FRAME_CONTEXTS);
    }

    /// Decode a real AV1 stream and report the sequence header's colour.
    /// Fixture: vendored cros-codecs IVF; first TU is a key frame. Covers
    /// rav1d FFI, I420 copy, and colour — none of which the H.264 leg hits.
    #[test]
    fn software_av1_decodes_and_reports_its_sequence_colour() {
        let mut dec = SoftwareDecoder::new(punktfunk_core::quic::CODEC_AV1).expect("av1 decoder");
        let mut first = None;
        let (mut units, mut frames) = (0u32, 0u32);
        // Whole vector: send loop + `Dav1dData` drop once per TU, so a leak or
        // wedge fails the run instead of drifting.
        for unit in pf_bitstream::testing::split_ivf(pf_bitstream::testing::AV1_25FPS) {
            units += 1;
            if let Some(f) = dec.decode(unit).expect("av1 decode") {
                frames += 1;
                if first.is_none() {
                    first = Some(f);
                }
            }
        }
        assert!(
            units > 100,
            "expected the full 25 fps vector, got {units} units"
        );
        // Latency guard for `n_fc = 2`. Stopping `get_picture` at the first
        // `EAGAIN` still decodes every frame, two AUs late, and `frames` would
        // be `n_fc` short of `units`. Equality means this AU's picture returned
        // in this call.
        assert_eq!(frames, units, "every temporal unit here shows a picture");
        let f = first.expect("no AV1 frame decoded");
        assert_eq!((f.width, f.height), (320, 240));
        assert!(f.keyframe, "the first temporal unit is a key frame");
        // Unspecified (2) must survive: `csc_rows` maps it to BT.709 SDR.
        // Resolving early would hide an in-band HDR flip.
        assert_eq!(f.color.matrix, 2, "unspecified matrix must survive as 2");
        assert!(!f.color.full_range);
        // Tightly packed at picture size: the presenter uploads with no stride.
        assert_eq!(f.plane(0).len(), (f.width * f.height) as usize);
        let (cw, ch) = CpuPlanarFrame::chroma_dims(f.width, f.height);
        assert_eq!(f.plane(1).len(), (cw * ch) as usize);
        assert_eq!(f.plane(2).len(), (cw * ch) as usize);
    }
}
