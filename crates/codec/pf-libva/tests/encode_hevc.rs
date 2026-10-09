//! The HEVC session, on real hardware: a stream the client's planner and ffmpeg
//! both accept, a loss recovered through the reference picture set, and a Main 10
//! stream carrying HDR10 metadata.
//!
//! Ignored: needs a VAAPI encode device. `.25` and `.50` both have one.

mod common;

use common::frame;
use pf_libva::encode::open;
use pf_libva::encode::CodecParams;
use pf_vaapi::enc_h265::NAL_IDR_W_RADL;
use pf_vaapi::enc_h265::NAL_PPS;
use pf_vaapi::enc_h265::NAL_PREFIX_SEI;
use pf_vaapi::enc_h265::NAL_SPS;
use pf_vaapi::enc_h265::NAL_TRAIL_R;
use pf_vaapi::enc_h265::NAL_VPS;
use pf_vaapi::enc_params::SessionParams;
use pf_vaapi::hevc::HdrStatic;
use pf_vaapi::hevc::COLOUR_BT2020_PQ;
use pf_vaapi::hevc::COLOUR_BT709;
use pf_vaapi::vpp::VA_FOURCC_X2R10G10B10;

fn params() -> SessionParams {
    SessionParams {
        width: 320,
        height: 240,
        fps_num: 60,
        fps_den: 1,
        bitrate_bps: 4_000_000,
        slots: 4,
        max_num_reorder_frames: 0,
        initial_qp: 26,
        vbv_frames: 1.0,
    }
}

fn hevc(ten_bit: bool) -> CodecParams {
    CodecParams::Hevc {
        ten_bit,
        colour: if ten_bit {
            COLOUR_BT2020_PQ
        } else {
            COLOUR_BT709
        },
    }
}

/// The NAL unit types in an access unit, in order.
fn nal_types(au: &[u8]) -> Vec<u8> {
    (0..au.len().saturating_sub(4))
        .filter(|&i| au[i..i + 4] == [0, 0, 0, 1])
        .map(|i| au[i + 4] >> 1)
        .collect()
}

fn write_out(stream: &[u8]) {
    if let Ok(path) = std::env::var("PF_ENC_OUT") {
        std::fs::write(&path, stream).expect("write the stream out");
        println!("wrote {path}");
    }
}

/// VPS, SPS, PPS, one IDR, twenty-nine P, every access unit planned by the client.
#[test]
#[ignore = "needs a VAAPI encode device"]
fn the_hevc_stream_decodes() {
    let p = params();
    let mut enc = open(p, hevc(false)).expect("an HEVC encoder");
    let (w, h) = (p.width as usize, p.height as usize);
    let mut planner = pf_bitstream::h265::H265Planner::new();
    let mut stream = Vec::new();
    for i in 0..30 {
        let (y, uv) = frame(w, h, i);
        enc.write_nv12(&y, &uv).expect("fill");
        enc.encode(i == 0).expect("encode");
        let pic = enc
            .collect(true)
            .expect("collect")
            .expect("a picture per encode");
        assert_eq!(pic.is_idr, i == 0);
        let types = nal_types(&pic.bytes);
        if i == 0 {
            assert_eq!(
                types,
                vec![NAL_VPS, NAL_SPS, NAL_PPS, NAL_IDR_W_RADL],
                "{types:?}"
            );
        } else {
            assert_eq!(types, vec![NAL_TRAIL_R], "picture {i}: {types:?}");
        }
        let plan = planner
            .plan_au(&pic.bytes)
            .unwrap_or_else(|e| panic!("AU {i} did not plan: {e}"));
        assert!(plan.warnings.is_empty(), "AU {i}: {:?}", plan.warnings);
        assert_eq!(plan.picture.pic_order_cnt, i as i32);
        stream.extend_from_slice(&pic.bytes);
    }
    println!("30 frames, {} bytes", stream.len());
    write_out(&stream);
}

/// Two pictures lost; the next is predicted from the newest slot the client still
/// holds, and its reference picture set no longer names the lost ones — so the
/// client plans it with no warning at all. No IDR after the first.
#[test]
#[ignore = "needs a VAAPI encode device"]
fn an_hevc_loss_recovers_through_the_rps() {
    let p = params();
    let mut enc = open(p, hevc(false)).expect("an HEVC encoder");
    let aus = common::encode_through_a_loss(&mut enc, p.width as usize, p.height as usize);

    let received: Vec<&[u8]> = aus
        .iter()
        .enumerate()
        .filter(|(i, _)| !(8..10).contains(i))
        .map(|(_, au)| au.as_slice())
        .collect();
    write_out(&received.concat());

    let mut planner = pf_bitstream::h265::H265Planner::new();
    for (n, au) in received.iter().enumerate() {
        let plan = planner
            .plan_au(au)
            .unwrap_or_else(|e| panic!("AU {n} did not plan: {e}"));
        assert!(plan.warnings.is_empty(), "AU {n}: {:?}", plan.warnings);
        let refs: Vec<i32> = plan.slices[0]
            .ref_list0
            .iter()
            .map(|r| r.pic_order_cnt)
            .collect();
        // The client sees POC 0..=7 then 10: its reference must be 7, and the two
        // pictures it never got are not in the RPS, so nothing is missing.
        if n == 8 {
            assert!(!plan.picture.is_idr, "recovery must not be an IDR");
            assert_eq!(plan.picture.pic_order_cnt, 10);
            assert_eq!(refs, vec![7]);
        } else if n > 8 {
            assert_eq!(refs, vec![plan.picture.pic_order_cnt - 1]);
        }
    }
}

/// Main 10: ten-bit RGB in, P010 through the VPP, the SPS at bit depth 10 with
/// BT.2020 PQ tags, and the HDR10 SEI on the IDR.
#[test]
#[ignore = "needs a VAAPI encode device"]
fn a_ten_bit_stream_carries_hdr10() {
    let p = params();
    let mut enc = open(p, hevc(true)).expect("a Main 10 encoder");
    enc.set_hdr(Some(HdrStatic {
        display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
        white_point: [15635, 16450],
        max_display_mastering_luminance: 10_000_000,
        min_display_mastering_luminance: 50,
        max_cll: 1000,
        max_fall: 400,
    }));
    // X2R10G10B10: a little-endian word, red in bits 20..30.
    let red: [u8; 4] = (1023u32 << 20).to_le_bytes();
    let picture: Vec<u8> = red.repeat((p.width * p.height) as usize);
    let mut planner = pf_bitstream::h265::H265Planner::new();
    let mut stream = Vec::new();
    for i in 0..5 {
        enc.submit_packed(
            &picture,
            VA_FOURCC_X2R10G10B10,
            p.width,
            p.height,
            p.width as usize * 4,
        )
        .expect("ten-bit ingest");
        enc.encode(i == 0).expect("encode");
        let pic = enc
            .collect(true)
            .expect("collect")
            .expect("a picture per encode");
        if i == 0 {
            assert_eq!(
                nal_types(&pic.bytes),
                vec![NAL_VPS, NAL_SPS, NAL_PPS, NAL_PREFIX_SEI, NAL_IDR_W_RADL]
            );
        }
        let plan = planner
            .plan_au(&pic.bytes)
            .unwrap_or_else(|e| panic!("AU {i} did not plan: {e}"));
        assert!(plan.warnings.is_empty(), "AU {i}: {:?}", plan.warnings);
        assert_eq!(plan.sps.bit_depth_luma_minus8, 2);
        assert_eq!(plan.sps.vui_parameters.transfer_characteristics, 16, "PQ");
        stream.extend_from_slice(&pic.bytes);
    }
    write_out(&stream);
}
