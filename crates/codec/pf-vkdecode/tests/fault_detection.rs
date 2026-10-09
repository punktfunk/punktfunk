//! CPU harness for [`pf_vkdecode::AuFault`]: planner warnings the client conceals on.
//!
//! Damages the vendored 250-AU host-shaped streams, plans each AU as
//! `VkH264Decoder::decode` / `VkH265Decoder::decode` would, and scores with
//! [`pf_vkdecode::PlanWarning::is_integrity`] /
//! [`pf_vkdecode::H265PlanWarning::is_integrity`]. Vulkan submit is omitted; it
//! cannot change a plan's warnings.
//!
//! H.264 drop is a `frame_num` gap. HEVC has none — POC jumps are legal —
//! so drop detection is RPS `MissingReference` only. Covering one codec
//! proves nothing about the other. Truncate and payload flip are
//! parser-invisible (Annex-B has no NALU length; a flipped payload byte is
//! valid syntax) and need the driver's `RESULT_STATUS` in the GPU tests.
//! A session without `queryResultStatusSupport` is unmeasured, not clean.
//!
//! Run: `cargo test -p pf-vkdecode --test fault_detection`.

use pf_bitstream::h264::H264Planner;
use pf_bitstream::h265::H265Planner;
use pf_bitstream::testing::split_h264_aus;
use pf_bitstream::testing::split_h265_aus;
use pf_vkdecode::{AuFault, FaultAction, FaultMode, H265PlanWarning, PlanWarning};

/// 250 AUs, IDR then P — the host envelope. GPU tests share these files.
const TEST_25FPS_H264: &[u8] = pf_bitstream::testing::H264_25FPS;
const TEST_25FPS_H265: &[u8] = pf_bitstream::testing::H265_25FPS;

/// Driver `Failed` on a prior frame has no analogue here.
fn damaged_h264(planner: &mut H264Planner, au: &[u8]) -> bool {
    match planner.plan_au(au) {
        Ok(plan) => plan.warnings.iter().any(PlanWarning::is_integrity),
        // A refused plan is concealment; the client never shows that picture.
        Err(_) => true,
    }
}

/// `RaslSkipped` is not damage: `VkH265Decoder::decode` maps it to `Ok(None)`
/// (spec 8.1.3 NOTE, open-GOP RASL). Counting it would flag a production no-op.
fn damaged_h265(planner: &mut H265Planner, au: &[u8]) -> bool {
    use pf_bitstream::h265::PlanError;
    match planner.plan_au(au) {
        Ok(plan) => plan.warnings.iter().any(H265PlanWarning::is_integrity),
        Err(PlanError::RaslSkipped { .. }) => false,
        Err(_) => true,
    }
}

/// A drop is `false` at that index: the AU never reached the planner.
fn replay<'a>(
    stream: &'a [u8],
    split: fn(&'a [u8]) -> Vec<&'a [u8]>,
    mut verdict: impl FnMut(&[u8]) -> bool,
    fault: Option<AuFault>,
) -> (Vec<bool>, usize) {
    let aus = split(stream);
    let mut fault = fault;
    let mut flags = Vec::with_capacity(aus.len());
    let mut faulted = 0usize;
    for au in aus {
        let action = match &mut fault {
            Some(f) => f.apply(au),
            None => FaultAction::Pass,
        };
        match action {
            FaultAction::Pass => flags.push(verdict(au)),
            FaultAction::Drop => {
                faulted += 1;
                flags.push(false);
            }
            FaultAction::Corrupt(bytes) => {
                faulted += 1;
                flags.push(verdict(&bytes));
            }
        }
    }
    (flags, faulted)
}

fn replay_h264(fault: Option<AuFault>) -> (Vec<bool>, usize) {
    let mut planner = H264Planner::new();
    replay(
        TEST_25FPS_H264,
        split_h264_aus,
        move |au| damaged_h264(&mut planner, au),
        fault,
    )
}

fn replay_h265(fault: Option<AuFault>) -> (Vec<bool>, usize) {
    let mut planner = H265Planner::new();
    replay(
        TEST_25FPS_H265,
        split_h265_aus,
        move |au| damaged_h265(&mut planner, au),
        fault,
    )
}

/// Next AU only. A dropped reference is visible on the AU that names it;
/// "eventually" is the 500 ms freeze backstop, not this detector.
const DETECT_WITHIN: usize = 1;

/// 50 → five faults in 250 AUs, far enough that each window is one fault.
const PERIOD: u32 = 50;

/// 0-based indices [`AuFault`] hits at `PERIOD`. The injector is 1-based, so
/// the first hit is `PERIOD - 1` and a period > 1 leaves the opening IDR.
fn fault_indices(total: usize) -> Vec<usize> {
    (PERIOD as usize - 1..total)
        .step_by(PERIOD as usize)
        .collect()
}

/// Returns checked windows so a trailing-only vector cannot pass.
fn assert_detected_after_each_fault(flags: &[bool], what: &str) -> usize {
    let mut checked = 0usize;
    for dropped in fault_indices(flags.len()) {
        // Last AU has no successor; asserting on it tests fixture length.
        let Some(window) = flags.get(dropped + 1..(dropped + 1 + DETECT_WITHIN).min(flags.len()))
        else {
            continue;
        };
        if window.is_empty() {
            continue;
        }
        checked += 1;
        assert!(
            window.iter().any(|&d| d),
            "{what}: the AU(s) after dropped AU {dropped} must read as damaged — \
             the reference it needs was never decoded"
        );
    }
    checked
}

fn flagged(flags: &[bool]) -> Vec<usize> {
    flags
        .iter()
        .enumerate()
        .filter(|&(_, &d)| d)
        .map(|(i, _)| i)
        .collect()
}

/// Control: a detector that fires on every AU would pass every fault test.
#[test]
fn a_clean_stream_never_reads_as_damaged() {
    for (codec, (flags, faulted)) in [("h264", replay_h264(None)), ("h265", replay_h265(None))] {
        assert_eq!(faulted, 0, "{codec}: no fault armed");
        assert_eq!(flags.len(), 250, "{codec}: the whole vector replayed");
        assert!(
            flagged(&flags).is_empty(),
            "{codec}: the clean vector must plan without a single integrity \
             warning — flagged AUs: {:?}",
            flagged(&flags)
        );
    }
}

#[test]
fn a_dropped_access_unit_is_detected_on_the_very_next_one() {
    let (flags, faulted) = replay_h264(Some(AuFault::new(FaultMode::Drop, PERIOD)));
    assert!(faulted >= 4, "the injector fired ({faulted} AUs dropped)");
    let checked = assert_detected_after_each_fault(&flags, "h264");
    assert!(
        checked >= 4,
        "{checked} drops actually had a successor to check"
    );
}

/// HEVC drop detection is RPS `MissingReference` only — no `frame_num` gap.
/// A sub-layer non-reference (`TRAIL_N` and friends) is named by nobody, so
/// its loss is a missing output frame (`client::FrameOrder::Gap`), not bitstream
/// damage. Both verdicts are asserted so silence is not a hole and a hole
/// cannot hide behind it.
#[test]
fn a_dropped_hevc_reference_picture_is_detected_through_the_rps() {
    // Classify from a clean replay so the labels are the stream's, not a guess.
    let mut planner = H265Planner::new();
    let referenced: Vec<bool> = split_h265_aus(TEST_25FPS_H265)
        .into_iter()
        .map(|au| {
            planner
                .plan_au(au)
                .map(|plan| !plan.picture.nalu_type.is_slnr())
                .unwrap_or(false)
        })
        .collect();

    let (flags, faulted) = replay_h265(Some(AuFault::new(FaultMode::Drop, PERIOD)));
    assert!(faulted >= 4, "the injector fired ({faulted} AUs dropped)");

    let (mut checked_refs, mut checked_slnr) = (0usize, 0usize);
    for dropped in fault_indices(flags.len()) {
        let Some(window) = flags.get(dropped + 1..(dropped + 1 + DETECT_WITHIN).min(flags.len()))
        else {
            continue;
        };
        if window.is_empty() {
            continue;
        }
        if referenced[dropped] {
            checked_refs += 1;
            assert!(
                window.iter().any(|&d| d),
                "h265: the AU after dropped REFERENCE picture {dropped} must read \
                 as damaged — its RPS names a picture the DPB never received"
            );
        } else {
            checked_slnr += 1;
            assert!(
                !window.iter().any(|&d| d),
                "h265: dropping sub-layer non-reference picture {dropped} damages \
                 nothing — if the planner starts complaining here it is reporting \
                 damage that did not happen, and every such report costs a frame \
                 and a keyframe round trip"
            );
        }
    }
    assert!(
        checked_refs >= 2 && checked_slnr >= 2,
        "the vector must exercise BOTH classes ({checked_refs} reference drops, \
         {checked_slnr} non-reference drops) or this test proves only half of what \
         it claims"
    );
}

/// Annex-B has no NALU length, so a mid-slice cut plans as a shorter slice.
/// `PlanWarning::TruncatedAu` is a malformed header with bytes still behind
/// it, not this. Detection is the driver's `RESULT_STATUS` on real hardware.
#[test]
fn a_truncated_access_unit_is_invisible_to_the_parser_and_needs_the_driver_verdict() {
    for (codec, (flags, faulted)) in [
        (
            "h264",
            replay_h264(Some(AuFault::new(FaultMode::Truncate, PERIOD))),
        ),
        (
            "h265",
            replay_h265(Some(AuFault::new(FaultMode::Truncate, PERIOD))),
        ),
    ] {
        assert!(
            faulted >= 4,
            "{codec}: the injector fired ({faulted} AUs truncated)"
        );
        assert!(
            flagged(&flags).is_empty(),
            "{codec}: a mid-slice cut carries no syntax error — if this starts \
             firing, the cut is landing on a NALU header and the mode has stopped \
             exercising the driver-only path it exists for (flagged AUs: {:?})",
            flagged(&flags)
        );
    }
}

/// A payload XOR is valid syntax and a wrong picture. Parser silence is why
/// a session without `queryResultStatusSupport` is unmeasured, not clean.
#[test]
fn a_payload_bit_flip_is_invisible_to_the_parser_which_is_why_the_status_query_exists() {
    for (codec, (flags, faulted)) in [
        (
            "h264",
            replay_h264(Some(AuFault::new(FaultMode::Flip, PERIOD))),
        ),
        (
            "h265",
            replay_h265(Some(AuFault::new(FaultMode::Flip, PERIOD))),
        ),
    ] {
        assert!(
            faulted >= 4,
            "{codec}: the injector fired ({faulted} AUs flipped)"
        );
        assert!(
            flagged(&flags).is_empty(),
            "{codec}: a payload flip must not be parser-visible — if this ever \
             starts firing, the flip is landing in syntax rather than payload and \
             the mode has stopped testing what it claims (flagged AUs: {:?})",
            flagged(&flags)
        );
    }
}
