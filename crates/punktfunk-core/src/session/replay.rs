//! Sliding-window anti-replay over the AEAD-authenticated packet number, applied on the
//! sealed receive path ([`Session::poll_frame`](super::Session::poll_frame)).

/// Sequences. Must cover [`LOSS_WINDOW_NS`](crate::packet) (120 ms) at line rate
/// or a late-but-usable shard dies here as "older than the window". 131072 is
/// 120 ms up to ~1.09M pkt/s (~12 Gbps wire); the bitmap is 16 KiB/session.
const REPLAY_WINDOW: u64 = 131072;
const REPLAY_WORDS: usize = (REPLAY_WINDOW / 64) as usize;

/// Bitmap of seen sequences, indexed `seq % WINDOW`. The protocol never
/// re-sends a sequence (FEC recovery shards get fresh ones); AEAD already
/// authenticates, so this rejects duplicates and anything older than the
/// window. In-window reorder is accepted.
pub(super) struct ReplayWindow {
    /// `seen` stays false until the first datagram (`highest` is then that seq, not 0).
    highest: u64,
    seen: bool,
    /// One bit per in-window sequence in `(highest - WINDOW, highest]`.
    bits: [u64; REPLAY_WORDS],
}

impl ReplayWindow {
    pub(super) fn new() -> ReplayWindow {
        ReplayWindow {
            highest: 0,
            seen: false,
            bits: [0; REPLAY_WORDS],
        }
    }

    #[inline]
    fn word_bit(seq: u64) -> (usize, u64) {
        let idx = (seq % REPLAY_WINDOW) as usize;
        (idx / 64, 1u64 << (idx % 64))
    }
    fn is_set(&self, seq: u64) -> bool {
        let (w, b) = Self::word_bit(seq);
        self.bits[w] & b != 0
    }
    fn set(&mut self, seq: u64) {
        let (w, b) = Self::word_bit(seq);
        self.bits[w] |= b;
    }

    /// Clear `from..to` (exclusive), word-wise. Callers guarantee
    /// `to - from < REPLAY_WINDOW` so the range cannot alias on the ring.
    /// Bit-at-a-time would run up to `WINDOW - 1` iterations per jump.
    fn clear_range(&mut self, from: u64, to: u64) {
        debug_assert!(to.saturating_sub(from) < REPLAY_WINDOW);
        let mut s = from;
        while s < to {
            let bit = (s % 64) as u32;
            let run = (64 - u64::from(bit)).min(to - s);
            let mask = if run == 64 {
                u64::MAX
            } else {
                ((1u64 << run) - 1) << bit
            };
            self.bits[Self::word_bit(s).0] &= !mask;
            s += run;
        }
    }

    /// The packet number a 32-bit one expands around: the highest accepted, 0 before any.
    pub(super) fn reference(&self) -> u64 {
        if self.seen {
            self.highest
        } else {
            0
        }
    }

    /// Worth an AEAD open: inside the window and at most one media key past the newest. An
    /// unauthenticated number further ahead would only make the receiver derive keys.
    pub(super) fn plausible(&self, seq: u64) -> bool {
        let key = crate::crypto::MEDIA_KEY_PACKETS;
        let reference = self.reference();
        seq / key <= reference / key + 1 && reference.saturating_sub(seq) < REPLAY_WINDOW
    }

    pub(super) fn accept(&mut self, seq: u64) -> bool {
        if !self.seen {
            self.seen = true;
            self.highest = seq;
            self.set(seq);
            return true;
        }
        if seq > self.highest {
            // Slid-in slots may still hold bits from a full window ago.
            if seq - self.highest >= REPLAY_WINDOW {
                self.bits = [0; REPLAY_WORDS];
            } else {
                self.clear_range(self.highest + 1, seq);
            }
            self.highest = seq;
            self.set(seq);
            true
        } else if self.highest - seq >= REPLAY_WINDOW || self.is_set(seq) {
            false
        } else {
            self.set(seq);
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_in_order_and_rejects_duplicates() {
        let mut w = ReplayWindow::new();
        for seq in 0..1000 {
            assert!(w.accept(seq), "fresh in-order seq {seq} must be accepted");
        }
        for seq in 0..1000 {
            assert!(!w.accept(seq), "replayed seq {seq} must be rejected");
        }
    }

    #[test]
    fn accepts_reorder_within_window_once() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(100));
        assert!(w.accept(80));
        assert!(!w.accept(80), "second copy of a reordered seq is a replay");
        assert!(w.accept(99));
        assert!(
            !w.accept(100),
            "the high-water seq itself can't be replayed"
        );
    }

    #[test]
    fn rejects_older_than_window() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(REPLAY_WINDOW * 2));
        assert!(!w.accept(REPLAY_WINDOW * 2 - REPLAY_WINDOW));
        assert!(!w.accept(0));
        assert!(w.accept(REPLAY_WINDOW * 2 - (REPLAY_WINDOW - 1)));
    }

    #[test]
    fn large_forward_jump_wipes_stale_bits() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(5));
        let far = 10 * REPLAY_WINDOW + 5;
        assert!(w.accept(far));
        assert!(
            !w.accept(5),
            "the pre-jump seq is now far older than the window"
        );
        assert!(w.accept(far - REPLAY_WINDOW + 1));
    }

    /// In-window jump: slid-in slots clear, bits below the jump stay set.
    #[test]
    fn large_in_window_jump_clears_word_wise() {
        let mut w = ReplayWindow::new();
        for seq in [0u64, 1, 63, 64, 100, 4000] {
            assert!(w.accept(seq));
        }
        let hi = REPLAY_WINDOW - 1;
        assert!(w.accept(hi));
        assert!(w.accept(50_000));
        assert!(!w.accept(50_000), "…and now a replay");
        for seq in [1u64, 63, 64, 100, 4000] {
            assert!(!w.accept(seq), "seeded seq {seq} must still read as seen");
        }
        assert!(!w.accept(hi));
    }

    #[test]
    fn first_seq_need_not_be_zero() {
        // Startup loss: the first opened datagram need not be seq 0.
        let mut w = ReplayWindow::new();
        assert!(w.accept(42));
        assert!(!w.accept(42));
        assert!(w.accept(43));
    }
}
