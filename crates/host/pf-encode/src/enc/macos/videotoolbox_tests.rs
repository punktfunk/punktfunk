//! Live VideoToolbox tests: synthetic frames through the encoder, read back with the repo's
//! parsers. They need the media engine, so they are ignored; run them on a Mac with
//! `cargo test -p pf-encode --lib videotoolbox -- --ignored --test-threads=1`.

use super::*;
use crate::parameter_sets::parameter_set_prefix;
use objc2_core_video::kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange;
use pf_bitstream::{h264::H264Planner, h265::H265Planner};
use pf_frame::PixelBufferFrame;
use std::time::Instant;

/// What the parsers read off one AU.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Read {
    idr: bool,
    params: bool,
    full_range: bool,
    primaries: u8,
    transfer: u8,
    depth: u8,
    chroma_idc: u8,
}

/// Plans every AU of a stream in order.
enum Planner {
    H264(Box<H264Planner>),
    H265(Box<H265Planner>),
}

impl Planner {
    fn new(codec: Codec) -> Planner {
        match codec {
            Codec::H264 => Planner::H264(Box::default()),
            _ => Planner::H265(Box::default()),
        }
    }

    fn read(&mut self, codec: Codec, au: &[u8]) -> Read {
        let params = parameter_set_prefix(codec, au).is_some();
        match self {
            Planner::H264(p) => {
                let pic = p.plan_au(au).expect("H.264 AU plans").picture;
                Read {
                    idr: pic.is_idr,
                    params,
                    full_range: pic.colour.video_full_range,
                    primaries: pic.colour.colour_primaries,
                    transfer: pic.colour.transfer_characteristics,
                    depth: 8 + pic.bit_depth_luma_minus8,
                    chroma_idc: pic.chroma_format_idc,
                }
            }
            Planner::H265(p) => {
                let pic = p.plan_au(au).expect("HEVC AU plans").picture;
                Read {
                    idr: pic.is_idr,
                    params,
                    full_range: pic.colour.video_full_range,
                    primaries: pic.colour.colour_primaries,
                    transfer: pic.colour.transfer_characteristics,
                    depth: 8 + pic.bit_depth_luma_minus8,
                    chroma_idc: pic.chroma_format_idc,
                }
            }
        }
    }
}

fn reads(codec: Codec, aus: &[EncodedFrame]) -> Vec<Read> {
    let mut planner = Planner::new(codec);
    aus.iter().map(|au| planner.read(codec, &au.data)).collect()
}

fn params(codec: Codec, width: u32, height: u32, bitrate_bps: u64) -> OpenParams {
    OpenParams {
        codec,
        format: PixelFormat::Bgrx,
        width,
        height,
        fps: 60,
        bitrate_bps,
        cuda: false,
        bit_depth: 8,
        hdr: false,
        chroma: ChromaFormat::Yuv420,
        cursor_blend: false,
        slices: 1,
        full_range: false,
    }
}

fn frame(
    width: u32,
    height: u32,
    n: u32,
    format: PixelFormat,
    payload: FramePayload,
) -> CapturedFrame {
    CapturedFrame {
        width,
        height,
        pts_ns: u64::from(n) * 16_666_667,
        format,
        payload,
        cursor: None,
        provenance: Default::default(),
    }
}

/// A moving gradient with a bar, so every frame differs from the last.
fn cpu_frame(width: u32, height: u32, n: u32) -> CapturedFrame {
    let (w, h) = (width as usize, height as usize);
    let mut px = vec![0xFFu8; w * h * 4];
    for (i, p) in px.chunks_exact_mut(4).enumerate() {
        let (x, y) = (i % w, i / w);
        p[0] = (x * 255 / w) as u8;
        p[1] = (y * 255 / h) as u8;
        p[2] = if (x + n as usize * 8) % w < w / 8 {
            255
        } else {
            (n * 3) as u8
        };
    }
    frame(width, height, n, PixelFormat::Bgrx, FramePayload::Cpu(px))
}

/// Noise, the same picture scrolled `step` pixels each frame, inverted every 20 frames: detail
/// the rate control cannot meet, and hard cuts.
fn hard_frame(width: u32, height: u32, n: u32, step: usize) -> CapturedFrame {
    let row = width as usize * 4;
    let mut s = 0x9E37_79B9u32;
    let noise: Vec<u8> = (0..row * height as usize)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s as u8
        })
        .collect();
    let shift = (n as usize * step * 4) % row;
    let invert = if (n / 20) % 2 == 1 { 0xFF } else { 0 };
    let px = noise
        .chunks(row)
        .flat_map(|r| r[shift..].iter().chain(&r[..shift]).map(|b| b ^ invert))
        .collect();
    frame(width, height, n, PixelFormat::Bgrx, FramePayload::Cpu(px))
}

/// An IOSurface-backed 4:2:0 pixel buffer of `format` (8-bit NV12 or 10-bit P010), as capture
/// hands one over.
fn pixel_frame(width: u32, height: u32, n: u32, format: u32) -> CapturedFrame {
    let (w, h) = (width as usize, height as usize);
    let buffer = new_pixel_buffer(w, h, format).expect("pixel buffer");
    let ten = format == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange;
    let luma = |i: usize| ((i % w + n as usize * 8) % 256) as u16 / 2 + 40;
    let (y, uv) = if ten {
        let y: Vec<u8> = (0..w * h)
            .flat_map(|i| ((luma(i) << 2) << 6).to_le_bytes())
            .collect();
        (
            y,
            [(512u16 << 6).to_le_bytes(); 2].concat().repeat(w * h / 4),
        )
    } else {
        let y: Vec<u8> = (0..w * h).map(|i| luma(i) as u8).collect();
        (y, vec![128u8; w * h / 2])
    };
    let bpp = if ten { 2 } else { 1 };
    copy_into(&buffer, &[(&y, w * bpp, h), (&uv, w * bpp, h / 2)]).expect("fill");
    let fmt = if ten {
        PixelFormat::P010
    } else {
        PixelFormat::Nv12
    };
    frame(
        width,
        height,
        n,
        fmt,
        FramePayload::PixelBuffer(PixelBufferFrame(buffer)),
    )
}

/// Submit each frame, calling `before(i, enc)` first, and collect every AU in order. A dropped
/// or failed frame fails the test.
fn run(
    enc: &mut VtEncoder,
    frames: impl IntoIterator<Item = CapturedFrame>,
    mut before: impl FnMut(u32, &mut VtEncoder),
) -> Vec<EncodedFrame> {
    let mut aus = Vec::new();
    for (i, f) in frames.into_iter().enumerate() {
        before(i as u32, enc);
        enc.submit(&f).expect("submit");
        while let Some(au) = enc.poll().expect("poll") {
            aus.push(au);
        }
    }
    enc.flush().expect("flush");
    while let Some(au) = enc.poll().expect("poll") {
        aus.push(au);
    }
    aus
}

/// The AU carries an HEVC prefix SEI holding the mastering display volume.
fn has_mastering_sei(au: &[u8]) -> bool {
    au.windows(7).any(|w| {
        w == [
            0,
            0,
            0,
            1,
            39 << 1,
            1,
            pf_frame::hdr::SEI_TYPE_MASTERING_DISPLAY_COLOUR_VOLUME as u8,
        ]
    })
}

const CODECS: [Codec; 2] = [Codec::H264, Codec::H265];

#[test]
#[ignore = "needs the media engine"]
fn the_hardware_list_names_h264_and_hevc() {
    let found = hardware_codecs();
    assert!(found.h264 && found.h265, "{found:?}");
}

#[test]
#[ignore = "needs the media engine"]
fn the_first_unit_is_a_keyframe_with_parameter_sets_from_cpu_and_pixel_buffers() {
    for codec in CODECS {
        for pixel in [false, true] {
            let mut enc = VtEncoder::open(&params(codec, 1280, 720, 8_000_000)).unwrap();
            let frames = (0..30).map(|n| {
                if pixel {
                    pixel_frame(
                        1280,
                        720,
                        n,
                        kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
                    )
                } else {
                    cpu_frame(1280, 720, n)
                }
            });
            let aus = run(&mut enc, frames, |_, _| {});
            assert_eq!(aus.len(), 30, "{codec:?} pixel={pixel}: one AU per frame");
            let r = reads(codec, &aus);
            assert!(
                aus[0].keyframe && r[0].idr && r[0].params,
                "{codec:?}: {:?}",
                r[0]
            );
            for (i, (au, r)) in aus.iter().zip(&r).enumerate().skip(1) {
                assert!(
                    !au.keyframe && !r.idr && !r.params,
                    "{codec:?} AU {i}: {r:?}"
                );
            }
            assert!(
                aus.windows(2).all(|w| w[0].pts_ns < w[1].pts_ns),
                "AUs come back in order with their own times"
            );
        }
    }
}

#[test]
#[ignore = "needs the media engine"]
fn request_keyframe_produces_one_where_asked() {
    for codec in CODECS {
        let mut enc = VtEncoder::open(&params(codec, 1280, 720, 8_000_000)).unwrap();
        let aus = run(
            &mut enc,
            (0..40).map(|n| cpu_frame(1280, 720, n)),
            |i, e| {
                if i == 20 {
                    e.request_keyframe();
                }
            },
        );
        let keys: Vec<usize> = (0..aus.len()).filter(|&i| aus[i].keyframe).collect();
        assert_eq!(keys, [0, 20], "{codec:?}");
        let r = reads(codec, &aus);
        assert!(r[20].idr && r[20].params, "{codec:?}: {:?}", r[20]);
    }
}

/// Hard cuts and detail past the rate: one AU per frame, and no keyframe but the first. With
/// `RealTime` on, VideoToolbox dropped frames here.
#[test]
#[ignore = "needs the media engine"]
fn hard_content_drops_no_frame_and_brings_no_keyframe() {
    for codec in CODECS {
        let mut enc = VtEncoder::open(&params(codec, 1920, 1080, 10_000_000)).unwrap();
        let aus = run(
            &mut enc,
            (0..120).map(|n| hard_frame(1920, 1080, n, 16)),
            |_, _| {},
        );
        assert_eq!(aus.len(), 120, "{codec:?}");
        let keys = aus.iter().filter(|a| a.keyframe).count();
        assert_eq!(keys, 1, "{codec:?}: only the first AU is a keyframe");
    }
}

#[test]
#[ignore = "needs the media engine"]
fn a_bitrate_change_takes_effect_without_a_keyframe() {
    for codec in CODECS {
        let mut enc = VtEncoder::open(&params(codec, 1280, 720, 20_000_000)).unwrap();
        let mut accepted = false;
        // A slow scroll, which both codecs predict, so the rate decides the size.
        let aus = run(
            &mut enc,
            (0..240).map(|n| hard_frame(1280, 720, n % 20, 2)),
            |i, e| {
                if i == 120 {
                    accepted = e.reconfigure_bitrate(2_000_000);
                }
            },
        );
        assert!(accepted, "{codec:?}: retarget accepted");
        assert_eq!(enc.applied_bitrate_bps(), Some(2_000_000));
        assert_eq!(aus.iter().filter(|a| a.keyframe).count(), 1, "{codec:?}");
        let mean = |r: std::ops::Range<usize>| {
            aus[r.clone()].iter().map(|a| a.data.len()).sum::<usize>() / r.len()
        };
        let (high, low) = (mean(60..120), mean(180..240));
        println!("{codec:?}: mean AU {high} B at 20 Mb/s, {low} B at 2 Mb/s");
        assert!(low * 3 < high, "{codec:?}: {high} B → {low} B");
    }
}

/// YUV pixel buffers keep their own range; BGRA comes out studio whatever the session asks,
/// which `resolve/macos.rs` reports as `rgb_full_range`.
#[test]
#[ignore = "needs the media engine"]
fn the_stream_signals_the_buffer_range_and_session_colour() {
    for codec in CODECS {
        for (format, full) in [
            (kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, false),
            (kCVPixelFormatType_420YpCbCr8BiPlanarFullRange, true),
        ] {
            let mut enc = VtEncoder::open(&params(codec, 640, 360, 4_000_000)).unwrap();
            let aus = run(
                &mut enc,
                (0..3).map(|n| pixel_frame(640, 360, n, format)),
                |_, _| {},
            );
            let r = reads(codec, &aus)[0];
            assert_eq!(
                (r.full_range, r.primaries, r.transfer),
                (full, 1, 1),
                "{codec:?}"
            );
        }
        for full_range in [false, true] {
            let p = OpenParams {
                full_range,
                ..params(codec, 640, 360, 4_000_000)
            };
            let mut enc = VtEncoder::open(&p).unwrap();
            let aus = run(&mut enc, (0..3).map(|n| cpu_frame(640, 360, n)), |_, _| {});
            assert!(
                !reads(codec, &aus)[0].full_range,
                "{codec:?} BGRA asked full={full_range}"
            );
        }
    }
}

/// A 10-bit HDR session over P010 buffers: Main 10, BT.2020 PQ, and the mastering volume on
/// each keyframe once the capturer hands one over.
#[test]
#[ignore = "needs the media engine"]
fn a_ten_bit_hdr_session_signals_pq_and_carries_the_mastering_volume() {
    assert!(
        opens(Codec::H265, true, ChromaFormat::Yuv420),
        "Main 10 opens"
    );
    let p = OpenParams {
        bit_depth: 10,
        hdr: true,
        format: PixelFormat::P010,
        ..params(Codec::H265, 1280, 720, 8_000_000)
    };
    let mut enc = VtEncoder::open(&p).unwrap();
    let frames = (0..30).map(|n| {
        pixel_frame(
            1280,
            720,
            n,
            kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
        )
    });
    let aus = run(&mut enc, frames, |i, e| {
        if i == 10 {
            e.set_hdr_meta(Some(pf_frame::hdr::generic_hdr10()));
            e.request_keyframe();
        }
    });
    let r = reads(Codec::H265, &aus);
    assert_eq!(
        (r[0].depth, r[0].primaries, r[0].transfer, r[0].chroma_idc),
        (10, 9, 16, 1)
    );
    assert!(
        !has_mastering_sei(&aus[0].data),
        "no volume before the capturer names one"
    );
    assert!(aus[10].keyframe && has_mastering_sei(&aus[10].data));
    assert!(
        !has_mastering_sei(&aus[11].data),
        "the volume rides keyframes only"
    );
}

/// The 4:4:4 answer is VideoToolbox's: when the profile opens, the stream is 4:4:4 from BGRA.
#[test]
#[ignore = "needs the media engine"]
fn four_four_four_follows_what_videotoolbox_opens() {
    let p = OpenParams {
        chroma: ChromaFormat::Yuv444,
        ..params(Codec::H265, 1280, 720, 8_000_000)
    };
    let offered = opens(Codec::H265, false, ChromaFormat::Yuv444);
    println!(
        "HEVC 4:4:4 opens: {offered}, 10-bit 4:4:4: {}",
        opens(Codec::H265, true, ChromaFormat::Yuv444)
    );
    let Ok(mut enc) = VtEncoder::open(&p) else {
        assert!(!offered, "the probe and the open agree");
        return;
    };
    assert!(offered, "the probe and the open agree");
    assert!(enc.caps().chroma_444);
    let aus = run(
        &mut enc,
        (0..10).map(|n| cpu_frame(1280, 720, n)),
        |_, _| {},
    );
    assert_eq!(reads(Codec::H265, &aus)[0].chroma_idc, 3);
}

/// Encode time per frame, submit to AU, one frame in flight at 60 Hz, pixel buffers prepared
/// ahead.
#[test]
#[ignore = "needs the media engine; prints timings"]
fn encode_time_per_frame() {
    for codec in CODECS {
        for (w, h, bps) in [(1920u32, 1080u32, 30_000_000u64), (3840, 2160, 80_000_000)] {
            let mut enc = VtEncoder::open(&params(codec, w, h, bps)).unwrap();
            let format = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
            let frames: Vec<CFRetained<CVPixelBuffer>> = (0..8)
                .map(|n| match pixel_frame(w, h, n, format).payload {
                    FramePayload::PixelBuffer(b) => b.0,
                    _ => unreachable!(),
                })
                .collect();
            let mut times = Vec::new();
            for i in 0..300u32 {
                let b = frames[i as usize % frames.len()].clone();
                let f = frame(
                    w,
                    h,
                    i,
                    PixelFormat::Nv12,
                    FramePayload::PixelBuffer(PixelBufferFrame(b)),
                );
                let t = Instant::now();
                enc.submit(&f).unwrap();
                while enc.poll().unwrap().is_none() {}
                times.push(t.elapsed().as_micros() as u64);
                std::thread::sleep(Duration::from_micros(16_667).saturating_sub(t.elapsed()));
            }
            times.drain(..30);
            times.sort_unstable();
            let p = |q: f64| times[((times.len() - 1) as f64 * q) as usize];
            println!(
                "{codec:?} {w}x{h}@60: encode p50 {} µs, p95 {} µs, max {} µs",
                p(0.5),
                p(0.95),
                times[times.len() - 1]
            );
        }
    }
}
