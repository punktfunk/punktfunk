//! Client audio loss recovery: sequence gaps, stale packets, the redundant `0xD2` plane and
//! bounded concealment of a packet drought.

use super::*;

/// Loss detector for the client audio plane, shared by every platform decoder.
///
/// `0xC9` datagrams carry a wrapping per-packet sequence on the lossy plane, with no FEC. Each
/// received sequence tells the decoder how many packets were missing immediately before it, so
/// it can run that many frames of libopus PLC (`decode` with empty input) first.
///
/// Reorders and duplicates conceal nothing (no reorder buffer). A gap is capped at
/// [`MAX_CONCEAL_MS`]; past that, libopus PLC has faded to silence and the ring underrun path
/// takes over.
#[derive(Debug)]
pub struct AudioGapTracker {
    last_seq: Option<u32>,
    /// One frame in microseconds; turns [`MAX_CONCEAL_MS`] into a packet cap. [`FRAME_MS`] until set.
    frame_us: u32,
}

impl Default for AudioGapTracker {
    fn default() -> Self {
        AudioGapTracker {
            last_seq: None,
            frame_us: FRAME_MS * 1000,
        }
    }
}

/// Longest gap one loss event conceals, in milliseconds — not a packet count, so a 2 ms lossless
/// frame still buys 50 ms. Same family as [`DroughtConceal::new_at_frame_us`].
///
/// Crate-internal: callers see [`AudioGapTracker::missing_before`]'s already-capped count.
/// Not part of the C ABI; cbindgen must not export this.
pub(crate) const MAX_CONCEAL_MS: u32 = 50;

/// [`MAX_CONCEAL_MS`] as a packet count at `frame_us`: 10 at 5 ms, 25 at 2 ms. Floors at 1 so a
/// zero cap cannot disable concealment.
///
/// Public so the C ABI's PCM decoder (`punktfunk-ffi`) can size its no-realloc buffer from the
/// same frame length. Buffer and cap must agree on how many frames can arrive at once.
pub const fn max_conceal_packets(frame_us: u32) -> u32 {
    let us = if frame_us == 0 {
        FRAME_MS * 1000
    } else {
        frame_us
    };
    let n = MAX_CONCEAL_MS * 1000 / us;
    if n == 0 {
        1
    } else {
        n
    }
}

impl AudioGapTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cap seq-gap PLC at 50 ms of `frame_us`. [`new`](Self::new) is the Opus 5 ms default.
    pub fn new_at_frame_us(frame_us: u32) -> Self {
        let mut t = Self::new();
        t.set_frame_us(frame_us);
        t
    }

    /// Frame length in microseconds — `audio_frame_us` on a lossless session, [`FRAME_MS`] on Opus.
    /// Known after `Welcome`; [`new_at_frame_us`](Self::new_at_frame_us) when the value is already in hand.
    pub fn set_frame_us(&mut self, frame_us: u32) {
        self.frame_us = frame_us.max(1);
    }

    /// Packets missing immediately before `seq` (`0` for in-order, first, duplicates, reorders),
    /// capped at [`MAX_CONCEAL_MS`]. A sequence in the backward half of u32 is a reorder, not a
    /// 2³¹ gap.
    pub fn missing_before(&mut self, seq: u32) -> u32 {
        let Some(last) = self.last_seq else {
            self.last_seq = Some(seq);
            return 0;
        };
        let delta = seq.wrapping_sub(last);
        if delta == 0 || delta > u32::MAX / 2 {
            return 0; // duplicate, or a reorder older than the newest
        }
        self.last_seq = Some(seq);
        (delta - 1).min(max_conceal_packets(self.frame_us))
    }
}

/// Drops audio a later packet already superseded. A reordered or duplicate datagram lands after
/// its slot was concealed or rebuilt from `0xD2`; decoding it plays that slot twice, out of order,
/// and feeds the decoder a stale packet.
#[derive(Debug, Default)]
pub struct AudioSeqGate {
    newest: Option<u32>,
}

impl AudioSeqGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` for the first packet and anything newer than the newest passed (wrap-aware).
    pub fn fresh(&mut self, seq: u32) -> bool {
        let fresh = self
            .newest
            .is_none_or(|n| (1..=u32::MAX / 2).contains(&seq.wrapping_sub(n)));
        if fresh {
            self.newest = Some(seq);
        }
        fresh
    }
}

/// Rebuilds the stream from the redundant `0xD2` plane so a single lost datagram is recovered,
/// not concealed.
///
/// Lives in core on the demux side so every embedder sees a complete stream and
/// [`AudioGapTracker`] stops seeing the gap. Only the immediately-preceding frame can be
/// recovered — that is all the wire carries ([`crate::quic::encode_audio_red_datagram`]). A
/// longer burst still conceals, one frame shorter.
#[derive(Debug, Default)]
pub struct AudioRedRecovery {
    last_seq: Option<u32>,
}

impl AudioRedRecovery {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `true` when the redundant copy should be emitted as `seq - 1` BEFORE this packet.
    /// Reorders, duplicates, and the first packet of a session recover nothing.
    pub fn recover_before(&mut self, seq: u32, has_prev: bool) -> bool {
        let recover = match self.last_seq {
            // First packet of the session: inserting the predecessor would prepend audio
            // the client never missed.
            None => false,
            Some(last) => {
                let delta = seq.wrapping_sub(last);
                // `delta == 1` is in-order. `delta >= 2` in the forward half means at least
                // the predecessor is missing.
                has_prev && (2..u32::MAX / 2).contains(&delta)
            }
        };
        self.last_seq = Some(match self.last_seq {
            // A reorder must not move the anchor backwards.
            Some(last) if seq.wrapping_sub(last) > u32::MAX / 2 => last,
            _ => seq,
        });
        recover
    }
}

// Drought thresholds (quiet-wire duration and ring-empty floor) are two protocol frames, so they
// live on `DroughtConceal` as `after()`/`floor_ms()` rather than constants. `2 * FRAME_MS` waits
// five frames on a 2 ms lossless frame.

/// Bounded concealment of a packet drought. Client-side twin of the host capture-hole infill
/// (`design/host-source-stutter-fixes.md`).
///
/// [`AudioGapTracker`] conceals a sequence gap once a later packet arrives. A quiet wire
/// reveals nothing: the ring drains, [`JitterPolicy::note_read`] de-primes, and the re-prime
/// is a whole target of silence. A drought that is draining the ring is concealed from the
/// same decoder state, bounded in time (never frames or callbacks). Time is passed in so the
/// policy stays syscall-free.
pub struct DroughtConceal {
    /// Frames concealed since the last real packet — what [`packet`](Self::packet) returns, and
    /// the unit that survives a non-5 ms frame. See [`new_at_frame_us`](Self::new_at_frame_us).
    concealed: u32,
    max_ms: u32,
    frame_us: u32,
    /// Session concealment, for the 10 s `plc_ms=` line. A policy that papers over a failing
    /// link must be visible.
    total: u64,
}

impl DroughtConceal {
    /// At the protocol's default frame ([`FRAME_MS`]).
    pub fn new(max_ms: u32) -> DroughtConceal {
        Self::new_at_frame_us(max_ms, FRAME_MS * 1000)
    }

    /// At an explicitly negotiated frame length. Charges one frame per concealed frame and bounds
    /// itself in wall-clock milliseconds, so the two have to agree on how long a frame is.
    pub fn new_at_frame_us(max_ms: u32, frame_us: u32) -> DroughtConceal {
        DroughtConceal {
            concealed: 0,
            max_ms,
            frame_us: frame_us.max(1),
            total: 0,
        }
    }

    /// How long a drought must last before it is concealed at all. Two frames, so an ordinary
    /// inter-packet gap is never mistaken for a stall.
    fn after(&self) -> std::time::Duration {
        std::time::Duration::from_micros(2 * self.frame_us as u64)
    }

    /// Ring depth below which a drought is worth concealing, in ms. A drought a deep ring can
    /// cover is not audible, and concealing it would synthesize audio the late packets are about
    /// to duplicate.
    fn floor_ms(&self) -> u32 {
        (2 * self.frame_us).div_ceil(1000)
    }

    /// A packet arrived. Returns frames already concealed so the caller can subtract them from
    /// [`AudioGapTracker`]: loss inside a covered drought must not be covered twice.
    pub fn packet(&mut self) -> u32 {
        std::mem::take(&mut self.concealed)
    }

    /// Conceal one more frame? `depth_ms` is the playout ring as the callback last saw it.
    pub fn conceal(&mut self, since_last_packet: std::time::Duration, depth_ms: u32) -> bool {
        if since_last_packet < self.after()
            || depth_ms > self.floor_ms()
            || self.concealed_ms() >= self.max_ms
        {
            return false;
        }
        self.concealed += 1;
        self.total += 1;
        true
    }

    fn concealed_ms(&self) -> u32 {
        (self.concealed as u64 * self.frame_us as u64 / 1000) as u32
    }

    pub fn total_ms(&self) -> u64 {
        self.total * self.frame_us as u64 / 1000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gap_tracker_counts_only_forward_gaps() {
        let mut t = AudioGapTracker::new();
        assert_eq!(t.missing_before(100), 0, "first packet");
        assert_eq!(t.missing_before(101), 0, "in order");
        assert_eq!(t.missing_before(104), 2, "102+103 lost");
        assert_eq!(t.missing_before(104), 0, "duplicate");
        assert_eq!(t.missing_before(103), 0, "late reorder conceals nothing");
        assert_eq!(t.missing_before(105), 0, "reorder didn't move the anchor");
        // A huge gap is capped; the stream continues from the new anchor.
        assert_eq!(t.missing_before(105 + 1000), 10);
        assert_eq!(t.missing_before(105 + 1001), 0);
    }

    /// The cap is 50 ms of audio, not ten packets. A short frame must still buy the
    /// milliseconds it says.
    #[test]
    fn the_conceal_cap_is_fifty_milliseconds_of_the_negotiated_frame() {
        // Opus: ten 5 ms frames.
        assert_eq!(max_conceal_packets(FRAME_MS * 1000), 10);
        let mut t = AudioGapTracker::new();
        t.missing_before(0);
        assert_eq!(t.missing_before(9_999), 10);

        // 2 ms lossless: twenty-five frames for the same 50 ms.
        assert_eq!(max_conceal_packets(2_000), 25);
        let mut p = AudioGapTracker::new_at_frame_us(2_000);
        p.missing_before(0);
        assert_eq!(p.missing_before(9_999), 25);

        for &us in &pcm::FRAME_US_LADDER {
            let n = max_conceal_packets(us);
            assert!(n >= 1, "{us} µs must conceal at least one frame");
            assert!(
                n as u64 * us as u64 <= MAX_CONCEAL_MS as u64 * 1000,
                "{us} µs × {n} frames exceeds the {MAX_CONCEAL_MS} ms this cap promises"
            );
        }
        assert_eq!(max_conceal_packets(0), 10, "0 µs falls back to the default");
        assert_eq!(max_conceal_packets(u32::MAX), 1);
    }

    #[test]
    fn gap_tracker_survives_seq_wraparound() {
        let mut t = AudioGapTracker::new();
        assert_eq!(t.missing_before(u32::MAX - 1), 0);
        assert_eq!(t.missing_before(u32::MAX), 0, "in order at the edge");
        assert_eq!(t.missing_before(1), 1, "seq 0 lost across the wrap");
        assert_eq!(t.missing_before(0), 0, "pre-wrap reorder, not a 2^31 gap");
    }

    // ---- redundant-plane recovery ---------------------------------------------------------

    #[test]
    fn a_superseded_audio_packet_is_dropped() {
        let mut g = AudioSeqGate::new();
        assert!(g.fresh(100));
        assert!(g.fresh(102), "a gap is concealed downstream");
        assert!(!g.fresh(101), "late: its slot was already concealed");
        assert!(!g.fresh(102), "duplicate");
        assert!(g.fresh(103));
        let mut w = AudioSeqGate::new();
        assert!(w.fresh(u32::MAX));
        assert!(w.fresh(0), "a wrap is newer");
    }

    #[test]
    fn red_recovery_rebuilds_exactly_the_single_missing_frame() {
        let mut r = AudioRedRecovery::new();
        assert!(!r.recover_before(10, true));
        assert!(!r.recover_before(11, true));
        // 12 lost: 13 carries it.
        assert!(r.recover_before(13, true));
        assert!(!r.recover_before(14, true));
    }

    #[test]
    fn red_recovery_is_conservative() {
        let mut r = AudioRedRecovery::new();
        r.recover_before(10, true);
        assert!(!r.recover_before(20, false));
        let mut r = AudioRedRecovery::new();
        r.recover_before(10, true);
        r.recover_before(11, true);
        assert!(!r.recover_before(11, true), "duplicate");
        assert!(!r.recover_before(9, true), "late reorder");
        assert!(
            !r.recover_before(12, true),
            "the reorder must not have moved the anchor"
        );
    }

    /// A longer burst still recovers its last frame. The remaining gap is one frame shorter.
    #[test]
    fn red_recovery_shortens_a_longer_burst() {
        let mut r = AudioRedRecovery::new();
        r.recover_before(100, true);
        assert!(
            r.recover_before(105, true),
            "104 is recoverable even though 101-103 are not"
        );
    }

    #[test]
    fn red_recovery_survives_seq_wraparound() {
        let mut r = AudioRedRecovery::new();
        assert!(!r.recover_before(u32::MAX - 1, true));
        assert!(
            !r.recover_before(u32::MAX, true),
            "in order across the edge"
        );
        assert!(r.recover_before(1, true), "seq 0 lost across the wrap");
        assert!(!r.recover_before(2, true));
    }

    /// Whatever `AudioRedRecovery` rebuilds, `AudioGapTracker` must then see as no gap. Recovery
    /// lives on the demux side so every embedder sees a complete stream.
    #[test]
    fn recovery_and_the_gap_tracker_agree() {
        let mut rec = AudioRedRecovery::new();
        let mut gaps = AudioGapTracker::new();
        let mut concealed = 0;
        // Deliver 0..20 with 7 and 13 lost; each survivor carries its predecessor.
        let mut emitted: Vec<u32> = Vec::new();
        for seq in (0..20u32).filter(|s| *s != 7 && *s != 13) {
            if rec.recover_before(seq, true) {
                emitted.push(seq - 1);
            }
            emitted.push(seq);
        }
        for seq in &emitted {
            concealed += gaps.missing_before(*seq);
        }
        assert_eq!(
            concealed, 0,
            "recovered stream must need no concealment: {emitted:?}"
        );
        assert_eq!(emitted.len(), 20, "every frame accounted for");
        assert!(
            emitted.windows(2).all(|w| w[1] == w[0] + 1),
            "and in order: {emitted:?}"
        );
    }

    // ---- drought concealment --------------------------------------------------------------

    fn drought_after() -> std::time::Duration {
        std::time::Duration::from_millis(2 * FRAME_MS as u64)
    }

    /// Concealment is for a ring that is running out. A drought a deep ring can cover is
    /// inaudible, and synthesizing over it would duplicate the late packets.
    #[test]
    fn a_drought_is_concealed_only_while_the_ring_is_running_out() {
        let mut c = DroughtConceal::new(JitterTuning::PIPEWIRE.plc_max_ms());
        let stalled = drought_after() + std::time::Duration::from_millis(FRAME_MS as u64);
        assert!(
            !c.conceal(stalled, 40),
            "a 40 ms ring covers this drought by itself"
        );
        assert!(c.conceal(stalled, 0), "an empty ring does not");
        assert_eq!(c.total_ms(), FRAME_MS as u64);
    }

    #[test]
    fn ordinary_jitter_is_not_a_drought() {
        let mut c = DroughtConceal::new(JitterTuning::AAUDIO.plc_max_ms());
        for _ in 0..1_000 {
            assert!(!c.conceal(std::time::Duration::from_millis(FRAME_MS as u64), 0));
        }
        assert_eq!(c.total_ms(), 0);
        assert_eq!(c.packet(), 0);
    }

    /// Every preset gets twice its own de-prime fuse, so no platform silently gets a third of
    /// another's protection.
    #[test]
    fn drought_concealment_is_bounded_at_twice_the_deprime_fuse() {
        for t in [
            JitterTuning::PIPEWIRE,
            JitterTuning::WASAPI,
            JitterTuning::COREAUDIO,
            JitterTuning::AAUDIO,
        ] {
            assert_eq!(t.plc_max_ms(), t.deprime_ms * 2);
            let mut c = DroughtConceal::new(t.plc_max_ms());
            let mut ms = 0u32;
            while c.conceal(drought_after(), 0) {
                ms += FRAME_MS;
                assert!(ms <= t.plc_max_ms(), "ran past the budget for {t:?}");
            }
            assert_eq!(ms, t.plc_max_ms(), "must use exactly the budget for {t:?}");
        }
    }

    /// Packets lost inside a drought already covered must not be covered a second time by the
    /// loss path: doing both would insert audio the stream never carried.
    #[test]
    fn concealment_already_paid_for_is_not_paid_for_twice() {
        let mut c = DroughtConceal::new(JitterTuning::WASAPI.plc_max_ms());
        for _ in 0..4 {
            assert!(c.conceal(drought_after(), 0));
        }
        let mut gaps = AudioGapTracker::new();
        gaps.missing_before(10);
        // Four frames concealed; the wire then reveals six were lost. Only two are still owed.
        let already = c.packet();
        assert_eq!(already, 4);
        assert_eq!(gaps.missing_before(17).saturating_sub(already), 2);
        assert!(c.conceal(drought_after(), 0));
    }

    /// The drought budget is wall-clock, spent one frame at a time, so the two have to agree
    /// about how long a frame is.
    #[test]
    fn the_drought_budget_is_spent_at_the_negotiated_frame_length() {
        // Opus: 100 ms of budget is twenty 5 ms frames, and the reported total agrees.
        let mut o = DroughtConceal::new(100);
        let mut n = 0;
        while o.conceal(drought_after(), 0) {
            n += 1;
        }
        assert_eq!(n, 20, "100 ms of 5 ms frames");
        assert_eq!(o.total_ms(), 100);
        assert_eq!(o.packet(), 20, "the caller is owed a FRAME count");

        // Same budget at 2 ms must buy the same wall clock: fifty frames, not twenty.
        let mut p = DroughtConceal::new_at_frame_us(100, 2_000);
        let mut m = 0;
        while p.conceal(std::time::Duration::from_millis(10), 0) {
            m += 1;
        }
        assert_eq!(m, 50, "100 ms of 2 ms frames");
        assert_eq!(p.total_ms(), 100, "plc_ms must not over-report");
        assert_eq!(p.packet(), 50);

        // A short frame also stops waiting five frames before it concedes a stall.
        let q = DroughtConceal::new_at_frame_us(100, 2_000);
        assert_eq!(q.after(), std::time::Duration::from_millis(4));
        assert_eq!(DroughtConceal::new(100).after(), drought_after());
    }
}
