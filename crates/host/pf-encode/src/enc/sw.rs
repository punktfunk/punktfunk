//! Software H.264 encoder (openh264): the GPU-less path, Linux only. A `software` pin of
//! `PUNKTFUNK_ENCODER` or `open_software_h264` opens it; `auto` never does. Low-latency
//! screen-content config: single-reference, Baseline (no B-frames), bitrate RC, in-band
//! SPS/PPS each IDR. `submit` encodes immediately and stashes the AU for `poll`.
//!
//! RGB→YUV is ours, BT.709 at the session's range (full, or studio where the
//! client asked). The crate `YUVBuffer` converter is BT.601; decoded-as-709 that
//! is a constant hue error, so it is not used. [`VuiConfig::bt709`] is written in
//! `open`: vendor TV decoders guess colorimetry from resolution when VUI is
//! unspecified (4K SDR as BT.2020).
//!
//! Pin the VUI bits with `sps_signals_bt709_at_the_session_range`. Conversion
//! anchors live in the tests below.

use super::{EncodedFrame, Encoder};
use crate::cpu_ingest::{chroma709, luma709};
use crate::env_knobs::Knobs;
use anyhow::{bail, ensure, Context, Result};
use openh264::encoder::{
    BitRate, Complexity, Encoder as Oh264, EncoderConfig, FrameRate, FrameType, IntraFramePeriod,
    Profile, RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;
use pf_frame::{CapturedFrame, FramePayload, PixelFormat};
use std::collections::VecDeque;

pub struct OpenH264Encoder {
    enc: Oh264,
    width: u32,
    height: u32,
    fps: u32,
    src_format: PixelFormat,
    /// The range the planes are written at and the SPS signals; `false` is studio.
    full_range: bool,
    /// Reused I420 planes (BT.709 CSC; see the module doc).
    y_plane: Vec<u8>,
    u_plane: Vec<u8>,
    v_plane: Vec<u8>,
    frame_idx: i64,
    force_kf: bool,
    /// FIFO of AUs for `poll`. A single-slot `Option` would overwrite when two
    /// submits precede a poll and skew pts pairing (opening IDR).
    pending: VecDeque<EncodedFrame>,
}

// SAFETY: `Oh264` holds a raw `ISVCEncoder` handle and is not auto-`Send`;
// the other fields (plane `Vec`s, scalars, `pending`) are owned. The session
// creates this value, calls `submit`/`poll`/`flush`, and drops it on one
// encode thread, never sharing it by reference, so the handle is only ever
// touched from that thread.
unsafe impl Send for OpenH264Encoder {}

/// openh264 level 5.2: long edge ≤ 3840, short edge ≤ 2160. Orientation-aware,
/// not `w <= 3840 && h <= 2160` — portrait 2160×3840 is legal.
const OPENH264_MAX_LONG_EDGE: u32 = 3840;
const OPENH264_MAX_SHORT_EDGE: u32 = 2160;

/// Bundled openh264's `reinit` check, which runs on the first encode, not at
/// construction. Without this, a too-large mode opens and then fails every
/// `submit`. `Codec::max_dimension` is 4096 for H.264 hardware; this ceiling
/// is software-only.
fn openh264_supports_dimensions(width: u32, height: u32) -> bool {
    width.max(height) <= OPENH264_MAX_LONG_EDGE && width.min(height) <= OPENH264_MAX_SHORT_EDGE
}

impl OpenH264Encoder {
    /// A session writing and signalling BT.709 at `full_range` (`false` is studio).
    pub fn open(
        format: PixelFormat,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: u64,
        full_range: bool,
        knobs: &Knobs,
    ) -> Result<Self> {
        // `validate_dimensions` already passed (even, ≤ 4096). Refuse here, at
        // `open`, the 4096-wide modes this encoder cannot serve.
        ensure!(
            openh264_supports_dimensions(width, height),
            "openh264 cannot encode {width}x{height}: the software encoder tops out at \
             {OPENH264_MAX_LONG_EDGE}x{OPENH264_MAX_SHORT_EDGE} (or \
             {OPENH264_MAX_SHORT_EDGE}x{OPENH264_MAX_LONG_EDGE} portrait) — lower the client \
             resolution, or use a host with a hardware encoder"
        );
        let bps: u32 = bitrate_bps.try_into().unwrap_or(u32::MAX);
        let cfg = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .max_frame_rate(FrameRate::from_hz(fps.max(1) as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(bps))
            .skip_frames(false)
            .intra_frame_period(IntraFramePeriod::from_num_frames(intra_period_frames(
                fps,
                knobs.oh264_gop,
            )))
            .sps_pps_strategy(SpsPpsStrategy::ConstantId) // SPS/PPS in-band on every IDR
            .num_threads(knobs.oh264_threads.unwrap_or(2)) // latency over throughput
            .scene_change_detect(false) // no surprise IDRs (bitrate spikes / freeze)
            .adaptive_quantization(true)
            .complexity(Complexity::Low) // latency over BD-rate
            .profile(Profile::Baseline) // no B-frames
            .vui(VuiConfig::bt709().full_range(full_range));
        let api = OpenH264API::from_source(); // statically-bundled build (default `source` feature)
        let enc = Oh264::with_api_config(api, cfg).context("openh264 Encoder::with_api_config")?;
        let (w, h) = (width as usize, height as usize);
        tracing::info!(
            "openh264 software encoder: {width}x{height}@{fps} {} Mbps (Baseline, screen-content)",
            bps / 1_000_000
        );
        Ok(Self {
            enc,
            width,
            height,
            fps,
            src_format: format,
            full_range,
            y_plane: vec![0; w * h],
            u_plane: vec![0; (w / 2) * (h / 2)],
            v_plane: vec![0; (w / 2) * (h / 2)],
            frame_idx: 0,
            force_kf: false,
            pending: VecDeque::new(),
        })
    }

    /// Packed full-range RGB → I420, BT.709 at the session's range. Luma per pixel;
    /// Cb/Cr from the 2×2 average (same box filter as the crate converter; only the
    /// matrix changed).
    fn convert_bt709(&mut self, src: &[u8], bpp: usize, ri: usize, gi: usize, bi: usize) {
        let w = self.width as usize;
        let h = self.height as usize;
        let cw = w / 2;
        for by in 0..h / 2 {
            for bx in 0..cw {
                let mut sum = (0f32, 0f32, 0f32);
                for (dy, dx) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                    let (px, py) = (bx * 2 + dx, by * 2 + dy);
                    let s = &src[(py * w + px) * bpp..];
                    let (r, g, b) = (f32::from(s[ri]), f32::from(s[gi]), f32::from(s[bi]));
                    self.y_plane[py * w + px] = luma709(r, g, b, self.full_range);
                    sum = (sum.0 + r, sum.1 + g, sum.2 + b);
                }
                let (cb, cr) = chroma709(sum.0 / 4.0, sum.1 / 4.0, sum.2 / 4.0, self.full_range);
                self.u_plane[by * cw + bx] = cb;
                self.v_plane[by * cw + bx] = cr;
            }
        }
    }
}

impl Encoder for OpenH264Encoder {
    fn submit(&mut self, captured: &CapturedFrame) -> Result<()> {
        ensure!(
            captured.width == self.width && captured.height == self.height,
            "captured {}x{} != encoder {}x{}",
            captured.width,
            captured.height,
            self.width,
            self.height
        );
        ensure!(
            captured.format == self.src_format,
            "captured format {:?} != encoder source {:?}",
            captured.format,
            self.src_format
        );
        let FramePayload::Cpu(bytes) = &captured.payload else {
            bail!("openh264 backend requires a CPU frame payload");
        };
        let w = self.width as usize;
        let h = self.height as usize;
        ensure!(
            bytes.len() >= w * h * self.src_format.bytes_per_pixel(),
            "captured buffer {} bytes too small for {w}x{h} {:?}",
            bytes.len(),
            self.src_format
        );

        // Packed-RGB layouts go straight to `convert_bt709`; no BGRA normalize pass.
        let (bpp, ri, gi, bi) = match self.src_format {
            PixelFormat::Rgb => (3, 0, 1, 2),
            PixelFormat::Bgr => (3, 2, 1, 0),
            PixelFormat::Rgba | PixelFormat::Rgbx => (4, 0, 1, 2),
            PixelFormat::Bgra | PixelFormat::Bgrx => (4, 2, 1, 0),
            // 10-bit is GPU-path only; this 8-bit encoder is never negotiated HDR/10-bit.
            PixelFormat::Rgb10a2
            | PixelFormat::Rgb10a2Sdr
            | PixelFormat::RgbaF16
            | PixelFormat::X2Rgb10
            | PixelFormat::X2Bgr10 => {
                anyhow::bail!(
                    "software H.264 encoder cannot encode 10-bit ({:?})",
                    self.src_format
                )
            }
            // NV12/P010/YUV444 are GPU outputs for NVENC; this path only sees CPU RGB.
            PixelFormat::Nv12 | PixelFormat::P010 | PixelFormat::Yuv444 => {
                anyhow::bail!(
                    "software encoder cannot encode YUV GPU frames (NV12/P010/YUV444 → NVENC only)"
                )
            }
        };
        self.convert_bt709(bytes, bpp, ri, gi, bi);

        if self.force_kf {
            self.enc.force_intra_frame();
            self.force_kf = false;
        }
        let slices = YUVSlices::new(
            (&self.y_plane, &self.u_plane, &self.v_plane),
            (w, h),
            (w, w / 2, w / 2),
        );
        let bs = self.enc.encode(&slices).context("openh264 encode")?;
        let mut data = Vec::new();
        bs.write_vec(&mut data); // AnnexB start codes; SPS/PPS prepended on IDR
        if !data.is_empty() {
            let keyframe = matches!(bs.frame_type(), FrameType::IDR | FrameType::I);
            let pts_ns = self.frame_idx as u64 * 1_000_000_000 / self.fps.max(1) as u64;
            self.pending.push_back(EncodedFrame {
                data,
                pts_ns,
                keyframe,
                recovery_anchor: false,
                recovery_point: false,
                recovery_close: false,
                chunk_aligned: false,
            });
        }
        self.frame_idx += 1;
        Ok(())
    }

    fn request_keyframe(&mut self) {
        self.force_kf = true;
    }

    fn poll(&mut self) -> Result<Option<EncodedFrame>> {
        Ok(self.pending.pop_front())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(()) // synchronous: nothing buffered
    }
}

/// Rare automatic IDRs (recovery is `request_keyframe`; this encoder has no RFI). `gop`
/// (`PUNKTFUNK_OH264_GOP`) overrides (0 = encoder-auto).
fn intra_period_frames(fps: u32, gop: Option<u32>) -> u32 {
    gop.unwrap_or_else(|| fps.max(1).saturating_mul(600)) // ~10 min between automatic IDRs
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

    /// Red Cr = 255 because 255(1−Kr)/(2(1−Kr)) = 127.5 and `as u8` saturates. ±1 for
    /// float rounding.
    #[test]
    fn bt709_conversion_anchor_points() {
        assert_eq!(luma709(255.0, 255.0, 255.0, true), 255);
        assert_eq!(luma709(0.0, 0.0, 0.0, true), 0);
        assert_eq!(chroma709(255.0, 255.0, 255.0, true), (128, 128));
        assert_eq!(chroma709(0.0, 0.0, 0.0, true), (128, 128));
        let (cb, cr) = chroma709(255.0, 0.0, 0.0, true);
        assert_eq!(cr, 255, "pure red must reach the Cr extreme");
        assert!((98..=100).contains(&cb), "red Cb ~99, got {cb}");
        let (cb, _) = chroma709(0.0, 0.0, 255.0, true);
        assert_eq!(cb, 255, "pure blue must reach the Cb extreme");
    }

    /// Studio: red Cr = 240 because 255(1−Kr)·(224/255)/(2(1−Kr)) = 112.
    #[test]
    fn bt709_studio_anchor_points() {
        assert_eq!(luma709(255.0, 255.0, 255.0, false), 235);
        assert_eq!(luma709(0.0, 0.0, 0.0, false), 16);
        assert_eq!(chroma709(255.0, 255.0, 255.0, false), (128, 128));
        let (cb, cr) = chroma709(255.0, 0.0, 0.0, false);
        assert_eq!(cr, 240, "pure red must reach the studio Cr extreme");
        assert!((101..=103).contains(&cb), "red Cb ~102, got {cb}");
        let (cb, _) = chroma709(0.0, 0.0, 255.0, false);
        assert_eq!(cb, 240, "pure blue must reach the studio Cb extreme");
    }

    #[test]
    fn bt709_is_not_bt601() {
        // BT.601 green luma: 255·0.587 = 149.7; BT.709: 255·0.7152 = 182.4.
        let y = luma709(0.0, 255.0, 0.0, true);
        assert!((181..=183).contains(&y), "709 green luma ~182, got {y}");
    }

    /// Exercises the 2×2 block loop and plane sizing, not just per-pixel math.
    #[test]
    fn converts_flat_gray_to_neutral_planes() {
        let (w, h) = (16u32, 8u32);
        let mut enc = OpenH264Encoder::open(
            PixelFormat::Bgrx,
            w,
            h,
            60,
            1_000_000,
            true,
            &Knobs::from_env(),
        )
        .expect("open openh264");
        let bytes = vec![0x80u8; (w * h * 4) as usize];
        enc.convert_bt709(&bytes, 4, 2, 1, 0);
        // Grey stays grey at full range.
        assert!(
            enc.y_plane.iter().all(|&y| y == 128),
            "{:?}",
            &enc.y_plane[..4]
        );
        assert!(enc.u_plane.iter().all(|&u| u == 128));
        assert!(enc.v_plane.iter().all(|&v| v == 128));
    }

    #[test]
    fn encodes_synthetic_frame_to_annexb_idr() {
        let (w, h, fps) = (1280u32, 720u32, 60u32);
        let mut enc = OpenH264Encoder::open(
            PixelFormat::Bgrx,
            w,
            h,
            fps,
            8_000_000,
            true,
            &Knobs::from_env(),
        )
        .expect("open openh264");
        let frame = CapturedFrame {
            provenance: Default::default(),
            width: w,
            height: h,
            pts_ns: 0,
            format: PixelFormat::Bgrx,
            payload: FramePayload::Cpu(vec![0x80u8; (w * h * 4) as usize]),
            cursor: None,
        };
        enc.submit(&frame).expect("submit");
        let au = enc.poll().expect("poll").expect("an AU");
        assert!(au.keyframe, "first frame must be an IDR");
        assert!(
            au.data.starts_with(&[0, 0, 0, 1]) || au.data.starts_with(&[0, 0, 1]),
            "expected AnnexB start code"
        );
        let has_sps = au
            .data
            .windows(5)
            .any(|w| w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1 && (w[4] & 0x1f) == 7);
        assert!(has_sps, "IDR must carry an SPS NAL (type 7)");
    }

    fn sps_rbsp(au: &[u8]) -> Vec<u8> {
        let start = au
            .windows(5)
            .position(|w| w[..4] == [0, 0, 0, 1] && (w[4] & 0x1f) == 7)
            .map(|p| p + 5)
            .expect("an SPS NAL");
        let end = au[start..]
            .windows(4)
            .position(|w| w[..3] == [0, 0, 1] || w == [0, 0, 0, 1])
            .map_or(au.len(), |p| start + p);
        let mut rbsp = Vec::new();
        let nal = &au[start..end];
        let mut i = 0;
        while i < nal.len() {
            // 00 00 03 -> the 03 is an emulation-prevention byte, not payload.
            if i + 2 < nal.len() && nal[i] == 0 && nal[i + 1] == 0 && nal[i + 2] == 3 {
                rbsp.extend_from_slice(&[0, 0]);
                i += 3;
            } else {
                rbsp.push(nal[i]);
                i += 1;
            }
        }
        rbsp
    }

    /// SPS colour from ITU-T H.264 §7.3.2.1.1:
    /// `(video_full_range_flag, primaries, transfer, matrix)`. `None` if unsignalled.
    fn sps_colour(rbsp: &[u8]) -> Option<(u8, u8, u8, u8)> {
        // Exp-Golomb ue(v): count leading zeros, then read that many trailing bits.
        fn ue(u: &mut dyn FnMut(u32) -> u32) -> u32 {
            let mut lz = 0;
            while u(1) == 0 {
                lz += 1;
                assert!(lz < 32, "malformed Exp-Golomb");
            }
            if lz == 0 {
                0
            } else {
                (1 << lz) - 1 + u(lz)
            }
        }
        let mut pos = 0usize;
        let mut u = |bits: u32| -> u32 {
            let mut v = 0;
            for _ in 0..bits {
                v = (v << 1) | u32::from((rbsp[pos / 8] >> (7 - (pos % 8))) & 1);
                pos += 1;
            }
            v
        };
        let profile_idc = u(8);
        u(8); // constraint_set flags + reserved
        u(8); // level_idc
        ue(&mut u); // seq_parameter_set_id
        assert_eq!(
            profile_idc, 66,
            "this encoder is pinned to Baseline — a profile change adds the chroma_format_idc \
             block this walk deliberately omits"
        );
        ue(&mut u); // log2_max_frame_num_minus4
        let poc_type = ue(&mut u);
        match poc_type {
            0 => {
                ue(&mut u);
            } // log2_max_pic_order_cnt_lsb_minus4
            1 => panic!("pic_order_cnt_type 1 unhandled — openh264 emits 0 or 2"),
            _ => {}
        }
        ue(&mut u); // max_num_ref_frames
        u(1); // gaps_in_frame_num_value_allowed_flag
        ue(&mut u); // pic_width_in_mbs_minus1
        ue(&mut u); // pic_height_in_map_units_minus1
        if u(1) == 0 {
            u(1); // mb_adaptive_frame_field_flag
        }
        u(1); // direct_8x8_inference_flag
        if u(1) == 1 {
            for _ in 0..4 {
                ue(&mut u); // frame_crop_*_offset
            }
        }
        if u(1) == 0 {
            return None; // vui_parameters_present_flag
        }
        if u(1) == 1 {
            // aspect_ratio_info_present_flag
            if u(8) == 255 {
                u(16);
                u(16);
            }
        }
        if u(1) == 1 {
            u(1); // overscan_info_present_flag -> overscan_appropriate_flag
        }
        if u(1) == 0 {
            return None; // video_signal_type_present_flag
        }
        u(3); // video_format
        let full_range = u(1) as u8;
        if u(1) == 0 {
            return None; // colour_description_present_flag
        }
        Some((full_range, u(8) as u8, u(8) as u8, u(8) as u8))
    }

    /// `VuiConfig::bt709()` is a request to a C library; this asserts the
    /// emitted SPS actually carries BT.709 at the range the planes are written at.
    /// Unsignalled 4K SDR is guessed as BT.2020 by vendor TV decoders.
    #[test]
    fn sps_signals_bt709_at_the_session_range() {
        let (w, h, fps) = (1280u32, 720u32, 60u32);
        for full_range in [true, false] {
            let mut enc = OpenH264Encoder::open(
                PixelFormat::Bgrx,
                w,
                h,
                fps,
                8_000_000,
                full_range,
                &Knobs::from_env(),
            )
            .expect("open openh264");
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: 0,
                format: PixelFormat::Bgrx,
                payload: FramePayload::Cpu(vec![0x80u8; (w * h * 4) as usize]),
                cursor: None,
            };
            enc.submit(&frame).expect("submit");
            let au = enc.poll().expect("poll").expect("an AU");
            let colour = sps_colour(&sps_rbsp(&au.data)).expect(
                "the SPS must carry video_signal_type + colour_description — \
                 see EncoderConfig::vui in `open`",
            );
            // (video_full_range_flag, colour_primaries, transfer, matrix) — 1 = BT.709.
            assert_eq!(colour, (u8::from(full_range), 1, 1, 1), "full={full_range}");
        }
    }

    /// The stream through the client's planner: one AU in, one picture out.
    #[test]
    fn the_client_shows_each_picture_in_its_own_au() {
        let (w, h, fps) = (640u32, 480u32, 60u32);
        let mut enc = OpenH264Encoder::open(
            PixelFormat::Bgrx,
            w,
            h,
            fps,
            2_000_000,
            true,
            &Knobs::from_env(),
        )
        .expect("open openh264");
        let mut planner = pf_vaapi::H264Planner::new();
        for i in 0..20u64 {
            let frame = CapturedFrame {
                provenance: Default::default(),
                width: w,
                height: h,
                pts_ns: i * 16_666_667,
                format: PixelFormat::Bgrx,
                payload: FramePayload::Cpu(vec![(i * 8) as u8; (w * h * 4) as usize]),
                cursor: None,
            };
            enc.submit(&frame).expect("submit");
            let au = enc.poll().expect("poll").expect("an AU");
            let plan = planner.plan_au(&au.data).expect("plan");
            assert_eq!(
                plan.dpb.outputs,
                [plan.dpb.stored.expect("stored")],
                "AU {i}"
            );
        }
    }

    /// Portrait 2160×3840 is legal; `w <= 3840 && h <= 2160` would reject it.
    #[test]
    fn openh264_accepts_up_to_4k_in_either_orientation() {
        assert!(openh264_supports_dimensions(1920, 1080));
        assert!(openh264_supports_dimensions(3840, 2160));
        assert!(openh264_supports_dimensions(2160, 3840));
        assert!(openh264_supports_dimensions(1080, 1920));
    }

    #[test]
    fn openh264_rejects_modes_that_would_fail_on_first_submit() {
        // 4096-wide is legal H.264 and passes `Codec::max_dimension`, but exceeds the long edge.
        assert!(!openh264_supports_dimensions(4096, 2160));
        assert!(!openh264_supports_dimensions(2160, 4096));
        // Long edge OK, short edge not.
        assert!(!openh264_supports_dimensions(3840, 2400));
        assert!(!openh264_supports_dimensions(2400, 3840));
    }

    #[test]
    fn open_refuses_a_mode_openh264_cannot_encode() {
        // Match, not `expect_err`: `OpenH264Encoder` is not `Debug` (raw C handle).
        let err = match OpenH264Encoder::open(
            PixelFormat::Bgra,
            4096,
            2160,
            60,
            20_000_000,
            true,
            &Knobs::from_env(),
        ) {
            Ok(_) => panic!("4096x2160 exceeds openh264's long-edge ceiling and must be refused"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("openh264 cannot encode 4096x2160"), "{msg}");
    }
}
