//! The client's bitstream planners on a stream a host sends.
//!
//! The input is a raw stream, so a vendored vector is a seed as it stands: Annex-B for H.264
//! and H.265, IVF for AV1. Every codec's planner reads every input, one access unit at a time
//! with its state carried across them, as a session does; so do the recoders a client runs on
//! the same units (the H.265 POC widener and both concealers). The crate is
//! `forbid(unsafe_code)`: a finding is a panic on the client's decode thread.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pf_bitstream::av1::{self, conceal::Av1Concealer, tiles::plan_bitstream, Av1Planner};
use pf_bitstream::h264::H264Planner;
use pf_bitstream::h265::{conceal::H265Concealer, poc_widen::PocWidener, H265Planner};
use pf_bitstream::testing::{split_h264_aus, split_h265_aus};

/// IVF frames: a 32-byte file header, then `size u32 ‖ pts u64 ‖ payload` each. A short last
/// frame is read as far as it goes.
fn ivf_frames(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = stream.get(32..).unwrap_or(&[]);
    std::iter::from_fn(move || {
        let size = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let body = rest.get(12..)?;
        let (frame, after) = body.split_at(size.min(body.len()));
        rest = after;
        Some(frame)
    })
}

fuzz_target!(|data: &[u8]| {
    let mut h264 = H264Planner::new();
    for au in split_h264_aus(data) {
        let _ = h264.plan_au(au);
    }
    let mut h265 = H265Planner::new();
    let mut widener = PocWidener::new();
    let mut h265_concealer = H265Concealer::new();
    for au in split_h265_aus(data) {
        let _ = h265.plan_au(au);
        let _ = widener.widen(au);
        let _ = h265_concealer.conceal(au);
    }
    let mut planner = Av1Planner::new();
    let mut av1_concealer = Av1Concealer::new();
    for unit in ivf_frames(data) {
        let _ = av1::sequence_info(unit);
        let _ = av1_concealer.conceal(unit);
        for plan in planner.plan_au(unit).unwrap_or_default() {
            let _ = plan_bitstream(unit, &plan.tiles, &plan.header);
        }
    }
});
