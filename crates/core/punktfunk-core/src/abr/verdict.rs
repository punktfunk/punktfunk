//! What one window is: severe, bad, quiet, starved — and which signal said so.
//!
//! Loss and a flush are absolute; one-way delay, client decode and host
//! encode are relative, each scored against the rolling minimum of the last
//! [`BASELINE_WINDOWS`] windows that carried the signal. An unrecoverable
//! frame is absolute too unless it is the only damage behind a long clean run
//! at this rate, which makes it a blip and moves nothing. Severe decides on
//! one window, ordinary on two. Quiet windows teach no baseline, and a starved
//! window withholds the host-encode signal.

use super::sample::WindowSample;
use std::collections::VecDeque;

/// Shard loss at which one window backs off. 6 % is past any retry tail;
/// 750 ms spent there is visible damage.
pub(super) const SEVERE_LOSS_PPM: u32 = 60_000;
/// Shard loss that marks a window bad without an unrecoverable frame. 2 %
/// sustained is congestion, not the random tail FEC exists for.
pub(super) const HEAVY_LOSS_PPM: u32 = 20_000;
/// Decode-recovery keyframe asks that mark a window bad. Two asks in 750 ms
/// means the decoder is overdriven, whatever `loss_ppm` says. RFI asks are
/// not counted — `loss_ppm` already prices them.
pub(super) const RECOVERY_KF_BAD: u32 = 2;
/// Keyframe asks that make one window severe. Emitters throttle at 100 ms, so
/// 4+ in 750 ms means most of the window produced no pictures.
pub(super) const RECOVERY_KF_SEVERE: u32 = 4;
/// Clean windows at the current rate behind a lone lost frame before it reads
/// as a blip. 8 × 750 ms = 6 s: long enough that the rate it held is proven,
/// short enough that a link dropping a frame every few seconds still cuts.
pub(super) const BLIP_CLEAN_WINDOWS: u32 = 8;
/// Lone dead frames a flat link may lose as blips in a minute, in percent of the frames
/// it carried: 36 at 60 Hz, a frame every two seconds.
pub(super) const LONE_DEAD_PER_MIN_PCT: u64 = 1;
/// Tail loss that names a filling queue: at least this many shards from the frames' tails
/// and their parity…
pub(super) const TAIL_MIN: u32 = 4;
/// …this many times the heads and middles lost. Parity leaves last, so an overflowing queue
/// drops it first; random loss puts at most two thirds there, even at 50 % parity.
pub(super) const TAIL_RATIO: u32 = 3;
/// Head loss that names a waking receiver: at least this many shards from the frames'
/// heads…
pub(super) const HEAD_MIN: u32 = 4;
/// …this many times everything else lost, with the socket dropping nothing.
pub(super) const HEAD_RATIO: u32 = 3;
/// One-way-delay rise above the rolling baseline that counts as queue growth.
/// 25 ms is far beyond jitter at any streamable frame rate.
const OWD_RISE_US: i64 = 25_000;
/// Decode-stage rise (received → decoded) that marks the decoder falling
/// behind, when no frame budget is known. With one, half a budget: the stage
/// includes the queue, so that is half a frame of standing backlog.
const DECODE_RISE_US: i64 = 15_000;
/// Severe decode rise without a frame budget. With one, 1.5 budgets: several
/// frames of backlog, and a second window is 750 ms more of visible damage.
const DECODE_SEVERE_US: i64 = 45_000;
/// Host-encode rise (`0xCF` `encode_us`) that marks the encoder past its
/// compute knee. Relative, not absolute: an escalated host inflates
/// `encode_us` by ~a frame of retrieve-queue. 4 ms ≈ half a 120 Hz frame
/// until the session's frame budget is known.
const ENCODE_RISE_US: i64 = 4_000;
/// Host-encode rise that is severe (≈1.5 × a 120 Hz budget). Scaled with
/// [`ENCODE_RISE_US`] once a frame budget is known.
const ENCODE_SEVERE_US: i64 = 12_000;
/// A deciding window that delivered under `current / 4` is starved: the
/// stream barely flowed, so distress is stall-shaped, not rate-shaped. It
/// may still back off on what the client saw, but it must not sample a decode
/// knee and must not carry host-encode (`encode_us` averaged over almost no
/// AUs describes the interruption). Far below the ×¾ climb bar; the band
/// between them stays ambiguous on purpose.
const STARVED_DELIVERY_DIV: u32 = 4;
/// Rolling window (~30 s at 750 ms) whose minimum mean is the latency
/// baseline. Long enough to remember the uncongested floor.
const BASELINE_WINDOWS: usize = 40;
/// Samples a rolling-min baseline must hold before its signal may fire. One
/// sample *is* the min; a calm seed plus ordinary variance reads as rise.
/// Our own decrease clears the encode baseline, so four windows (3 s) is the
/// floor.
pub(super) const BASELINE_MIN_WINDOWS: usize = 4;

/// Why the rate moved, named by the signal that decided the window.
///
/// Severe signals rank ahead of ordinary ones, in the order they are scored.
/// A window nothing flagged is [`Clean`](Self::Clean), or [`Quiet`](Self::Quiet)
/// when there was no new content to judge. [`Blip`](Self::Blip) moves no rate:
/// it is a lost frame a long clean run says the link did not cause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Clean,
    Quiet,
    Blip,
    LostFrame,
    Flush,
    Loss,
    Owd,
    Decode,
    Encode,
    KeyframeAsks,
}

/// One window scored. `decode_mean_us` is the sample's, with quiet windows
/// filtered out — what the decisions downstream of the verdict read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Verdict {
    pub severe: bool,
    pub bad: bool,
    pub quiet: bool,
    pub starved: bool,
    /// One-way delay over its rolling baseline. The link's own signal, and so
    /// what tells a queue that is filling from a fault the rate cannot fix.
    pub owd_bad: bool,
    pub decode_bad: bool,
    pub decode_severe: bool,
    pub encode_bad: bool,
    pub encode_severe: bool,
    pub decode_mean_us: Option<i64>,
    pub reason: Reason,
}

/// This device's own socket dropped packets: the receiver could not drain the rate. The
/// window is the link's whatever `loss_ppm` says.
pub(super) fn link_signature(w: &WindowSample) -> bool {
    w.sock_drops > 0
}

/// Loss at the frames' tails, well past the rest: a queue on the path overflowing. A bad
/// window that shows it is the link's whatever `loss_ppm` says.
pub(super) fn tail_signature(w: &WindowSample) -> bool {
    tails(w.head, w.mid, w.tail)
}

/// The tail signature while every frame still decodes: the queue is close to full, and a
/// mark can go in before the picture breaks.
pub(super) fn tail_mark(w: &WindowSample) -> bool {
    tail_signature(w) && w.dropped == 0
}

fn tails(head: u32, mid: u32, tail: u32) -> bool {
    tail >= TAIL_MIN && tail >= TAIL_RATIO.saturating_mul(head.saturating_add(mid))
}

/// Loss at the frames' heads, well past the rest, with the socket dropping nothing: a
/// receiver whose adapter wakes late. The wake shape answers it, not the rate.
pub(crate) fn head_signature(w: &WindowSample) -> bool {
    heads(w.head, w.mid, w.tail, w.sock_drops)
}

fn heads(head: u32, mid: u32, tail: u32, sock_drops: u32) -> bool {
    head >= HEAD_MIN
        && head >= HEAD_RATIO.saturating_mul(mid.saturating_add(tail))
        && sock_drops == 0
}

/// A report window's lost shards fell at its frames' heads or tails: a waking receiver or
/// a filling queue. A host that sees one keeps its encoder on the frames the client
/// confirmed.
pub fn shows_loss_shape(head: u32, mid: u32, tail: u32, sock_drops: u32) -> bool {
    heads(head, mid, tail, sock_drops) || tails(head, mid, tail)
}

/// Rolling-min baselines for the three relative signals.
#[derive(Debug)]
pub(crate) struct Baselines {
    owd: VecDeque<i64>,
    decode: VecDeque<i64>,
    encode: VecDeque<i64>,
    /// Windows of the last minute that lost frames: when, and how many.
    dead: VecDeque<(std::time::Instant, u64)>,
    /// The last window was a blip: the next one is judged without the lone-frame rule.
    last_blip: bool,
}

impl Baselines {
    pub(crate) fn new() -> Self {
        Baselines {
            owd: VecDeque::with_capacity(BASELINE_WINDOWS),
            decode: VecDeque::with_capacity(BASELINE_WINDOWS),
            encode: VecDeque::with_capacity(BASELINE_WINDOWS),
            dead: VecDeque::new(),
            last_blip: false,
        }
    }

    /// A lone dead frame on a flat link, within the minute's budget: frames lost over the
    /// last minute, this one included, stay under [`LONE_DEAD_PER_MIN_PCT`] of the frames
    /// the minute carries at this refresh. Not in the window right after a blip, and not
    /// in a window whose loss has a filling queue's shape.
    fn lone_dead(&self, w: &WindowSample, dropped: u64, frame_budget_us: Option<i64>) -> bool {
        let Some(budget_us) = frame_budget_us.filter(|&b| b > 0) else {
            return false;
        };
        let minute = std::time::Duration::from_secs(60);
        let recent: u64 = self
            .dead
            .iter()
            .filter(|(t, _)| w.now.duration_since(*t) < minute)
            .map(|(_, n)| n)
            .sum();
        let frames = minute.as_micros() as u64 / budget_us as u64;
        dropped == 1
            && !self.last_blip
            && !tails(w.head, w.mid, w.tail)
            && w.delay
                .is_some_and(|d| d.rise_us < super::controller::DRAIN_FALL_US)
            && (recent + 1) * 100 < frames * LONE_DEAD_PER_MIN_PCT
    }

    /// Every signal is a property of the mode that produced it.
    pub(crate) fn clear(&mut self) {
        self.owd.clear();
        self.decode.clear();
        self.encode.clear();
    }

    /// Our own decrease changes the encode-time regime. Judging the new
    /// regime against the old baseline would train-fire.
    pub(crate) fn clear_encode(&mut self) {
        self.encode.clear();
    }

    /// The decode driver stood down: its latency is not the rate's to read.
    pub(crate) fn clear_decode(&mut self) {
        self.decode.clear();
    }

    /// Score one window, then record what it taught.
    ///
    /// `current_kbps` is the acked rate starvation is measured against,
    /// `frame_budget_us` sizes the decode and encode thresholds,
    /// `encode_disarmed` withholds the host-encode signal entirely and
    /// `decode_disarmed` the client-decode one (its driver found the latency
    /// is not a function of the rate),
    /// `clean_run` is the undamaged windows this rate has already held —
    /// what tells a blip from the first window of congestion — and
    /// `draining` says the last link cut is still emptying the queue it
    /// caused, which is the one thing a delay rise can mean that the rate
    /// must not answer again, `link_vouches` is the same vouching the run
    /// gives, read off this window instead of its history, `freeze_owd`
    /// hands the window's delay to the lift being judged — the baseline holds
    /// still and the verdict is the lift's, because a baseline that learns the
    /// rise it is supposed to detect detects nothing — and `still` says the
    /// source drew too few frames for its loss to be the rate's doing.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn score(
        &mut self,
        w: &WindowSample,
        current_kbps: u32,
        frame_budget_us: Option<i64>,
        encode_disarmed: bool,
        decode_disarmed: bool,
        clean_run: u32,
        draining: bool,
        link_vouches: bool,
        freeze_owd: bool,
        still: bool,
    ) -> Verdict {
        let quiet = w.activity.quiet();
        // Keepalive OWD/decode would train the rolling min on the quietest
        // traffic, so the first motion window reads as congestion.
        let owd_mean_us = w.owd_mean_us.filter(|_| !quiet);
        let decode_mean_us = w.decode_mean_us.filter(|_| !quiet);
        // No severe OWD tier: a standing queue is congestion, not visible
        // damage, so it always takes the two-window path.
        let (owd_rise, _) = score_baseline(
            &mut self.owd,
            owd_mean_us.filter(|_| !freeze_owd),
            OWD_RISE_US,
            i64::MAX,
        );
        // Delay over a queue the last cut is still draining is that cut
        // working. The baseline still learns the window: only the verdict is
        // withheld, and only for delay — loss, drops and a flush keep full
        // power.
        let owd_bad = owd_rise && !draining;
        // Decode rise ends slow start immediately; a far-past-baseline
        // excursion is severe (one window). Sized in frame budgets. Stood
        // down, the signal is withheld and teaches no baseline, as encode is.
        let (decode_rise_us, decode_severe_us) = decode_thresholds(frame_budget_us);
        let (decode_bad, decode_severe) = score_baseline(
            &mut self.decode,
            decode_mean_us.filter(|_| !decode_disarmed),
            decode_rise_us,
            decode_severe_us,
        );
        let starved = (w.actual_kbps as u64) * (STARVED_DELIVERY_DIV as u64) < current_kbps as u64;
        // Encode: the only signal that can descend on a clean LAN. Withheld
        // when starved (mean describes the interruption), disarmed, or quiet.
        // Passed as absent so it cannot teach the baseline either. Loss,
        // flush, and drop keep full power.
        let (encode_rise_us, encode_severe_us) = encode_thresholds(frame_budget_us);
        let encode_usable = !starved && !encode_disarmed && !quiet;
        let (encode_bad, encode_severe) = score_baseline(
            &mut self.encode,
            w.encode_mean_us.filter(|_| encode_usable),
            encode_rise_us,
            encode_severe_us,
        );
        // A still source offers the link a sliver of the rate: its loss and
        // lost frames are the path's, and cutting would not change them. FEC
        // and RFI answer them. A queue holding its frames is still the rate's.
        let path_noise = still && !owd_bad;
        let loss_ppm = if path_noise { 0 } else { w.loss_ppm };
        let dropped = if path_noise { 0 } else { w.dropped };
        let link_sig = !path_noise && link_signature(w);
        // A lost frame and nothing else is the recovery plane's, not the rate's:
        // after a long clean run, on a wire and delay with room, or lone on a flat
        // link within the minute's budget. Frames dying at their heads are a waking
        // receiver's, which the wake shape answers, however heavy.
        let head = head_signature(w);
        let lone = self.lone_dead(w, dropped, frame_budget_us);
        let blip = dropped > 0
            && (clean_run >= BLIP_CLEAN_WINDOWS || link_vouches || lone || head)
            && !link_sig
            && (loss_ppm < HEAVY_LOSS_PPM || head)
            && !w.flushed
            && !owd_bad
            && !decode_bad
            && !encode_bad
            && w.recovery_kf < RECOVERY_KF_BAD;
        // Severe: one window. Ordinary congestion: two consecutive.
        let severe = !blip
            && (dropped > 0
                || w.flushed
                || loss_ppm >= SEVERE_LOSS_PPM
                || decode_severe
                || encode_severe
                || w.recovery_kf >= RECOVERY_KF_SEVERE);
        let bad = severe
            || (!blip
                && (loss_ppm >= HEAVY_LOSS_PPM
                    || link_sig
                    || owd_bad
                    || decode_bad
                    || encode_bad
                    || w.recovery_kf >= RECOVERY_KF_BAD));
        let mut v = Verdict {
            severe,
            bad,
            quiet,
            starved,
            owd_bad,
            decode_bad,
            decode_severe,
            encode_bad,
            encode_severe,
            decode_mean_us,
            reason: Reason::Clean,
        };
        v.reason = if blip {
            Reason::Blip
        } else {
            let seen = WindowSample {
                loss_ppm,
                dropped,
                ..*w
            };
            reason(&seen, owd_bad, &v)
        };
        self.last_blip = blip;
        if dropped > 0 {
            self.dead.push_back((w.now, dropped));
        }
        while self
            .dead
            .front()
            .is_some_and(|(t, _)| w.now.duration_since(*t) >= std::time::Duration::from_secs(60))
        {
            self.dead.pop_front();
        }
        v
    }
}

/// The signal that decided the window, in the order the verdict scores them:
/// the severe tier first, then the ordinary one.
fn reason(w: &WindowSample, owd_bad: bool, v: &Verdict) -> Reason {
    if !v.bad {
        return if v.quiet {
            Reason::Quiet
        } else {
            Reason::Clean
        };
    }
    if w.dropped > 0 {
        Reason::LostFrame
    } else if w.flushed {
        Reason::Flush
    } else if w.loss_ppm >= SEVERE_LOSS_PPM {
        Reason::Loss
    } else if v.decode_severe {
        Reason::Decode
    } else if v.encode_severe {
        Reason::Encode
    } else if w.recovery_kf >= RECOVERY_KF_SEVERE {
        Reason::KeyframeAsks
    } else if w.loss_ppm >= HEAVY_LOSS_PPM || link_signature(w) {
        Reason::Loss
    } else if owd_bad {
        Reason::Owd
    } else if v.decode_bad {
        Reason::Decode
    } else if v.encode_bad {
        Reason::Encode
    } else {
        Reason::KeyframeAsks
    }
}

/// Decode `(rise, severe)` in frame budgets. The stage includes the queue, so
/// half a budget of rise is half a frame of standing backlog at any refresh;
/// the absolute defaults stand until a budget is known.
pub(crate) fn decode_thresholds(frame_budget_us: Option<i64>) -> (i64, i64) {
    match frame_budget_us {
        Some(budget) => (budget / 2, budget * 3 / 2),
        None => (DECODE_RISE_US, DECODE_SEVERE_US),
    }
}

/// Encode `(rise, severe)`: half a frame budget and 1.5 of them, against this
/// session's refresh. Not the source's delivered fps — inferring that from
/// arrival cadence is the jitter the signal is trying to read through.
pub(crate) fn encode_thresholds(frame_budget_us: Option<i64>) -> (i64, i64) {
    match frame_budget_us {
        Some(budget) => (budget / 2, budget * 3 / 2),
        None => (ENCODE_RISE_US, ENCODE_SEVERE_US),
    }
}

/// Score one window's latency against its rolling-min baseline, then record it.
///
/// Shared by OWD, client decode, and host encode. `mean` is `None` when nobody
/// reports the signal — absent, not clean, so it neither marks bad nor teaches
/// a baseline. Compared against PRIOR windows before recording, and only after
/// [`BASELINE_MIN_WINDOWS`]. Pass `i64::MAX` for `severe_us` on a signal with
/// no severe tier.
fn score_baseline(
    means: &mut VecDeque<i64>,
    mean: Option<i64>,
    rise_us: i64,
    severe_us: i64,
) -> (bool, bool) {
    let Some(mean) = mean else {
        return (false, false);
    };
    let base = (means.len() >= BASELINE_MIN_WINDOWS)
        .then(|| means.iter().min().copied())
        .flatten();
    let over = |t: i64| base.is_some_and(|b| mean > b.saturating_add(t));
    if means.len() == BASELINE_WINDOWS {
        means.pop_front();
    }
    means.push_back(mean);
    (over(rise_us), over(severe_us))
}

#[cfg(test)]
mod tests {
    use super::super::controller::BitrateController;
    use super::super::harness::*;
    use super::super::sample::WindowActivity;
    use super::*;
    use std::time::Instant;

    /// Where a window's shards went missing names its cause: the socket's own drops a
    /// receiver that could not drain, the tails well past the rest a filling queue, the
    /// heads well past the rest a receiver waking late; a socket that drops vetoes the last.
    #[test]
    fn loss_positions_name_their_signature() {
        let w = |head, mid, tail, sock_drops| WindowSample {
            head,
            mid,
            tail,
            sock_drops,
            ..WindowSample::at(Instant::now())
        };
        assert!(link_signature(&w(0, 0, 0, 1)));
        assert!(!link_signature(&w(0, 3, 9, 0)));
        assert!(head_signature(&w(6, 1, 1, 0)));
        assert!(
            !head_signature(&w(6, 2, 1, 0)),
            "the rest lost a third as much"
        );
        assert!(!head_signature(&w(9, 0, 0, 3)), "the socket dropped");
        assert!(!head_signature(&w(3, 0, 0, 0)), "too few at the heads");
        assert!(tail_mark(&w(0, 0, 4, 0)));
        assert!(!tail_mark(&w(0, 0, 3, 0)), "too few at the tails");
        assert!(!tail_mark(&w(1, 2, 6, 0)), "the rest lost half as much");
        assert!(!tail_mark(&WindowSample {
            dropped: 1,
            ..w(0, 0, 4, 0)
        }));
        assert!(shows_loss_shape(6, 1, 1, 0) && shows_loss_shape(0, 0, 4, 0));
        assert!(!shows_loss_shape(0, 9, 1, 0), "mid-frame loss is neither");
        assert!(
            !shows_loss_shape(24, 93, 38, 0),
            "a minute of 0.7 % random loss is neither"
        );
    }

    /// A lone dead frame on a flat link is a blip while the minute's dead frames stay
    /// under [`LONE_DEAD_PER_MIN_PCT`] of the refresh. Two windows running, a frame dying at
    /// its tail, or the budget spent is damage the rate answers; frames dying at their
    /// heads are a waking receiver's, however heavy.
    #[test]
    fn lone_dead_frames_are_blips_within_the_minutes_budget() {
        let start = Instant::now();
        let level = crate::abr::DelayTrend {
            samples: 20,
            mean_us: 10_000,
            rise_us: 0,
            last_us: 10_000,
        };
        let flat = |at: u32, dropped: u64, tail: u32| WindowSample {
            owd_mean_us: Some(10_000),
            delay: Some(level),
            dropped,
            tail,
            actual_kbps: 20_000,
            ..WindowSample::at(ticks(start, at))
        };
        let fresh = || {
            let mut c = BitrateController::new(20_000, None);
            c.set_frame_budget(60);
            c
        };
        // One every other window: 40 a minute against the 36 allowed at 60 Hz.
        let mut c = fresh();
        for n in 0..35u32 {
            assert_eq!(c.on_window(&flat(2 * n, 1, 0)), None, "dead frame {n}");
            assert_eq!(c.last_reason(), Reason::Blip);
            assert_eq!(c.on_window(&flat(2 * n + 1, 0, 0)), None);
        }
        assert!(
            c.on_window(&flat(70, 1, 0)).is_some(),
            "the budget is spent"
        );

        let mut c = fresh();
        assert_eq!(c.on_window(&flat(0, 1, 0)), None);
        assert!(c.on_window(&flat(1, 1, 0)).is_some(), "two windows running");

        let mut c = fresh();
        assert_eq!(c.on_window(&flat(0, 1, 0)), None);
        assert_eq!(c.on_window(&flat(1, 0, 0)), None);
        assert!(
            c.on_window(&flat(2, 1, TAIL_MIN)).is_some(),
            "a frame dying at its tail"
        );

        let mut c = fresh();
        let heads = WindowSample {
            head: 10,
            loss_ppm: 30_000,
            ..flat(0, 3, 0)
        };
        assert_eq!(c.on_window(&heads), None);
        assert_eq!(c.last_reason(), Reason::Blip);
    }

    #[test]
    fn owd_rise_alone_is_a_congestion_signal() {
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        // ~10 ms OWD baseline.
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // +40 ms OWD, zero loss: two windows → back off.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(50_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(52_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 5))
            }),
            Some(14_000)
        );
        assert_eq!(c.last_cut(), Some(Reason::Owd));
        // A climb the host grants clears it; a cut it grants does not.
        c.on_ack(14_000, None);
        assert_eq!(c.last_cut(), Some(Reason::Owd));
        c.on_ack(15_000, None);
        assert_eq!(c.last_cut(), None);
    }

    #[test]
    fn decode_latency_rise_alone_is_a_congestion_signal() {
        // Pristine link; only decode latency is rising.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        // ~8 ms decode baseline.
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(8_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // +30 ms decode, zero loss, flat OWD: two windows → ×0.7.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(38_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(40_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 5))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn keyframe_ask_storm_alone_is_a_congestion_signal() {
        // Pristine link, no latency signal, two kf asks per window: ordinary-bad.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                recovery_kf: 2,
                ..WindowSample::at(ticks(start, 0))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                recovery_kf: 2,
                ..WindowSample::at(ticks(start, 1))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn keyframe_ask_saturation_is_severe() {
        // Emitters throttle at 100 ms: 4+ asks in 750 ms is severe.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                actual_kbps: 1_000_000,
                recovery_kf: 4,
                ..WindowSample::at(ticks(start, 0))
            }),
            Some(14_000)
        );
    }

    /// A clean run at the rate is what makes one lost frame isolated: without
    /// it, and for the next one inside it, the window cuts as it always did.
    #[test]
    fn one_lost_frame_after_a_clean_run_is_a_blip() {
        // Start at the ceiling, so no climb interrupts the run.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        let held = |at: u32| WindowSample {
            owd_mean_us: Some(10_000),
            actual_kbps: 20_000,
            ..WindowSample::at(ticks(start, at))
        };
        for i in 0..BLIP_CLEAN_WINDOWS {
            assert_eq!(c.on_window(&held(i)), None);
        }
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                ..held(BLIP_CLEAN_WINDOWS)
            }),
            None,
            "one lost frame, nothing else, after six seconds at this rate"
        );
        assert_eq!(c.last_reason(), Reason::Blip);
        assert!(c.probing, "and slow start is not spent on it");
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                ..held(BLIP_CLEAN_WINDOWS + 1)
            }),
            Some(14_000),
            "a second one inside the next run is congestion"
        );

        // Same window with no run behind it: the first window of a cascade.
        let mut c = BitrateController::new(20_000, None);
        assert_eq!(
            c.on_window(&WindowSample {
                dropped: 1,
                ..held(0)
            }),
            Some(14_000)
        );
    }

    /// A climbing session never holds a rate long enough to earn the run, so
    /// the window says for itself what the run would have said: the wire
    /// carried what the rate asked for and the queue is not growing.
    #[test]
    fn one_lost_frame_on_a_full_wire_is_a_blip_with_no_run_behind_it() {
        let start = Instant::now();
        let mut c = BitrateController::new(20_000, None);
        let held = |at: u32, dropped: u64, actual_kbps: u32| WindowSample {
            owd_mean_us: Some(10_000),
            delay: Some(crate::abr::DelayTrend {
                samples: 20,
                mean_us: 10_000,
                rise_us: 0,
                last_us: 10_000,
            }),
            dropped,
            actual_kbps,
            ..WindowSample::at(start + TICK * at)
        };
        // Four windows build this rate's wire norm, and no more than that.
        for i in 0..4 {
            assert_eq!(c.on_window(&held(i, 0, 20_000)), None);
        }
        assert_eq!(
            c.on_window(&held(4, 1, 20_000)),
            None,
            "one lost frame on a wire that carried the rate"
        );
        assert_eq!(c.last_reason(), Reason::Blip);
        assert_eq!(
            c.on_window(&held(5, 1, 20_000)),
            Some(14_000),
            "a second one inside the hold is congestion like any other"
        );

        // The same window on a wire that fell short: the link is answering.
        let mut c = BitrateController::new(20_000, None);
        for i in 0..4 {
            assert_eq!(c.on_window(&held(i, 0, 20_000)), None);
        }
        assert_eq!(
            c.on_window(&held(4, 1, 14_000)),
            Some(14_000),
            "a wire short of the rate is the link answering, not a blip"
        );
    }

    #[test]
    fn a_single_keyframe_ask_is_not_congestion() {
        // One kf ask is not congestion, even in a row.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    actual_kbps: 1_000_000,
                    recovery_kf: 1,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
    }

    #[test]
    fn one_calm_window_is_not_a_baseline() {
        // Our own decrease clears the encode baseline. One sample must not arm.
        let mut c = BitrateController::new(100_000, None);
        let start = Instant::now();
        // One 3 ms seed, then 12 ms: past [`ENCODE_RISE_US`], but no baseline yet.
        for i in 0..BASELINE_MIN_WINDOWS as u32 {
            let mean = if i == 0 { 3_000 } else { 12_000 };
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    encode_mean_us: Some(mean),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None,
                "window {i} fired off a baseline of fewer than {BASELINE_MIN_WINDOWS} samples"
            );
        }
        // With 4 samples, a sustained rise still backs off.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(20_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 8))
            }),
            Some(87_500)
        );
    }

    /// Same history: non-marking host trains OWD on keepalive and backs off
    /// at motion; marking host trains nothing and climbs.
    #[test]
    fn idle_windows_train_no_baselines() {
        let start = Instant::now();
        let run = |marking: bool| -> Option<u32> {
            let mut c = BitrateController::new(20_000, None);
            c.set_ceiling(300_000);
            c.set_frame_budget(60);
            // A host that marks repeats reports the new-content count; an
            // older one reports nothing but arrivals.
            let act = |n: u32| {
                if marking {
                    WindowActivity::Active(n)
                } else {
                    WindowActivity::Unmarked
                }
            };
            let mut decision = None;
            // Four active windows at 30 ms OWD.
            for i in 0..4 {
                let r = c.on_window(&WindowSample {
                    owd_mean_us: Some(30_000),
                    actual_kbps: 2_000,
                    activity: act(45),
                    ..WindowSample::at(ticks(start, i))
                });
                assert_eq!(r, None);
            }
            // Keepalive at 1 ms OWD: marking host trains nothing; legacy trains min to 1 ms.
            for i in 4..10 {
                let r = c.on_window(&WindowSample {
                    owd_mean_us: Some(1_000),
                    actual_kbps: 200,
                    activity: act(0),
                    ..WindowSample::at(ticks(start, i))
                });
                assert_eq!(r, None);
            }
            // Motion at the warmup's 30 ms OWD. First decision is the verdict
            // (cooldown silences the second).
            for i in 10..12 {
                let r = c.on_window(&WindowSample {
                    owd_mean_us: Some(30_000),
                    actual_kbps: 18_000,
                    activity: act(45),
                    ..WindowSample::at(ticks(start, i))
                });
                decision = decision.or(r);
            }
            decision
        };
        // Legacy: 30 ms vs 1 ms baseline → ×0.7.
        assert_eq!(run(false), Some(14_000));
        // Marking: 30 ms is normal; first utilized window climbs to 27 000.
        assert_eq!(run(true), Some(27_000));
    }

    #[test]
    fn deep_decode_excursion_is_severe() {
        // Decode rise >45 ms is already overload: one window.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    decode_mean_us: Some(8_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        // 52 ms over 8 ms baseline: immediate ×0.7. 30 ms still takes two.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                decode_mean_us: Some(60_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            Some(14_000)
        );
    }

    #[test]
    fn host_encode_latency_rise_backs_off() {
        // Only host encode time moves: two risen windows → one notch.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    encode_mean_us: Some(7_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(11_500),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            None
        );
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(12_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 6))
            }),
            Some(17_500)
        );
    }

    #[test]
    fn deep_encode_excursion_is_severe() {
        // ≈1.5 frame budgets over baseline: severe, one window.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        for i in 0..4 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    encode_mean_us: Some(7_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(20_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            Some(17_500)
        );
    }

    #[test]
    fn rate_decrease_rebases_the_encode_baseline() {
        // Our own decrease must rebase encode; old baseline would train-fire.
        let mut c = BitrateController::new(20_000, None);
        let start = Instant::now();
        for i in 0..4 {
            let _ = c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(7_000),
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, i))
            });
        }
        // A decrease the encoder had no part in: severe loss, ×0.7.
        assert_eq!(
            c.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(7_000),
                loss_ppm: SEVERE_LOSS_PPM,
                actual_kbps: 1_000_000,
                ..WindowSample::at(ticks(start, 4))
            }),
            Some(14_000)
        );
        // After rebase, 15 ms against the old 7 ms floor must read clean.
        c.on_ack(14_000, None);
        for i in 8..11 {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(10_000),
                    encode_mean_us: Some(15_000),
                    actual_kbps: 1_000_000,
                    ..WindowSample::at(ticks(start, i))
                }),
                None
            );
        }
    }

    #[test]
    fn the_encode_thresholds_follow_the_session_frame_budget() {
        // Same physical hiccup: severe at 120 Hz, ordinary at 60 Hz when
        // thresholds follow the session frame budget.
        let excursion = 23_700; // 7 ms baseline + ~one 60 Hz frame
        let mut hz120 = BitrateController::new(20_000, None);
        hz120.set_frame_budget(120);
        let mut tick = 0;
        let start = Instant::now();
        assert_eq!(
            encode_choke(&mut hz120, start, &mut tick, excursion),
            Some(17_500),
            "at 120 Hz that is ~2.8 frame budgets over baseline — severe, one window"
        );

        let mut hz60 = BitrateController::new(20_000, None);
        hz60.set_frame_budget(60);
        let mut tick = 0;
        assert_eq!(
            encode_choke(&mut hz60, start, &mut tick, excursion),
            None,
            "the same excursion is ~1 frame budget at 60 Hz — bad, but not severe"
        );
        // Second window still backs off: re-scaled, not weakened.
        let at = ticks(start, tick + 1);
        assert_eq!(
            hz60.on_window(&WindowSample {
                owd_mean_us: Some(10_000),
                encode_mean_us: Some(excursion),
                actual_kbps: 1_000_000,
                ..WindowSample::at(at)
            }),
            Some(17_500)
        );
    }

    #[test]
    fn decode_thresholds_follow_the_frame_budget() {
        // +6 000 µs over the baseline: half a 120 Hz budget (4 166) is a rise,
        // the 15 ms no-budget default is not.
        let mut hz120 = BitrateController::new(100_000, None);
        hz120.set_frame_budget(120);
        let mut plain = BitrateController::new(100_000, None);
        let start = Instant::now();
        for i in 0..5 {
            assert_eq!(loaded(&mut hz120, ticks(start, i), 3_000), None);
            assert_eq!(loaded(&mut plain, ticks(start, i), 3_000), None);
        }
        loaded(&mut hz120, ticks(start, 5), 9_000);
        loaded(&mut plain, ticks(start, 5), 9_000);
        assert!(!hz120.probing, "a budget-sized rise ends slow start");
        assert!(plain.probing, "below the absolute default it is not a rise");
    }

    /// Starved window: encode_us is not a measurement of encode cost. Withheld;
    /// the window decides nothing. The same excursion at full delivery still
    /// backs off.
    #[test]
    fn a_starved_window_cannot_back_off_on_host_encode_time_alone() {
        let mut c = BitrateController::new(20_000, None);
        c.set_ceiling(657_000);
        let start = Instant::now();
        let mut t = 0;
        // Half-utilized: encode samples count, no climb, `current_kbps` stays.
        for _ in 0..BASELINE_MIN_WINDOWS {
            assert_eq!(
                c.on_window(&WindowSample {
                    owd_mean_us: Some(3_500),
                    decode_mean_us: Some(200),
                    encode_mean_us: Some(2_800),
                    actual_kbps: 10_000,
                    ..WindowSample::at(ticks(start, t))
                }),
                None
            );
            t += 1;
        }
        assert!(
            c.probing,
            "slow start is still armed going into the rebuild"
        );

        let verdict = c.on_window(&WindowSample {
            owd_mean_us: Some(15_711),
            decode_mean_us: Some(129),
            encode_mean_us: Some(15_063),
            actual_kbps: 390,
            ..WindowSample::at(ticks(start, t))
        });
        t += 1;
        assert_eq!(
            verdict, None,
            "a host-local rebuild must not move the rate: nothing was lost and nothing was slow"
        );
        assert_eq!(c.current_kbps, 20_000, "and the rate is untouched");
        assert!(
            c.probing,
            "nor may it retire slow start — recovery would crawl at +6 % per six windows"
        );

        // Same encode excursion at full delivery is still severe.
        let verdict = c.on_window(&WindowSample {
            owd_mean_us: Some(3_600),
            decode_mean_us: Some(210),
            encode_mean_us: Some(15_063),
            actual_kbps: 20_000,
            ..WindowSample::at(ticks(start, t))
        });
        assert!(
            verdict.is_some_and(|k| k < 20_000),
            "a real encode excursion at full delivery still backs off, got {verdict:?}"
        );
    }
}
