//! Protocol v7: the driver encodes, and the host reads access units instead of pixels.
//!
//! It replaced the pixel ring rather than joining it — one transport, as
//! `design/windows-video-plane-overhaul.md` §1.1 decided. The host still owns the memory: it
//! creates an unnamed section plus a ready event, duplicates both into WUDFHost and delivers the
//! values over [`IOCTL_SET_ENCODE`], which also carries codec, mode, bitrate, HDR metadata and an
//! ordered backend preference list. The driver opens the first backend that works and answers with
//! [`SetEncodeReply`] — the backend that took, its [`EncoderCapsWire`] and the applied bitrate, or
//! a named failure. No silent fallback.
//!
//! Steady state lives in [`au`]: a 128-byte header, a 16-entry slot table and a bitstream heap the
//! encode thread writes into. Publishing goes through [`FrameToken`], so the host takes a slot only
//! under a generation check. Runtime control — keyframe, RFI, bitrate, HDR metadata, reset, flush —
//! travels as one-shot [`IOCTL_ENCODE_CTL`] calls on the framework queue that already carries
//! `PING`.

use super::ctl_code;
use bytemuck::{Pod, Zeroable};

pub mod au;
pub mod backend;
pub mod codec;
pub mod slots;

pub use slots::{offer_slot, republish_slot, BypassHold, OfferSlot};

/// The publish cell of [`au::AuHeader::latest`]: `(generation << 40) | (seq << 8) | slot`,
/// with `generation` 24-bit, `seq` 32-bit and `slot` 8-bit. `generation` is bumped on every
/// [`IOCTL_SET_ENCODE`], so a publish an old encoder left behind is rejected, never consumed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameToken {
    pub generation: u32,
    pub seq: u32,
    pub slot: u8,
}

impl FrameToken {
    /// Low 24 bits of `generation` are significant.
    pub const GENERATION_MASK: u32 = 0x00FF_FFFF;

    #[must_use]
    pub const fn pack(self) -> u64 {
        (((self.generation & Self::GENERATION_MASK) as u64) << 40)
            | (((self.seq as u64) & 0xFFFF_FFFF) << 8)
            | (self.slot as u64)
    }

    #[must_use]
    pub const fn unpack(v: u64) -> Self {
        Self {
            generation: ((v >> 40) as u32) & Self::GENERATION_MASK,
            seq: ((v >> 8) & 0xFFFF_FFFF) as u32,
            slot: (v & 0xFF) as u8,
        }
    }
}

/// [`au::AuHeader::driver_status`] values. UMDF hides `OutputDebugString` and the restricted
/// token blocks file writes, so this word is how a driver with no debugger reports state.
pub const DRV_STATUS_NONE: u32 = 0;
/// An encoder is open on this section.
pub const DRV_STATUS_OPENED: u32 = 1;

/// Open the encoder for one monitor and adopt its AU section + ready event. Input
/// [`SetEncodeRequest`], output [`SetEncodeReply`]. A resolution, codec or HDR change is a new
/// SET_ENCODE — the same tear-down-and-rebuild the host does today.
pub const IOCTL_SET_ENCODE: u32 = ctl_code(0x90C);
/// One-shot control on the live encoder. Input [`EncodeCtlRequest`], no output.
pub const IOCTL_ENCODE_CTL: u32 = ctl_code(0x90D);

/// [`IOCTL_SET_ENCODE`] input. `section` and `event` are handle VALUES already duplicated into
/// WUDFHost, adopt-on-success-only exactly as
/// [`SetFrameChannelRequest`](crate::control::SetFrameChannelRequest): the driver owns and
/// closes them IFF the IOCTL succeeds, and the host reaps with `DUPLICATE_CLOSE_SOURCE` on any
/// error. Closing on error double-closes a possibly-reused handle value.
///
/// Everything the backend needs to open travels in one request, so opening is not a
/// negotiation: the driver walks `backends` in order and reports what took.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct SetEncodeRequest {
    /// OS target id of the monitor to encode (the [`AddReply`](crate::control::AddReply) one).
    pub target_id: u32,
    /// Aligns `section` to 8 (Pod forbids implicit padding).
    pub _pad: u32,
    /// AU section mapping handle VALUE; the driver maps it and writes [`au::AuHeader`].
    pub section: u64,
    /// Event handle VALUE the driver signals after each publish.
    pub event: u64,
    /// Bytes the host allocated for the section — [`au::section_bytes`].
    pub section_bytes: u32,
    /// A [`codec`] id, the same numbering
    /// [`EncodeProbeRequest`](crate::control::EncodeProbeRequest) uses.
    pub codec: u32,
    /// `0` = 4:2:0, `1` = 4:4:4. Not the H.264/HEVC `chroma_format_idc`.
    pub chroma: u32,
    /// Bits per component the encoder emits: 8 or 10.
    pub bit_depth: u32,
    pub width: u32,
    pub height: u32,
    /// Target frame rate; with `bitrate_kbps` it sizes the heap ([`au::heap_bytes_for`]).
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// `1` = PQ BT.2020 stream and `hdr_meta` is valid.
    pub hdr: u32,
    /// `pf_frame::HdrMeta` as its 28 raw bytes — opaque here, this crate has no HDR types.
    pub hdr_meta: [u8; 28],
    /// Slice-chunk target for `Encoder::set_wire_chunking`; `0` = publish whole AUs.
    pub wire_chunk_bytes: u32,
    /// First `wire_seq` the driver stamps, so the host's `au_seq` domain survives a DriverCycle.
    pub wire_seq_base: u32,
    /// Ordered preference, 0-terminated. [`backend`] ids.
    pub backends: [u32; 4],
    /// `SET_ENCODE_FLAG_*` bits; unknown bits are ignored.
    pub flags: u32,
    /// H.264/HEVC slices per frame the client takes. A host that leaves it zero gets
    /// single-slice, which is also all a driver older than the field encodes.
    pub slices: u32,
    /// Encoder knobs from the host's `host.env`. Prefix-compatible after
    /// [`SET_ENCODE_REQUEST_LEGACY_SIZE`]: an old host sends none and the driver zero-fills,
    /// which is every backend's default; an old driver reads the prefix and ignores them.
    pub knobs: EncodeKnobs,
    /// The size the stream is encoded at when it is not the surface's; zero is the surface's
    /// own. Protocol v10, after [`SET_ENCODE_REQUEST_V9_SIZE`]: `width` and `height` stay the
    /// surface's, so a v9 driver, which reads none of this, opens the plain session.
    pub encode_width: u32,
    pub encode_height: u32,
    /// The rectangle of the `width` × `height` surface that is encoded, `x, y, w, h`, scaled to
    /// the encode size. Zero is the whole surface. [`SetEncodeRequest::framing`] resolves both.
    pub src_rect: [u32; 4],
}

/// Bytes of [`SetEncodeRequest`] before [`SetEncodeRequest::encode_width`]: what a v9 host sends
/// and a v9 driver reads.
pub const SET_ENCODE_REQUEST_V9_SIZE: usize = 144;

/// The picture a session encodes: `crop` of each `source`-sized surface, scaled to `encode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Framing {
    pub source: (u32, u32),
    /// `x, y, w, h` in surface pixels.
    pub crop: [u32; 4],
    pub encode: (u32, u32),
}

impl Framing {
    /// Anything but the whole surface at its own size: the session runs the scale pass.
    #[must_use]
    pub fn scales(&self) -> bool {
        self.encode != self.source || self.crop != [0, 0, self.source.0, self.source.1]
    }
}

impl SetEncodeRequest {
    /// What this request encodes. A zero rectangle is the whole surface; a zero encode size is
    /// the rectangle's own. `None` for a rectangle with an empty side or past the surface, or an
    /// encode size with one empty side.
    #[must_use]
    pub fn framing(&self) -> Option<Framing> {
        let source = (self.width, self.height);
        let crop = if self.src_rect == [0; 4] {
            [0, 0, self.width, self.height]
        } else {
            self.src_rect
        };
        let [x, y, w, h] = crop;
        let inside = x.checked_add(w).is_some_and(|r| r <= self.width)
            && y.checked_add(h).is_some_and(|b| b <= self.height);
        let encode = match (self.encode_width, self.encode_height) {
            (0, 0) => (w, h),
            (0, _) | (_, 0) => return None,
            size => size,
        };
        (w != 0 && h != 0 && inside).then_some(Framing {
            source,
            crop,
            encode,
        })
    }
}

/// [`SetEncodeRequest::flags`]: the display composes FP16 scRGB under SDR wide colour, and the
/// stream is BT.709 SDR. Sent only to a driver that declared wide colour, so an older one, which
/// would read BGRA from an FP16 surface, never sees it.
pub const SET_ENCODE_FLAG_SDR_FP16: u32 = 1;

/// [`SetEncodeRequest::flags`]: encode and signal studio range, because the host advertised
/// studio for this session. The GPU's own RGB conversion writes studio whatever is asked (AMF
/// on RDNA4); the YUV this side converts follows it. A driver older than the bit ignores it.
pub const SET_ENCODE_FLAG_STUDIO_RANGE: u32 = 2;

/// Bytes of [`SetEncodeRequest`] before [`SetEncodeRequest::knobs`]: what a host older than
/// the knobs sends, and what a driver older than them reads.
pub const SET_ENCODE_REQUEST_LEGACY_SIZE: usize = 120;

/// Encoder tuning the host resolves once from `host.env` and hands to the driver in the
/// request, so a knob takes effect on the next session instead of after `setx /M` plus a
/// driver restart. Zero is every backend's own default. Each field names the environment
/// variable it replaces; [`EncodeKnobs::apply_env`] is the one parser for those, and the
/// encoder crate still reads the same variables inside WUDFHost as a dev override.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, Default, PartialEq, Eq)]
pub struct EncodeKnobs {
    /// `PUNKTFUNK_IR_PERIOD_FRAMES`: intra-refresh wave length in frames, `>= 2`; `0` = half
    /// a second of frames.
    pub ir_period_frames: u16,
    /// `PUNKTFUNK_LTR_INTERVAL_FRAMES`: frames between LTR marks; `0` = the backend's tuning.
    pub ltr_interval_frames: u16,
    /// `PUNKTFUNK_LTR_FORCE_AT`: spike-only self-triggered RFI at this frame; `0` = off.
    pub ltr_force_at: u16,
    /// `PUNKTFUNK_SPLIT_ENCODE`: `0` = by pixel rate, `1` = disable, `2` = auto-forced,
    /// `3` = two engines, `4` = three engines.
    pub split_encode: u8,
    /// `PUNKTFUNK_NVENC_ASYNC`: `1` = the two-thread retrieve, `2` = never pipelined (a falsy
    /// value), `0` = unset: the Linux backend escalates on demand.
    pub nvenc_async: u8,
    /// `PUNKTFUNK_NVENC_ASYNC_DEPTH`: in-flight encodes in async mode; `0` = 4.
    pub nvenc_async_depth: u8,
    /// `PUNKTFUNK_NVENC_SLICES`: H.264/HEVC slices, `1..=32`; `0` = the session's default.
    pub nvenc_slices: u8,
    /// `PUNKTFUNK_NVENC_SUBFRAME`: `0` = the GPU's cap decides, `1` = never, `2` = force.
    pub nvenc_subframe: u8,
    /// `PUNKTFUNK_NVENC_MAX_SESSIONS`: concurrent-session budget; `0` = 8.
    pub nvenc_max_sessions: u8,
    /// `PUNKTFUNK_NVENC_SPLIT_ARBITRATE`: `1` = arm the live split experiment.
    pub nvenc_split_arbitrate: u8,
    /// `PUNKTFUNK_INTRA_REFRESH`: `0` = the on-demand wave only, `1` = the periodic wave on
    /// AMF/QSV too, `2` = no wave, IDR on every loss.
    pub intra_refresh: u8,
    /// `PUNKTFUNK_AMF_USAGE`: `0` = ultralowlatency, `1` = lowlatency,
    /// `2` = lowlatency_high_quality, `3` = transcoding, `4` = highquality.
    pub amf_usage: u8,
    /// `PUNKTFUNK_NO_AMF_LTR`: `1` = IDR-only loss recovery on AMF.
    pub no_amf_ltr: u8,
    /// `PUNKTFUNK_NO_QSV_LTR`: `1` = IDR-only loss recovery on QSV.
    pub no_qsv_ltr: u8,
    /// `PUNKTFUNK_VBV_FRAMES` in tenths of a frame interval; `0` = 10 (one frame).
    pub vbv_tenths: u8,
    /// `PUNKTFUNK_PYROWAVE_STREAMED_AU`: `1` = arm streamed PyroWave AUs.
    pub pyrowave_streamed_au: u8,
    /// `PUNKTFUNK_PYROWAVE_CHUNK_KIB` / 64: streamed-AU chunk target; `0` = 256 KiB.
    pub pyrowave_chunk_64kib: u8,
    /// `PUNKTFUNK_NVENC_PRESET`: `1..=7` = P1..P7; `0` = P1.
    pub nvenc_preset: u8,
    /// Reserved; send `0`.
    pub _reserved: [u8; 3],
}

/// The trimmed, case-folded truthy set every `PUNKTFUNK_*` flag shares.
#[must_use]
pub fn truthy(v: &str) -> bool {
    let v = v.trim();
    v == "1"
        || v.eq_ignore_ascii_case("true")
        || v.eq_ignore_ascii_case("yes")
        || v.eq_ignore_ascii_case("on")
}

impl EncodeKnobs {
    /// Every variable [`Self::apply_env`] knows, for a caller that walks the environment.
    pub const ENV_NAMES: [&'static str; 18] = [
        "PUNKTFUNK_SPLIT_ENCODE",
        "PUNKTFUNK_NVENC_ASYNC",
        "PUNKTFUNK_NVENC_ASYNC_DEPTH",
        "PUNKTFUNK_NVENC_SLICES",
        "PUNKTFUNK_NVENC_SUBFRAME",
        "PUNKTFUNK_NVENC_MAX_SESSIONS",
        "PUNKTFUNK_NVENC_SPLIT_ARBITRATE",
        "PUNKTFUNK_INTRA_REFRESH",
        "PUNKTFUNK_IR_PERIOD_FRAMES",
        "PUNKTFUNK_LTR_INTERVAL_FRAMES",
        "PUNKTFUNK_LTR_FORCE_AT",
        "PUNKTFUNK_AMF_USAGE",
        "PUNKTFUNK_NO_AMF_LTR",
        "PUNKTFUNK_NO_QSV_LTR",
        "PUNKTFUNK_VBV_FRAMES",
        "PUNKTFUNK_PYROWAVE_STREAMED_AU",
        "PUNKTFUNK_PYROWAVE_CHUNK_KIB",
        "PUNKTFUNK_NVENC_PRESET",
    ];

    /// Set the knob `name` names from its environment spelling. Unparseable values leave
    /// the field alone, as the encoders always did. `false` = not a knob name.
    pub fn apply_env(&mut self, name: &str, value: &str) -> bool {
        let v = value.trim();
        let num = |lo: u32, hi: u32| v.parse::<u32>().ok().filter(|n| (lo..=hi).contains(n));
        match name {
            "PUNKTFUNK_SPLIT_ENCODE" => {
                self.split_encode = match v {
                    "0" | "disable" => 1,
                    "1" | "auto" => 2,
                    "2" => 3,
                    "3" => 4,
                    _ => self.split_encode,
                }
            }
            "PUNKTFUNK_NVENC_ASYNC" => {
                let falsy = ["0", "false", "no", "off"]
                    .iter()
                    .any(|f| v.eq_ignore_ascii_case(f));
                self.nvenc_async = if truthy(v) {
                    1
                } else if falsy {
                    2
                } else {
                    0
                }
            }
            "PUNKTFUNK_NVENC_ASYNC_DEPTH" => {
                if let Some(n) = num(1, 255) {
                    self.nvenc_async_depth = n as u8;
                }
            }
            "PUNKTFUNK_NVENC_SLICES" => {
                if let Some(n) = num(1, 32) {
                    self.nvenc_slices = n as u8;
                }
            }
            "PUNKTFUNK_NVENC_SUBFRAME" => {
                self.nvenc_subframe = match v {
                    "0" => 1,
                    "1" => 2,
                    _ => self.nvenc_subframe,
                }
            }
            "PUNKTFUNK_NVENC_PRESET" => {
                let n = v.strip_prefix(['p', 'P']).unwrap_or(v);
                if let Some(n) = n.parse::<u8>().ok().filter(|n| (1..=7).contains(n)) {
                    self.nvenc_preset = n;
                }
            }
            "PUNKTFUNK_NVENC_MAX_SESSIONS" => {
                if let Some(n) = num(1, 255) {
                    self.nvenc_max_sessions = n as u8;
                }
            }
            "PUNKTFUNK_NVENC_SPLIT_ARBITRATE" => self.nvenc_split_arbitrate = (v == "1") as u8,
            "PUNKTFUNK_INTRA_REFRESH" => {
                self.intra_refresh = if v == "0" {
                    2
                } else if truthy(v) {
                    1
                } else {
                    0
                }
            }
            "PUNKTFUNK_IR_PERIOD_FRAMES" => {
                if let Some(n) = num(2, u16::MAX as u32) {
                    self.ir_period_frames = n as u16;
                }
            }
            "PUNKTFUNK_LTR_INTERVAL_FRAMES" => {
                if let Some(n) = num(1, u16::MAX as u32) {
                    self.ltr_interval_frames = n as u16;
                }
            }
            "PUNKTFUNK_LTR_FORCE_AT" => {
                if let Some(n) = num(1, u16::MAX as u32) {
                    self.ltr_force_at = n as u16;
                }
            }
            "PUNKTFUNK_AMF_USAGE" => {
                self.amf_usage = match v {
                    "ultralowlatency" => 0,
                    "lowlatency" => 1,
                    "lowlatency_high_quality" => 2,
                    "transcoding" => 3,
                    "highquality" | "high_quality" => 4,
                    _ => self.amf_usage,
                }
            }
            "PUNKTFUNK_NO_AMF_LTR" => self.no_amf_ltr = truthy(v) as u8,
            "PUNKTFUNK_NO_QSV_LTR" => self.no_qsv_ltr = truthy(v) as u8,
            "PUNKTFUNK_VBV_FRAMES" => {
                // Tenths, so `1.5` survives the byte; `0` and garbage keep the default.
                if let Some(t) = parse_tenths(v).filter(|t| (1..=255).contains(t)) {
                    self.vbv_tenths = t as u8;
                }
            }
            "PUNKTFUNK_PYROWAVE_STREAMED_AU" => self.pyrowave_streamed_au = (v == "1") as u8,
            "PUNKTFUNK_PYROWAVE_CHUNK_KIB" => {
                // 4..=8192 KiB in the encoder; the byte carries 64 KiB steps, so the floor
                // rounds up to one step.
                if let Some(k) = num(4, 8192) {
                    self.pyrowave_chunk_64kib = k.div_ceil(64) as u8;
                }
            }
            _ => return false,
        }
        true
    }

    /// `PUNKTFUNK_VBV_FRAMES` as the encoders read it: frame intervals, default one.
    #[must_use]
    pub fn vbv_frames(&self) -> f64 {
        if self.vbv_tenths == 0 {
            1.0
        } else {
            f64::from(self.vbv_tenths) / 10.0
        }
    }
}

/// `"1.5"` → `15`; integers and one decimal only, no exponent. Negative or empty = `None`.
fn parse_tenths(v: &str) -> Option<u32> {
    let (whole, frac) = match v.split_once('.') {
        Some((w, f)) => (w, f),
        None => (v, ""),
    };
    let whole: u32 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let tenth: u32 = match frac.as_bytes().first() {
        None => 0,
        Some(d) if d.is_ascii_digit() => u32::from(d - b'0'),
        Some(_) => return None,
    };
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(whole.checked_mul(10)? + tenth)
}

/// The encoder input one [`SetEncodeRequest`] resolves to, so the chroma the reply promises
/// and the pixels the backend is handed come from the same decision. The driver owns the
/// D3D targets behind each variant; this crate owns only the choice and what it can carry.
///
/// [`Self::full_chroma`] is the honest ceiling for [`EncoderCapsWire::chroma_444`]: a
/// subsampled input cannot become 4:4:4 downstream, whatever the request asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeInput {
    /// BGRA straight into the backend, which does the RGB→YUV CSC.
    Bgra,
    /// Video-engine BGRA→NV12, 8-bit 4:2:0.
    Nv12,
    /// Shader FP16 scRGB→P010 PQ, 10-bit 4:2:0.
    P010,
    /// Video-engine BGRA→P010, 10-bit 4:2:0 BT.709 (10-bit SDR): an 8-bit capture widened to a
    /// Main10 stream under BT.709, no HDR volume. AMF and QSV; NVENC widens from `Bgra` itself.
    P010Sdr,
    /// Shader FP16 scRGB→packed `R10G10B10A2` PQ BT.2020; the backend CSCs to 4:4:4 itself.
    Rgb10,
    /// FP16 scRGB straight into the backend, which converts to BT.2020 PQ itself. AMF only.
    Fp16,
    /// Shader FP16 scRGB→packed `R10G10B10A2`, sRGB curve on BT.709: SDR wide colour, the
    /// desktop's own bits past 8. NVENC; it CSCs itself, at full chroma.
    Rgb10Wcg,
    /// Shader FP16 scRGB→P010 BT.709 at the session's range: SDR wide colour for AMF and QSV.
    P010Wcg,
    /// Shareable Y + CbCr planes plus a fence, as PyroWave's own Vulkan device imports them.
    /// `hdr` is BT.2020 PQ and `wcg` BT.709 from FP16; either writes 10-bit codes into 16-bit
    /// planes.
    Planar {
        hdr: bool,
        wcg: bool,
        chroma444: bool,
    },
}

impl EncodeInput {
    /// The input for `backend` (the [`SetEncodeRequest::backends`] numbering) under the
    /// request's HDR, depth and 4:4:4 flags. Every input carries the session's range: full, or
    /// studio under [`SET_ENCODE_FLAG_STUDIO_RANGE`]. NVENC and AMF ingest
    /// BGRA (AMF also FP16 under HDR) and convert it themselves — NVENC's conversion follows
    /// the signalled range, AMF takes it as a property — which keeps the conversion off the 3D
    /// engine a game renders on. QSV's own RGB conversion is studio range whatever is
    /// signalled, so it reads the host's converters: NV12, P010, or BT.709 P010 (`P010Sdr`).
    /// Only NVENC ingests packed 10-bit RGB at full chroma; QSV takes P010 under HDR and
    /// encodes 4:2:0. `ten_bit` without `hdr` is 10-bit SDR: NVENC widens from `Bgra`, AMF and
    /// QSV take `P010Sdr`. Media Foundation takes NV12 whatever was asked for — no vendor's
    /// MFT accepts P010, so an HDR request that reaches it encodes 8-bit rather than failing.
    ///
    /// `sdr_fp16` ([`SET_ENCODE_FLAG_SDR_FP16`]) means the surface is FP16 under an SDR
    /// transfer: every input then converts from it, and `None` is a backend that cannot read
    /// it (Media Foundation), which the open reports instead of refusing every frame.
    #[must_use]
    pub const fn choose(
        backend: u32,
        hdr: bool,
        ten_bit: bool,
        chroma444: bool,
        sdr_fp16: bool,
    ) -> Option<Self> {
        let wcg = sdr_fp16 && !hdr;
        Some(match (backend, hdr, chroma444) {
            (backend::PYROWAVE, _, _) => Self::Planar {
                hdr,
                wcg,
                chroma444,
            },
            (backend::MEDIA_FOUNDATION, _, _) if wcg => return None,
            (backend::MEDIA_FOUNDATION, _, _) => Self::Nv12,
            (backend::NVENC, false, _) if wcg => Self::Rgb10Wcg,
            (backend::AMF | backend::QSV, false, _) if wcg => Self::P010Wcg,
            (backend::NVENC, true, true) => Self::Rgb10,
            (backend::AMF, true, _) => Self::Fp16,
            (_, true, _) => Self::P010,
            (backend::NVENC, false, _) => Self::Bgra,
            (backend::AMF | backend::QSV, false, _) if ten_bit => Self::P010Sdr,
            (backend::AMF, false, _) => Self::Bgra,
            _ if wcg => return None,
            _ => Self::Nv12,
        })
    }

    /// What `backend` opens with after it refused `self`. Only AMF's RGB inputs have a
    /// second choice: an encoder that declines one still encodes the YUV converted for it.
    #[must_use]
    pub const fn fallback(self, backend: u32) -> Option<Self> {
        match (backend, self) {
            (backend::AMF, Self::Bgra) => Some(Self::Nv12),
            (backend::AMF, Self::Fp16) => Some(Self::P010),
            _ => None,
        }
    }

    /// The backend reads the format the display composes, so no converter runs and the
    /// acquired surface itself can be the encoder's input ([`zero_copy`]).
    #[must_use]
    pub const fn composed(self) -> bool {
        matches!(self, Self::Bgra | Self::Fp16)
    }

    /// Full chroma reaches the backend: packed RGB, or planes built at full resolution.
    #[must_use]
    pub const fn full_chroma(self) -> bool {
        matches!(
            self,
            Self::Bgra
                | Self::Rgb10
                | Self::Rgb10Wcg
                | Self::Fp16
                | Self::Planar {
                    chroma444: true,
                    ..
                }
        )
    }
}
/// `pf_encode_core::EncoderCaps` as plain integers — the encoder crate depends on this one,
/// not the reverse. Each `bool` there is `0`/`1` here; the driver fills this from
/// `Encoder::caps()` after the open and the host maps it straight back, so the session routes
/// by query rather than by a `false` default.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct EncoderCapsWire {
    /// `supports_rfi`: `invalidate_ref_frames` can succeed; else the host keyframes on loss.
    pub supports_rfi: u32,
    /// `chroma_444`: the opened encoder emits 4:4:4. Cross-check against the request's `chroma`.
    pub chroma_444: u32,
    /// `intra_refresh`: a moving intra band instead of periodic IDRs.
    pub intra_refresh: u32,
    /// `intra_refresh_recovery`: the wave has a decoder-visible clean point, so freezes lift.
    pub intra_refresh_recovery: u32,
    /// `intra_refresh_period`: wave length in frames; `0` when the wave is off.
    pub intra_refresh_period: u32,
    /// `blends_cursor`: the encoder composited the pointer, so the host must not.
    pub blends_cursor: u32,
}

/// [`SetEncodeReply::status`]: an encoder is open.
pub const SET_ENCODE_OK: u32 = 0;
/// No arrived monitor has the request's `target_id`.
pub const SET_ENCODE_NO_MONITOR: u32 = 1;
/// The section did not pass [`au::au_readable`], or is smaller than its header claims.
pub const SET_ENCODE_BAD_SECTION: u32 = 2;
/// The monitor has no render device yet: no swap-chain has been assigned since arrival.
pub const SET_ENCODE_NO_DEVICE: u32 = 3;
/// Every backend in the list refused; `error` and `name` are the last one's.
pub const SET_ENCODE_NO_BACKEND: u32 = 4;
/// The pool or its converters could not be built on the render device.
pub const SET_ENCODE_POOL: u32 = 5;
/// The encode thread could not be started.
pub const SET_ENCODE_THREAD: u32 = 6;
/// The open did not finish within the driver's bound; the thread was abandoned.
pub const SET_ENCODE_TIMEOUT: u32 = 7;

/// [`IOCTL_SET_ENCODE`] output. `status` is the driver's own failure domain, not an HRESULT:
/// [`SET_ENCODE_OK`] means an encoder is open, anything else means none is and the session
/// ends with a structured error — `error` carries the backend's raw code and `name` a short
/// NUL-padded tag (the driver log has the rest). `backend_opened` names the entry from
/// [`SetEncodeRequest::backends`] that took, never a guess.
///
/// The IOCTL itself completes successfully whenever the request was well-formed and named a
/// monitor; from that point the driver owns the two handles and closes them itself when
/// `status` is non-zero. Only an NTSTATUS failure leaves them for the host to reap.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct SetEncodeReply {
    /// One of the `SET_ENCODE_*` codes.
    pub status: u32,
    /// The [`backend`] id that opened; `0` when `status` is non-zero.
    pub backend_opened: u32,
    /// What the opened backend can actually do.
    pub caps: EncoderCapsWire,
    /// Bitrate the backend accepted — may differ from what was asked.
    pub applied_bitrate_kbps: u32,
    /// Raw backend code for a non-zero `status` (HRESULT, NVENC status, AMF result).
    pub error: i32,
    /// Short NUL-padded tag: the backend name on success, the failing stage otherwise.
    pub name: [u8; 32],
}

/// [`EncodeCtlRequest::op`]: force the next submitted frame to an IDR.
pub const ENCODE_CTL_REQUEST_KEYFRAME: u32 = 1;
/// Invalidate reference frames `arg0..=arg1` in the host's wire-index domain (RFI).
pub const ENCODE_CTL_INVALIDATE_REF_FRAMES: u32 = 2;
/// Distrust every reference; the next AU is a clean recovery anchor.
pub const ENCODE_CTL_DISTRUST_REFERENCES: u32 = 3;
/// Reconfigure the bitrate to `arg0` kbps without reopening the backend.
pub const ENCODE_CTL_RECONFIGURE_BITRATE: u32 = 4;
/// Replace the HDR mastering metadata from `payload` (28 `pf_frame::HdrMeta` bytes).
pub const ENCODE_CTL_SET_HDR_META: u32 = 5;
/// Detach the wedged encode thread, open a fresh encoder, restart `wire_seq` at `arg0`.
pub const ENCODE_CTL_RESET: u32 = 6;
/// Push the encoder's in-flight AUs into the section.
pub const ENCODE_CTL_FLUSH: u32 = 7;
/// Stop the session whose `generation` is `arg0` — the host's proxy going away. A
/// generation that is not the live one is a stale proxy and a no-op, so a dropped
/// predecessor never stops its successor.
pub const ENCODE_CTL_CLOSE: u32 = 8;
/// Reference only frames the client confirmed: the newest at wire index `arg0`, the sixteen
/// before it in `arg1 >> 1`, while `arg1 & 1`; `arg1` 0 restores the chain. A driver without
/// it answers `STATUS_INVALID_PARAMETER`.
pub const ENCODE_CTL_SET_REFERENCE_FLOOR: u32 = 9;
/// What the client confirmed it decoded clean, in [`ENCODE_CTL_SET_REFERENCE_FLOOR`]'s
/// encoding, whatever the link: the backend keeps the newest confirmed picture resident as the
/// next loss's anchor. A driver without it answers `STATUS_INVALID_PARAMETER`.
pub const ENCODE_CTL_NOTE_CONFIRMED: u32 = 10;

/// `SET_ENCODE` completed with fewer reply bytes than [`SetEncodeReply`]. The IOCTL itself
/// succeeded, so the driver adopted the host's handles: the host must not close them too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyTooShort {
    pub got: usize,
    pub want: usize,
}

impl core::fmt::Display for ReplyTooShort {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "SET_ENCODE: short reply ({} of {} bytes) — driver predates proto v7",
            self.got, self.want
        )
    }
}

impl core::error::Error for ReplyTooShort {}

/// Whether a session's encoder reads the acquired surface itself instead of a copy of it.
/// Only a `composed` input ([`EncodeInput::composed`]) at the surface's own size can: every
/// other kind needs its converter, and a [`Framing`] that scales needs the scale pass. On by default for AMF, QSV and NVENC: the copy runs on the 3D engine a game
/// renders on, and on an RTX 4090 at 4K120 under full load it held the frame 2.7 ms longer
/// before the encoder saw it. `knob` is `PFVD_POOL_BYPASS`: `0` turns it off, any other
/// value turns it on for every backend.
///
/// The driver's pool is Windows-only; the rule lives here so it is covered everywhere.
#[must_use]
pub fn zero_copy(backend: u32, composed: bool, knob: Option<&str>) -> bool {
    composed
        && match knob.map(str::trim) {
            Some("0") => false,
            Some(_) => true,
            None => matches!(backend, backend::AMF | backend::QSV | backend::NVENC),
        }
}

/// [`IOCTL_ENCODE_CTL`] input: one op against one monitor's live encoder. Unused `arg*` /
/// `payload` bytes are zero. The ops are the `Encoder` trait calls the stream loop already
/// makes locally on Linux, forwarded by a control proxy — so the wire shape is deliberately
/// flat. A command ring with a doorbell is the upgrade path if measured IOCTL latency hurts
/// RFI recovery; not before.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct EncodeCtlRequest {
    /// OS target id of the monitor whose encoder this addresses.
    pub target_id: u32,
    /// One `ENCODE_CTL_*` op.
    pub op: u32,
    /// First argument: reference-range start, bitrate kbps, or new `wire_seq_base`.
    pub arg0: u32,
    /// Second argument: reference-range end (inclusive) for
    /// [`ENCODE_CTL_INVALIDATE_REF_FRAMES`].
    pub arg1: u32,
    /// [`ENCODE_CTL_SET_HDR_META`] payload: 28 `pf_frame::HdrMeta` bytes.
    pub payload: [u8; 28],
}

// Same reason as the ring's asserts: the IOCTL buffers are raw bytes on the far side.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(size_of::<SetEncodeRequest>() == 168);
    assert!(offset_of!(SetEncodeRequest, encode_width) == SET_ENCODE_REQUEST_V9_SIZE);
    assert!(offset_of!(SetEncodeRequest, src_rect) == 152);
    assert!(offset_of!(SetEncodeRequest, target_id) == 0);
    assert!(offset_of!(SetEncodeRequest, section) == 8);
    assert!(offset_of!(SetEncodeRequest, event) == 16);
    assert!(offset_of!(SetEncodeRequest, hdr_meta) == 60);
    assert!(offset_of!(SetEncodeRequest, backends) == 96);
    assert!(offset_of!(SetEncodeRequest, flags) == 112);
    assert!(offset_of!(SetEncodeRequest, knobs) == SET_ENCODE_REQUEST_LEGACY_SIZE);
    assert!(size_of::<EncodeKnobs>() == 24);
    assert!(offset_of!(EncodeKnobs, split_encode) == 6);
    assert!(offset_of!(EncodeKnobs, nvenc_preset) == 20);
    assert!(offset_of!(EncodeKnobs, _reserved) == 21);

    assert!(size_of::<EncoderCapsWire>() == 24);
    assert!(size_of::<SetEncodeReply>() == 72);
    assert!(offset_of!(SetEncodeReply, caps) == 8);
    assert!(offset_of!(SetEncodeReply, applied_bitrate_kbps) == 32);
    assert!(offset_of!(SetEncodeReply, error) == 36);
    assert!(offset_of!(SetEncodeReply, name) == 40);

    assert!(size_of::<EncodeCtlRequest>() == 44);
    assert!(offset_of!(EncodeCtlRequest, op) == 4);
    assert!(offset_of!(EncodeCtlRequest, arg0) == 8);
    assert!(offset_of!(EncodeCtlRequest, arg1) == 12);
    assert!(offset_of!(EncodeCtlRequest, payload) == 16);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{control, ctl_code};

    /// The wire numbering, which the host writes and the driver dispatches on. A silent
    /// renumber swaps one encoder for another on a shipped driver, so pin it here.
    #[test]
    fn backend_and_codec_ids_are_the_wire_numbering() {
        use super::{backend as be, codec as cc};
        assert_eq!(
            [
                be::NVENC,
                be::AMF,
                be::QSV,
                be::PYROWAVE,
                be::MEDIA_FOUNDATION
            ],
            [1, 2, 3, 4, 5]
        );
        assert_eq!([cc::H264, cc::HEVC, cc::AV1, cc::PYROWAVE], [1, 2, 3, 4]);
        // The driver indexes NAMES by `id - 1` and replies with the entry it found.
        assert_eq!(be::name(be::MEDIA_FOUNDATION), Some("mf"));
        assert_eq!(be::NAMES[be::NVENC as usize - 1], "nvenc");
        assert_eq!(cc::name(cc::AV1), Some("av1"));
        assert_eq!(be::name(0), None, "0 terminates a list, it names nothing");
        assert_eq!(be::name(6), None);
    }

    /// `listed` guards a 0-terminated preference list; `valid` guards a single required field.
    /// Reading either as the other would let a `SET_ENCODE` through with no codec at all.
    #[test]
    fn a_terminator_is_listable_but_never_a_valid_codec() {
        use super::{backend as be, codec as cc};
        assert!(be::listed(0), "the list terminator");
        assert!(be::listed(be::MEDIA_FOUNDATION), "the widest id");
        assert!(!be::listed(6));
        assert!(!cc::valid(0), "a codec field is required");
        assert!(cc::valid(cc::PYROWAVE));
        assert!(!cc::valid(5));
    }

    #[test]
    fn frame_token_roundtrips() {
        for (g, s, slot) in [
            (1u32, 0u32, 0u8),
            (5, 12_345, 3),
            (FrameToken::GENERATION_MASK, 0xFFFF_FFFF, 5),
            (0, 1, 255),
        ] {
            let t = FrameToken {
                generation: g,
                seq: s,
                slot,
            };
            assert_eq!(FrameToken::unpack(t.pack()), t);
        }
    }

    #[test]
    fn frame_token_packing_matches_legacy_layout() {
        // Packing was `(gen<<40)|(seq<<8)|slot` by hand; lock the bit positions.
        let t = FrameToken {
            generation: 7,
            seq: 42,
            slot: 3,
        };
        assert_eq!(t.pack(), (7u64 << 40) | (42u64 << 8) | 3u64);
    }

    #[test]
    fn encode_ioctls_follow_the_probe_pair() {
        println!(
            "SET_ENCODE {:#x}, ENCODE_CTL {:#x} (after probe {:#x})",
            IOCTL_SET_ENCODE,
            IOCTL_ENCODE_CTL,
            control::IOCTL_ENCODE_PROBE_STATUS,
        );
        assert_eq!(IOCTL_SET_ENCODE, ctl_code(0x90C));
        assert_eq!(IOCTL_ENCODE_CTL, ctl_code(0x90D));
        // METHOD_BUFFERED on FILE_DEVICE_UNKNOWN, like every other op in the space.
        assert_eq!(IOCTL_SET_ENCODE & 0b11, 0);
        assert_eq!(IOCTL_SET_ENCODE >> 16, 0x22);
    }

    #[test]
    fn set_encode_request_layout_is_pinned() {
        use core::mem::{offset_of, size_of};

        println!(
            "SetEncodeRequest {} bytes: target_id@{} section@{} event@{} hdr_meta@{} \
             backends@{} flags@{} slices@{}",
            size_of::<SetEncodeRequest>(),
            offset_of!(SetEncodeRequest, target_id),
            offset_of!(SetEncodeRequest, section),
            offset_of!(SetEncodeRequest, event),
            offset_of!(SetEncodeRequest, hdr_meta),
            offset_of!(SetEncodeRequest, backends),
            offset_of!(SetEncodeRequest, flags),
            offset_of!(SetEncodeRequest, slices),
        );
        assert_eq!(size_of::<SetEncodeRequest>(), 168);
        assert_eq!(
            offset_of!(SetEncodeRequest, knobs),
            SET_ENCODE_REQUEST_LEGACY_SIZE
        );
        // v10's tail: everything a v9 driver reads ends where it starts.
        assert_eq!(
            offset_of!(SetEncodeRequest, encode_width),
            SET_ENCODE_REQUEST_V9_SIZE
        );
        assert_eq!(offset_of!(SetEncodeRequest, encode_height), 148);
        assert_eq!(offset_of!(SetEncodeRequest, src_rect), 152);
        assert_eq!(offset_of!(SetEncodeRequest, target_id), 0);
        assert_eq!(offset_of!(SetEncodeRequest, section), 8);
        assert_eq!(offset_of!(SetEncodeRequest, event), 16);
        assert_eq!(offset_of!(SetEncodeRequest, hdr_meta), 60);
        assert_eq!(offset_of!(SetEncodeRequest, backends), 96);
        assert_eq!(offset_of!(SetEncodeRequest, flags), 112);
        // The old tail pad: a host that predates `slices` sends zero here.
        assert_eq!(offset_of!(SetEncodeRequest, slices), 116);
        // The HDR blob is exactly `pf_frame::HdrMeta`, which the driver copies through untouched.
        assert_eq!(size_of::<[u8; 28]>(), 28);
    }

    #[test]
    fn set_encode_reply_and_ctl_layouts_are_pinned() {
        use core::mem::{offset_of, size_of};

        println!(
            "SetEncodeReply {} bytes (caps {} bytes): caps@{} applied@{} error@{} name@{}",
            size_of::<SetEncodeReply>(),
            size_of::<EncoderCapsWire>(),
            offset_of!(SetEncodeReply, caps),
            offset_of!(SetEncodeReply, applied_bitrate_kbps),
            offset_of!(SetEncodeReply, error),
            offset_of!(SetEncodeReply, name),
        );
        assert_eq!(size_of::<EncoderCapsWire>(), 24);
        assert_eq!(size_of::<SetEncodeReply>(), 72);
        assert_eq!(offset_of!(SetEncodeReply, status), 0);
        assert_eq!(offset_of!(SetEncodeReply, backend_opened), 4);
        assert_eq!(offset_of!(SetEncodeReply, caps), 8);
        assert_eq!(offset_of!(SetEncodeReply, applied_bitrate_kbps), 32);
        assert_eq!(offset_of!(SetEncodeReply, error), 36);
        assert_eq!(offset_of!(SetEncodeReply, name), 40);

        println!(
            "EncodeCtlRequest {} bytes: op@{} arg0@{} arg1@{} payload@{}",
            size_of::<EncodeCtlRequest>(),
            offset_of!(EncodeCtlRequest, op),
            offset_of!(EncodeCtlRequest, arg0),
            offset_of!(EncodeCtlRequest, arg1),
            offset_of!(EncodeCtlRequest, payload),
        );
        assert_eq!(size_of::<EncodeCtlRequest>(), 44);
        assert_eq!(offset_of!(EncodeCtlRequest, target_id), 0);
        assert_eq!(offset_of!(EncodeCtlRequest, op), 4);
        assert_eq!(offset_of!(EncodeCtlRequest, arg0), 8);
        assert_eq!(offset_of!(EncodeCtlRequest, arg1), 12);
        assert_eq!(offset_of!(EncodeCtlRequest, payload), 16);
    }

    /// A `SET_ENCODE` reply may promise only the chroma its chosen input can carry. The two
    /// backends the host ever negotiates 4:4:4 for must therefore land on a full-chroma input at
    /// both depths: HDR once picked P010 here, so the encoder emitted 4:2:0 while the reply — and
    /// the client's Welcome — still said 4:4:4.
    #[test]
    fn a_444_request_picks_a_full_chroma_input() {
        use super::EncodeInput::{Bgra, Fp16, Nv12, P010Sdr, Planar, Rgb10, P010};

        // (backend, hdr, ten_bit, chroma444) -> input. HDR implies ten_bit; 10-bit SDR is
        // ten_bit without hdr.
        let table = [
            ((1, false, false, false), Bgra),
            ((1, false, true, false), Bgra), // NVENC widens SDR-10 from BGRA itself
            ((1, false, false, true), Bgra),
            ((1, true, true, false), P010),
            ((1, true, true, true), Rgb10),
            ((2, true, true, true), Fp16), // AMF HDR: VCN converts, and encodes 4:2:0
            ((2, true, true, false), Fp16),
            ((3, true, true, false), P010),
            ((2, false, true, false), P010Sdr), // AMF 10-bit SDR: BT.709 P010
            ((2, false, false, false), Bgra),   // AMF 8-bit SDR: VCN converts
            ((3, false, false, false), Nv12),   // QSV 8-bit SDR: its own CSC is studio range
            ((3, false, false, true), Nv12),
            ((3, false, true, true), P010Sdr), // QSV 10-bit SDR: BT.709 P010
            (
                (4, true, true, true),
                Planar {
                    hdr: true,
                    wcg: false,
                    chroma444: true,
                },
            ),
        ];
        for ((backend, hdr, ten_bit, chroma444), want) in table {
            let got = EncodeInput::choose(backend, hdr, ten_bit, chroma444, false);
            assert_eq!(
                got,
                Some(want),
                "backend {backend} hdr {hdr} 10bit {ten_bit} 444 {chroma444}"
            );
        }
        for backend in [1, 4] {
            for (hdr, sdr_fp16) in [(false, false), (true, false), (false, true)] {
                assert!(
                    EncodeInput::choose(backend, hdr, true, true, sdr_fp16)
                        .is_some_and(EncodeInput::full_chroma),
                    "backend {backend} hdr {hdr} fp16 {sdr_fp16} asked 4:4:4 and got a \
                     subsampled input"
                );
            }
        }
    }

    /// An FP16 SDR desktop must reach every backend through a converter that reads FP16, never
    /// a BGRA input the pool would refuse frame by frame, and HDR still wins over the flag.
    #[test]
    fn an_fp16_sdr_desktop_picks_an_fp16_input_or_none() {
        use super::EncodeInput::{P010Wcg, Planar, Rgb10, Rgb10Wcg, P010};
        let fp16 =
            |backend, hdr, chroma444| EncodeInput::choose(backend, hdr, true, chroma444, true);
        assert_eq!(fp16(1, false, false), Some(Rgb10Wcg));
        assert_eq!(fp16(1, false, true), Some(Rgb10Wcg));
        assert_eq!(fp16(2, false, false), Some(P010Wcg));
        assert_eq!(fp16(3, false, false), Some(P010Wcg));
        assert_eq!(
            fp16(4, false, false),
            Some(Planar {
                hdr: false,
                wcg: true,
                chroma444: false
            })
        );
        assert_eq!(
            fp16(5, false, false),
            None,
            "Media Foundation reads no FP16"
        );
        assert_eq!(fp16(1, true, true), Some(Rgb10), "HDR wins over the flag");
        assert_eq!(fp16(3, true, false), Some(P010));
        for kind in [Rgb10Wcg, P010Wcg] {
            assert!(!kind.composed(), "{kind:?} needs its converter");
            assert_eq!(kind.fallback(2), None);
        }
    }

    /// Only AMF's RGB inputs have a second one: the YUV the converter kinds deliver. QSV never
    /// opens on BGRA now.
    #[test]
    fn only_amf_rgb_inputs_fall_back() {
        use super::EncodeInput::{Bgra, Fp16, Nv12, P010Sdr, P010};
        assert_eq!(Bgra.fallback(2), Some(Nv12));
        assert_eq!(Bgra.fallback(3), None);
        assert_eq!(P010.fallback(3), None);
        assert_eq!(Fp16.fallback(2), Some(P010));
        assert!(Bgra.composed() && Fp16.composed() && !P010.composed());
        assert_eq!(
            Bgra.fallback(1),
            None,
            "NVENC has no NV12 path in the driver"
        );
        for kind in [Nv12, P010, P010Sdr] {
            assert_eq!(kind.fallback(2), None, "{kind:?}");
        }
    }

    #[test]
    fn zero_copy_defaults_to_amf_qsv_and_nvenc() {
        use super::backend::{AMF, MEDIA_FOUNDATION, NVENC, QSV};
        assert!(zero_copy(AMF, true, None));
        assert!(zero_copy(QSV, true, None));
        assert!(zero_copy(NVENC, true, None));
        assert!(
            !zero_copy(MEDIA_FOUNDATION, true, None),
            "unmeasured: the copy stays"
        );
        assert!(!zero_copy(AMF, true, Some("0")));
        assert!(zero_copy(MEDIA_FOUNDATION, true, Some("1")));
        assert!(!zero_copy(AMF, false, Some("1")), "a converter kind");
    }

    #[test]
    fn encode_ctl_ops_are_distinct() {
        let ops = [
            ENCODE_CTL_REQUEST_KEYFRAME,
            ENCODE_CTL_INVALIDATE_REF_FRAMES,
            ENCODE_CTL_DISTRUST_REFERENCES,
            ENCODE_CTL_RECONFIGURE_BITRATE,
            ENCODE_CTL_SET_HDR_META,
            ENCODE_CTL_RESET,
            ENCODE_CTL_FLUSH,
            ENCODE_CTL_CLOSE,
            ENCODE_CTL_SET_REFERENCE_FLOOR,
            ENCODE_CTL_NOTE_CONFIRMED,
        ];
        println!("ENCODE_CTL ops: {ops:?}");
        assert_eq!(ops, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        // `0` stays unassigned: a zeroed request is not a silent keyframe.
        assert!(!ops.contains(&0));
    }

    #[test]
    fn keyframe_republishes_the_stash_only_when_nothing_composed() {
        // Idle desktop, slot back in the free list: the request re-encodes the stash as an IDR.
        assert_eq!(republish_slot(Some(1), 0, &[1, 2]), Some(1));
        // A composed frame is queued — the ordinary path already produces the IDR.
        assert_eq!(republish_slot(Some(1), 1, &[1, 2]), None);
        // The slot is in use again (a drain pass or an AU still owed on it).
        assert_eq!(republish_slot(Some(1), 0, &[2]), None);
        // Nothing was ever encoded on this pool.
        assert_eq!(republish_slot(None, 0, &[1, 2]), None);
    }

    #[test]
    fn a_full_pool_recycles_the_oldest_frame_not_the_new_one() {
        // A free slot is always taken, whatever is queued behind it.
        assert_eq!(offer_slot(Some(2), Some(0), true), Some(OfferSlot::Free(2)));
        // No free slot: the oldest queued frame goes, and a consumer lost it.
        assert_eq!(
            offer_slot(None, Some(0), true),
            Some(OfferSlot::Recycle {
                slot: 0,
                lost: true
            })
        );
        // Between sessions nobody is reading, so the same recycle costs nothing.
        assert_eq!(
            offer_slot(None, Some(0), false),
            Some(OfferSlot::Recycle {
                slot: 0,
                lost: false
            })
        );
        // Every slot is out at the encoder: this frame has nowhere to land.
        assert_eq!(offer_slot(None, None, true), None);
    }

    #[test]
    fn au_publish_token_round_trips_every_slot() {
        use super::au::AU_SLOTS;

        for slot in 0..AU_SLOTS as u8 {
            let t = FrameToken {
                generation: 0x00AB_CDEF,
                seq: 0xDEAD_BEEF,
                slot,
            };
            let packed = t.pack();
            assert_eq!(FrameToken::unpack(packed), t, "slot {slot} did not survive");
        }
        let t = FrameToken {
            generation: 7,
            seq: 15,
            slot: 15,
        };
        println!("token {t:?} packs to {:#x}", t.pack());
        // 16 slots need 4 bits of the 8 the token carries: the table can double without a repack.
        assert_eq!(FrameToken::unpack(t.pack()).slot, 15);
    }

    #[test]
    fn set_encode_request_round_trips_through_bytes() {
        let mut req = SetEncodeRequest::zeroed();
        req.target_id = 0x1234;
        req.section = 0x0BAD_C0DE_0BAD_C0DE;
        req.event = 0xFEED_FACE_FEED_FACE;
        req.section_bytes = au::section_bytes(au::heap_bytes_for(80_000, 60));
        req.codec = 2;
        req.chroma = 1;
        req.bit_depth = 10;
        req.width = 3840;
        req.height = 2160;
        req.fps = 120;
        req.bitrate_kbps = 80_000;
        req.hdr = 1;
        req.hdr_meta[27] = 0xAB;
        req.wire_chunk_bytes = 0;
        req.wire_seq_base = 0x7FFF_FFFF;
        req.backends = [1, 2, 0, 0];

        req.knobs.apply_env("PUNKTFUNK_NVENC_SLICES", "4");
        (req.encode_width, req.encode_height) = (1920, 1080);

        let bytes = bytemuck::bytes_of(&req);
        println!("SetEncodeRequest on the wire: {} bytes", bytes.len());
        assert_eq!(bytes.len(), 168);
        let back: SetEncodeRequest = bytemuck::pod_read_unaligned(bytes);
        assert_eq!(back, req);
        // A pre-knobs driver reads the first 120 bytes and sees the same request minus knobs;
        // a pre-knobs host's 120 bytes zero-fill to every backend's default.
        let mut prefix = [0u8; 168];
        prefix[..SET_ENCODE_REQUEST_LEGACY_SIZE]
            .copy_from_slice(&bytes[..SET_ENCODE_REQUEST_LEGACY_SIZE]);
        let old: SetEncodeRequest = bytemuck::pod_read_unaligned(&prefix);
        assert_eq!(old.backends, req.backends);
        assert_eq!(old.knobs, EncodeKnobs::default());
        // A v9 host's 144 bytes zero-fill to no framing: the surface at its own size.
        let mut v9 = [0u8; 168];
        v9[..SET_ENCODE_REQUEST_V9_SIZE].copy_from_slice(&bytes[..SET_ENCODE_REQUEST_V9_SIZE]);
        let v9: SetEncodeRequest = bytemuck::pod_read_unaligned(&v9);
        assert_eq!(v9.knobs, req.knobs);
        assert!(!v9.framing().is_some_and(|f| f.scales()));
        // The two handle values sit where the driver reads them, byte for byte.
        assert_eq!(
            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            req.section
        );
        assert_eq!(
            u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            req.event
        );
        // A zeroed request is inert: no target, no handles, no backend.
        let zero = SetEncodeRequest::zeroed();
        assert_eq!(zero.backends, [0; 4]);
        assert_eq!(bytemuck::bytes_of(&zero), [0u8; 168]);
    }

    /// The tail a v10 host appends: zero is the surface at its own size, an encode size alone
    /// scales the whole surface, and a rectangle that leaves the surface is refused.
    #[test]
    fn a_request_resolves_its_framing() {
        let req = |encode: (u32, u32), rect: [u32; 4]| SetEncodeRequest {
            width: 3840,
            height: 2160,
            encode_width: encode.0,
            encode_height: encode.1,
            src_rect: rect,
            ..SetEncodeRequest::zeroed()
        };
        let plain = req((0, 0), [0; 4]).framing().expect("plain");
        assert_eq!(plain.encode, (3840, 2160));
        assert!(!plain.scales());
        let scaled = req((1920, 1080), [0; 4]).framing().expect("scaled");
        assert_eq!(scaled.crop, [0, 0, 3840, 2160]);
        assert_eq!(scaled.encode, (1920, 1080));
        assert!(scaled.scales());
        let crop = req((0, 0), [0, 0, 1920, 1080]).framing().expect("crop");
        assert_eq!(crop.encode, (1920, 1080), "a crop alone keeps its own size");
        assert!(crop.scales());
        // Naming the whole surface is no framing.
        let whole = req((3840, 2160), [0, 0, 3840, 2160])
            .framing()
            .expect("whole");
        assert!(!whole.scales());
        for (encode, rect) in [
            ((1920, 0), [0; 4]),
            ((0, 0), [1, 0, 3840, 2160]),
            ((0, 0), [0, 0, 0, 1080]),
            ((0, 0), [u32::MAX, 0, 2, 2]),
        ] {
            assert_eq!(req(encode, rect).framing(), None, "{encode:?} {rect:?}");
        }
    }

    #[test]
    fn encode_knobs_parse_their_env_spellings() {
        let mut k = EncodeKnobs::default();
        assert!(!k.apply_env("PUNKTFUNK_NOT_A_KNOB", "1"));
        for name in EncodeKnobs::ENV_NAMES {
            assert!(k.apply_env(name, ""), "{name} must be a knob name");
        }
        assert_eq!(
            k,
            EncodeKnobs::default(),
            "empty values leave every default"
        );

        assert!(k.apply_env("PUNKTFUNK_SPLIT_ENCODE", "disable"));
        assert_eq!(k.split_encode, 1);
        k.apply_env("PUNKTFUNK_SPLIT_ENCODE", "3");
        assert_eq!(k.split_encode, 4);
        k.apply_env("PUNKTFUNK_SPLIT_ENCODE", "garbage");
        assert_eq!(k.split_encode, 4, "garbage keeps the last value");
        k.apply_env("PUNKTFUNK_NVENC_ASYNC", " yes ");
        assert_eq!(k.nvenc_async, 1);
        k.apply_env("PUNKTFUNK_NVENC_ASYNC", "Off");
        assert_eq!(k.nvenc_async, 2, "a falsy value vetoes pipelining");
        k.apply_env("PUNKTFUNK_NVENC_ASYNC", "maybe");
        assert_eq!(k.nvenc_async, 0);
        k.apply_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        assert_eq!(k.nvenc_subframe, 1);
        k.apply_env("PUNKTFUNK_NVENC_PRESET", "P4");
        assert_eq!(k.nvenc_preset, 4);
        k.apply_env("PUNKTFUNK_NVENC_PRESET", "8");
        assert_eq!(k.nvenc_preset, 4, "past P7 is ignored");
        k.apply_env("PUNKTFUNK_NVENC_SLICES", "33");
        assert_eq!(k.nvenc_slices, 0, "out of range is ignored");
        k.apply_env("PUNKTFUNK_INTRA_REFRESH", "0");
        assert_eq!(k.intra_refresh, 2);
        k.apply_env("PUNKTFUNK_INTRA_REFRESH", "on");
        assert_eq!(k.intra_refresh, 1);
        k.apply_env("PUNKTFUNK_INTRA_REFRESH", "maybe");
        assert_eq!(k.intra_refresh, 0);
        k.apply_env("PUNKTFUNK_IR_PERIOD_FRAMES", "1");
        assert_eq!(k.ir_period_frames, 0, "a one-frame wave is not a wave");
        k.apply_env("PUNKTFUNK_IR_PERIOD_FRAMES", "60");
        assert_eq!(k.ir_period_frames, 60);
        k.apply_env("PUNKTFUNK_AMF_USAGE", "highquality");
        assert_eq!(k.amf_usage, 4);
        k.apply_env("PUNKTFUNK_VBV_FRAMES", "1.5");
        assert_eq!(k.vbv_tenths, 15);
        assert_eq!(k.vbv_frames(), 1.5);
        k.apply_env("PUNKTFUNK_VBV_FRAMES", "-1");
        assert_eq!(k.vbv_tenths, 15);
        k.apply_env("PUNKTFUNK_PYROWAVE_CHUNK_KIB", "4");
        assert_eq!(k.pyrowave_chunk_64kib, 1, "the floor rounds up to one step");
        k.apply_env("PUNKTFUNK_PYROWAVE_CHUNK_KIB", "8192");
        assert_eq!(k.pyrowave_chunk_64kib, 128);
        assert!(truthy("TRUE") && truthy(" 1") && !truthy("2") && !truthy(""));
    }

    #[test]
    fn a_timed_out_hold_is_settled_by_its_own_release() {
        let mut h = BypassHold::default();
        assert!(h.may_hold());
        // Frame N was held and taken; the hold timed out and the worker took N back.
        h.timed_out();
        assert!(!h.may_hold(), "no new hold while a release is owed");
        // N's release lands after N+1 would have been held: it frees nothing.
        assert!(!h.released(), "the owed release settles the debt");
        assert!(h.may_hold());
        assert!(h.released(), "a release with nothing owed frees the hold");
        h.timed_out();
        h.clear();
        assert!(h.may_hold(), "a detach forgets the debt");
        assert!(h.released());
    }
}
