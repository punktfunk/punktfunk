//! Does the native encoder produce bytes a decoder accepts?
//!
//! Ignored: needs a VAAPI encode device. `.25` (AMD 780M) and `.50` (Intel UHD 750)
//! both have one.
//!
//! ```text
//! cargo test -p pf-libva --test encode_h264 -- --ignored --nocapture
//! ```
//!
//! `PF_ENC_OUT=/tmp/out.h264` writes the stream out so `ffmpeg -i` can be the second
//! opinion — our own planner agreeing with our own encoder proves less than a decoder
//! that shares no code with either.

mod common;

use common::frame;
use pf_libva::encode::open;
use pf_libva::encode::CodecParams;
use pf_vaapi::enc_params::SessionParams;

/// 320x240 keeps the surfaces small and is still off the macroblock grid in neither
/// dimension, so the crop path stays out of the way of a first-frame proof.
fn params() -> SessionParams {
    SessionParams {
        width: 320,
        height: 240,
        fps_num: 60,
        fps_den: 1,
        bitrate_bps: 4_000_000,
        slots: 1,
        max_num_reorder_frames: 0,
        initial_qp: 26,
        vbv_frames: 1.0,
    }
}

/// Thirty frames: SPS, PPS, one IDR, twenty-nine P slices, and every access unit
/// accepted by the client's own planner. `PF_ENC_OUT` hands the same bytes to
/// ffmpeg, the reader that shares no code with either side.
#[test]
#[ignore = "needs a VAAPI encode device"]
fn the_encoder_emits_a_decodable_stream() {
    let p = params();
    let mut enc = match open(p, CodecParams::H264) {
        Ok(e) => e,
        Err(e) => panic!("no VAAPI encoder here: {e:#}"),
    };

    let mut stream = Vec::new();
    let mut sizes = Vec::new();
    for i in 0..30 {
        let (y, uv) = frame(p.width as usize, p.height as usize, i);
        enc.write_nv12(&y, &uv).expect("fill the input surface");
        enc.encode(i == 0).expect("encode");
        let pic = enc
            .collect(true)
            .expect("collect")
            .expect("a picture per encode");
        assert!(!pic.bytes.is_empty(), "frame {i} came back empty");
        assert_eq!(pic.is_idr, i == 0, "only the first frame opens the GOP");
        sizes.push(pic.bytes.len());
        stream.extend_from_slice(&pic.bytes);
    }

    println!("30 frames, {} bytes, sizes {:?}", stream.len(), &sizes[..5]);

    // Written before the assertions, so a failure still leaves the artefact to look at.
    if let Ok(path) = std::env::var("PF_ENC_OUT") {
        std::fs::write(&path, &stream).expect("write the stream out");
        println!("wrote {path} — check with: ffmpeg -v error -i {path} -f null -");
    }

    let starts: Vec<usize> = (0..stream.len().saturating_sub(4))
        .filter(|&i| stream[i..i + 4] == [0, 0, 0, 1])
        .collect();

    // One IDR and twenty-nine P slices. Counting types beats comparing sizes: a
    // moving edge makes P frames legitimately expensive, so size proves nothing,
    // but an encoder that ignores our slice_type emits thirty IDRs and this catches
    // it exactly.
    let nal_types: Vec<u8> = starts.iter().map(|&i| stream[i + 4] & 0x1f).collect();
    let idrs = nal_types.iter().filter(|&&t| t == 5).count();
    let ps = nal_types.iter().filter(|&&t| t == 1).count();
    assert_eq!(idrs, 1, "exactly one IDR: {nal_types:?}");
    assert_eq!(ps, 29, "the rest are P slices: {nal_types:?}");
    assert!(starts.len() >= 3, "expected SPS, PPS and slices");
    assert_eq!(stream[starts[0] + 4] & 0x1f, 7, "first NALU is the SPS");
    assert_eq!(stream[starts[1] + 4] & 0x1f, 8, "then the PPS");
    assert_eq!(stream[starts[2] + 4] & 0x1f, 5, "then an IDR slice");

    // Our own planner is the first reader: it is the client's, so a stream it
    // refuses is a stream the client refuses.
    let mut planner = pf_bitstream::h264::H264Planner::new();
    let aus = split_aus(&stream);
    let mut planned = 0;
    for (i, au) in aus.iter().enumerate() {
        match planner.plan_au(au) {
            Ok(_) => planned += 1,
            Err(e) => panic!("AU {i} did not plan: {e}"),
        }
    }
    assert_eq!(planned, 30, "every access unit should plan");
}

/// The headline: two pictures are lost, the next is predicted from the newest slot
/// the client still holds, and the client's own planner resolves that reference
/// to the pre-loss picture. No IDR anywhere after the first.
#[test]
#[ignore = "needs a VAAPI encode device"]
fn a_loss_recovers_on_an_anchored_p_not_an_idr() {
    let p = SessionParams {
        slots: 4,
        ..params()
    };
    let mut enc = open(p, CodecParams::H264).expect("an encoder");
    let aus = common::encode_through_a_loss(&mut enc, p.width as usize, p.height as usize);

    // What the client decodes: everything but 8 and 9.
    let received: Vec<&[u8]> = aus
        .iter()
        .enumerate()
        .filter(|(i, _)| !(8..10).contains(i))
        .map(|(_, au)| au.as_slice())
        .collect();
    if let Ok(path) = std::env::var("PF_ENC_OUT") {
        std::fs::write(&path, received.concat()).expect("write the stream out");
        println!("wrote {path} — 8 and 9 are missing on purpose");
    }

    let mut planner = pf_bitstream::h264::H264Planner::new();
    let mut stored = Vec::new();
    for (n, au) in received.iter().enumerate() {
        let plan = planner
            .plan_au(au)
            .unwrap_or_else(|e| panic!("AU {n} did not plan: {e}"));
        let refs: Vec<u64> = plan
            .slices
            .iter()
            .flat_map(|s| s.ref_list0.iter().map(|r| r.id))
            .collect();
        stored.push(plan.dpb.stored);
        // The client sees 0..=7 then 10: its reference must be 7's picture, not a
        // placeholder for the gap.
        if n == 8 {
            assert!(!plan.picture.is_idr, "recovery must not be an IDR");
            assert_eq!(refs, vec![stored[7].expect("7 was stored")]);
        } else if n > 8 {
            assert_eq!(
                refs,
                vec![stored[n - 1].expect("the previous picture was stored")]
            );
        }
    }
}

/// The bitrate steps mid-stream and the pictures follow, with no IDR. Noise makes
/// every rate bind — a flat picture would sit under both targets and prove nothing.
#[test]
#[ignore = "needs a VAAPI encode device"]
fn a_bitrate_step_lands_without_an_idr() {
    let p = SessionParams {
        bitrate_bps: 8_000_000,
        ..params()
    };
    let mut enc = open(p, CodecParams::H264).expect("an encoder");
    let (w, h) = (p.width as usize, p.height as usize);
    let mut seed = 0x2545_f491u32;
    let mut sizes = Vec::new();
    let mut idrs = 0;
    for i in 0..120 {
        if i == 60 {
            enc.set_bitrate(2_000_000);
        }
        let y: Vec<u8> = (0..w * h)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 24) as u8
            })
            .collect();
        enc.write_nv12(&y, &vec![128u8; w * h / 2]).expect("fill");
        enc.encode(i == 0).expect("encode");
        let pic = enc
            .collect(true)
            .expect("collect")
            .expect("a picture per encode");
        idrs += usize::from(
            pic.bytes
                .windows(5)
                .any(|s| s[..4] == [0, 0, 0, 1] && s[4] & 0x1f == 5),
        );
        sizes.push(pic.bytes.len());
    }
    let mean = |s: &[usize]| s.iter().sum::<usize>() / s.len();
    let (high, low) = (mean(&sizes[20..60]), mean(&sizes[80..120]));
    println!("8 Mbps: {high} B/frame, 2 Mbps: {low} B/frame, {idrs} IDR");
    assert_eq!(idrs, 1, "the step must not cost an IDR");
    let ratio = high as f64 / low as f64;
    assert!(
        (2.5..6.0).contains(&ratio),
        "sizes should track the rate: {high} vs {low}"
    );
    // 8 Mbps at 60 fps is 16.7 KB a picture; CBR should sit near it, not under.
    assert!(high > 10_000, "8 Mbps did not bind: {high} B/frame");
}

/// Split on the parameter-set/slice boundary: each AU here is the SPS+PPS+slice of
/// an IDR, or a single P slice.
fn split_aus(stream: &[u8]) -> Vec<&[u8]> {
    let starts: Vec<usize> = (0..stream.len().saturating_sub(4))
        .filter(|&i| stream[i..i + 4] == [0, 0, 0, 1])
        .collect();
    let mut aus = Vec::new();
    let mut au_start = 0;
    for (n, &s) in starts.iter().enumerate() {
        let nal_type = stream[s + 4] & 0x1f;
        // 7 = SPS opens a new AU; 1/5 = a slice ends one unless the SPS just began it.
        if n > 0 && (nal_type == 7 || (nal_type == 1 && stream[starts[n - 1] + 4] & 0x1f != 7)) {
            aus.push(&stream[au_start..s]);
            au_start = s;
        }
    }
    aus.push(&stream[au_start..]);
    aus
}
