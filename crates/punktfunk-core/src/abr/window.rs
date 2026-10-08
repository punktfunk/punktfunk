//! Assembling one report window from what the session did.
//!
//! Counter anchors (loss, lost frames, wire bytes) are differenced against the
//! snapshot the last window closed on, so a window is a delta, not a level.
//! The latency signals arrive as sums and counts because that is what the
//! embedders gather. A window whose numbers describe something other than the
//! link — the tail of a capacity burst, a host pipeline rebuild — is
//! discarded whole: one bogus congestion verdict ends slow start for good.
//! A burst's freeze outlives that window, so its keyframe asks are disowned
//! for as long as it lasts.

use super::sample::{DelayTrend, WindowActivity, WindowSample, WINDOW};
use crate::stats::Stats;
use std::time::Instant;

/// Windows after a capacity burst whose keyframe asks still belong to it.
/// 8 × 750 ms = 6 s: a freeze's asks (one per 100 ms on webOS) plus a lost
/// IDR's retry.
pub(super) const PROBE_AFTERMATH_WINDOWS: u32 = 8;

/// A closed report window: the controller's input, plus what the embedder
/// still owes the host for it.
pub(crate) struct Closed {
    pub sample: WindowSample,
    /// The window describes a burst tail or a rebuild, not the link. No
    /// report goes out and the controller never sees it.
    pub discarded: bool,
    /// Session total of media packets received, for the window's report.
    pub packets_received: u64,
}

/// What the pump measures between report ticks.
pub(crate) struct WindowAccumulator {
    /// Audio's wire reservation, spent whether video flows or not. Added so
    /// the delivered rate lives in the same budget as the target.
    audio_reserved_kbps: u32,
    /// Host marks idle-keepalive repeats. Older hosts are
    /// [`WindowActivity::Unmarked`].
    marks_repeats: bool,
    last_report: Instant,
    /// Counter anchors: this window is the session totals minus these.
    recovered: u64,
    late: u64,
    received: u64,
    dropped: u64,
    bytes: u64,
    /// Loss by position drained this window, and the socket's drops at its start.
    positions: crate::packet::LossPositions,
    sock_drops_at: u64,
    /// The socket's drops as last sampled ([`Self::on_sock_drops`]).
    sock_drops: u64,
    /// Latest session snapshot. The pump samples once per iteration and every
    /// window number is differenced from it.
    stats: Stats,
    owd_sum_ns: i128,
    owd_frames: u32,
    /// First-shard delay, µs: the count, the sum, and `Σ(ordinal × sample)` —
    /// the least-squares fit needs no more than that, and none of it grows
    /// with the window.
    shard_owd_count: u32,
    shard_owd_sum_us: i64,
    shard_owd_xy_us: i64,
    shard_owd_last_us: i64,
    au_frames: u32,
    au_repeats: u32,
    decode_sum_us: u64,
    decode_count: u32,
    encode_sum_us: u64,
    encode_count: u32,
    recovery_kf: u32,
    /// Windows whose keyframe asks are still the burst's ([`probe_aftermath`]).
    aftermath_left: u32,
    flushed: bool,
    discard: bool,
}

impl WindowAccumulator {
    pub(crate) fn new(audio_reserved_kbps: u32, marks_repeats: bool, now: Instant) -> Self {
        WindowAccumulator {
            audio_reserved_kbps,
            marks_repeats,
            last_report: now,
            recovered: 0,
            late: 0,
            received: 0,
            dropped: 0,
            bytes: 0,
            positions: Default::default(),
            sock_drops_at: 0,
            sock_drops: 0,
            stats: Stats::default(),
            owd_sum_ns: 0,
            owd_frames: 0,
            shard_owd_count: 0,
            shard_owd_sum_us: 0,
            shard_owd_xy_us: 0,
            shard_owd_last_us: 0,
            au_frames: 0,
            au_repeats: 0,
            decode_sum_us: 0,
            decode_count: 0,
            encode_sum_us: 0,
            encode_count: 0,
            recovery_kf: 0,
            aftermath_left: 0,
            flushed: false,
            discard: false,
        }
    }

    pub(crate) fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The session counters as of now. Every window number is a delta of two
    /// of these, so the pump feeds one per iteration — a total-loss drought
    /// completes no frame but still moves them.
    pub(crate) fn on_stats(&mut self, st: &Stats) {
        self.stats = *st;
    }

    /// The data socket's drops since it opened, as the pump samples them.
    pub(crate) fn on_sock_drops(&mut self, total: u64) {
        self.sock_drops = total;
    }

    /// Loss by position the reassembler settled since the last drain.
    pub(crate) fn on_loss_positions(&mut self, p: crate::packet::LossPositions) {
        let q = &mut self.positions;
        (q.head, q.mid, q.tail) = (
            q.head.saturating_add(p.head),
            q.mid.saturating_add(p.mid),
            q.tail.saturating_add(p.tail),
        );
    }

    /// One completed access unit. `repeat` is the host's idle keepalive mark:
    /// an arrived AU that carries no new content.
    pub(crate) fn on_au(&mut self, repeat: bool) {
        self.au_frames = self.au_frames.saturating_add(1);
        if repeat {
            self.au_repeats = self.au_repeats.saturating_add(1);
        }
    }

    /// Capture → received for one AU. Rising delay under zero loss is queue
    /// growth, the signal that arrives before the loss does.
    pub(crate) fn on_owd(&mut self, ns: i128) {
        self.owd_sum_ns += ns;
        self.owd_frames += 1;
    }

    /// Capture → first-shard arrival for one frame. Frames that never complete
    /// have one of these and nothing else, so this is the delay reading a
    /// cascade cannot silence. `x` is the sample's ordinal: the trend is a fit
    /// over the window's own order, never against a remembered floor.
    pub(crate) fn on_shard_owd(&mut self, ns: i128) {
        let us = (ns / 1_000) as i64;
        self.shard_owd_xy_us = self
            .shard_owd_xy_us
            .saturating_add(i64::from(self.shard_owd_count).saturating_mul(us));
        self.shard_owd_sum_us = self.shard_owd_sum_us.saturating_add(us);
        self.shard_owd_last_us = us;
        self.shard_owd_count += 1;
    }

    /// The window's client decode-stage accumulator, however the embedder
    /// gathered it. Absent (`count` 0) means nobody reports it.
    pub(crate) fn on_decode_latency(&mut self, sum_us: u64, count: u32) {
        self.decode_sum_us += sum_us;
        self.decode_count += count;
    }

    /// Host encode-stage timings from the `0xCF` stage report.
    pub(crate) fn on_encode_latency(&mut self, sum_us: u64, count: u32) {
        self.encode_sum_us += sum_us;
        self.encode_count += count;
    }

    /// Decode-recovery keyframe asks that went out this window.
    pub(crate) fn on_keyframe_asks(&mut self, n: u32) {
        self.recovery_kf = self.recovery_kf.saturating_add(n);
    }

    /// A jump-to-live: the client could not hold the rate. Severe.
    pub(crate) fn on_flush(&mut self) {
        self.flushed = true;
    }

    /// This window describes something other than the link.
    pub(crate) fn discard(&mut self) {
        self.discard = true;
    }

    /// How long this window has been open, ms — for the log that says why it
    /// is being thrown away.
    pub(crate) fn open_ms(&self) -> u64 {
        self.last_report.elapsed().as_millis() as u64
    }

    /// Past the report cadence, and no burst is in flight to distort it.
    pub(crate) fn due(&self, now: Instant, probing: bool) -> bool {
        !probing && now.duration_since(self.last_report) >= WINDOW
    }

    /// Every anchor forward to now, dropping what the window held, and the
    /// burst's aftermath armed.
    ///
    /// The capacity burst lands in the packet and byte counters but never in
    /// the decoder, so without this the first window after it reads as a
    /// throughput that never happened. The freeze it can leave behind outlives
    /// this one window, which is what [`probe_aftermath`] covers.
    pub(crate) fn rebase(&mut self, now: Instant) {
        let st = self.stats;
        self.recovered = st.fec_recovered_shards;
        self.late = st.fec_late_shards;
        self.received = st.packets_received;
        self.dropped = st.frames_dropped;
        self.bytes = wire_bytes(&st);
        self.positions = Default::default();
        self.sock_drops_at = self.sock_drops;
        self.last_report = now;
        self.discard = true;
        self.aftermath_left = PROBE_AFTERMATH_WINDOWS;
        self.flushed = false;
    }

    /// The byte anchor alone. Video that landed under a suppressed report tick
    /// would otherwise be one window's worth of a much longer span.
    pub(crate) fn rebase_bytes(&mut self) {
        self.bytes = wire_bytes(&self.stats);
    }

    /// Close the window: the sample the controller judges, and what the host
    /// is owed for it.
    pub(crate) fn close(&mut self, now: Instant) -> Closed {
        let st = self.stats;
        let discarded = std::mem::take(&mut self.discard);
        let dropped = st.frames_dropped.wrapping_sub(self.dropped);
        let loss_ppm = crate::quic::window_loss_ppm(
            st.fec_recovered_shards.wrapping_sub(self.recovered),
            st.fec_late_shards.wrapping_sub(self.late),
            st.packets_received.wrapping_sub(self.received),
        );
        // Wire throughput vs target: headers, seals and FEC parity included
        // (they spend the budget), minus probe filler, plus the audio
        // reservation.
        let window_ms = now.duration_since(self.last_report).as_millis().max(1) as u64;
        let actual_kbps = ((wire_bytes(&st).wrapping_sub(self.bytes).saturating_mul(8) / window_ms)
            as u32)
            .saturating_add(self.audio_reserved_kbps);
        let mean = |sum: u64, count: u32| (count > 0).then(|| (sum / u64::from(count)) as i64);
        // The forced tail window neither spends nor ends the aftermath: its
        // asks may not have surfaced yet.
        let recovery_kf =
            if !discarded && probe_aftermath(&mut self.aftermath_left, self.recovery_kf > 0) {
                tracing::debug!(
                    recovery_kf = self.recovery_kf,
                    "keyframe asks in the capacity burst's aftermath — not judged as congestion"
                );
                0
            } else {
                self.recovery_kf
            };
        let sample = WindowSample {
            now,
            dropped,
            loss_ppm,
            owd_mean_us: (self.owd_frames > 0)
                .then(|| (self.owd_sum_ns / i128::from(self.owd_frames) / 1_000) as i64),
            delay: (self.shard_owd_count > 0).then(|| DelayTrend {
                samples: self.shard_owd_count,
                mean_us: self.shard_owd_sum_us / i64::from(self.shard_owd_count),
                rise_us: fitted_rise_us(
                    self.shard_owd_count,
                    self.shard_owd_sum_us,
                    self.shard_owd_xy_us,
                ),
                last_us: self.shard_owd_last_us,
            }),
            decode_mean_us: mean(self.decode_sum_us, self.decode_count),
            encode_mean_us: mean(self.encode_sum_us, self.encode_count),
            actual_kbps,
            flushed: self.flushed,
            recovery_kf,
            activity: activity(self.marks_repeats, self.au_frames, self.au_repeats),
            head: self.positions.head,
            mid: self.positions.mid,
            tail: self.positions.tail,
            sock_drops: delta(self.sock_drops, self.sock_drops_at),
        };
        self.reset(now);
        Closed {
            sample,
            discarded,
            packets_received: st.packets_received,
        }
    }

    /// Anchors forward, accumulators empty. A discarded window's counts must
    /// not leak into the next one.
    fn reset(&mut self, now: Instant) {
        let st = self.stats;
        self.recovered = st.fec_recovered_shards;
        self.late = st.fec_late_shards;
        self.received = st.packets_received;
        self.dropped = st.frames_dropped;
        self.bytes = wire_bytes(&st);
        self.positions = Default::default();
        self.sock_drops_at = self.sock_drops;
        self.last_report = now;
        self.owd_sum_ns = 0;
        self.owd_frames = 0;
        self.shard_owd_count = 0;
        self.shard_owd_sum_us = 0;
        self.shard_owd_xy_us = 0;
        self.shard_owd_last_us = 0;
        self.au_frames = 0;
        self.au_repeats = 0;
        self.decode_sum_us = 0;
        self.decode_count = 0;
        self.encode_sum_us = 0;
        self.encode_count = 0;
        self.recovery_kf = 0;
        self.flushed = false;
    }
}

/// This window's new-content evidence.
///
/// No arrivals are [`WindowActivity::Empty`]: quiet like idle, not an
/// older-host unmarked window. Repeat-only (every arrived AU a host-marked
/// repeat) is [`WindowActivity::Active`]`(0)` and counts toward re-arm.
/// Arrivals on a host that does not mark repeats are
/// [`WindowActivity::Unmarked`]. Stillness is never inferred from a blackout.
fn activity(marks_repeats: bool, frames: u32, repeats: u32) -> WindowActivity {
    if frames == 0 {
        WindowActivity::Empty
    } else if marks_repeats {
        WindowActivity::Active(frames.saturating_sub(repeats))
    } else {
        WindowActivity::Unmarked
    }
}

/// Whether this window's keyframe asks still belong to the capacity burst.
///
/// The burst runs beside live video, so a link it overdrives drops frames too
/// and the client asks for keyframes until one lands — past the one discarded
/// tail window. Two asks in a judged window end slow start and four cut the
/// rate, yet they say nothing about the link after the burst. Only the asks
/// are disowned: drops, loss and a flush in the same window are still judged,
/// which is what keeps a link the start rate overloads from hiding here. Each
/// window with asks spends one of `windows_left`; the first without ends it.
fn probe_aftermath(windows_left: &mut u32, asked: bool) -> bool {
    if *windows_left == 0 {
        return false;
    }
    if asked {
        *windows_left -= 1;
    } else {
        *windows_left = 0;
    }
    asked
}

/// Least-squares delay rise across one window, µs: the fitted line's first
/// sample to its last.
///
/// `x` is the sample ordinal, so `Σx` and `Σx²` are closed forms of `n` and
/// the window only has to carry `Σy` and `Σxy`. Fewer than two samples is no
/// line. i128 because `n·Σxy` outgrows i64 on a long window at a deep queue.
fn fitted_rise_us(n: u32, sum_us: i64, xy_us: i64) -> i64 {
    if n < 2 {
        return 0;
    }
    let n = i128::from(n);
    let sx = n * (n - 1) / 2;
    let sxx = (n - 1) * n * (2 * n - 1) / 6;
    let den = n * sxx - sx * sx;
    if den == 0 {
        return 0;
    }
    ((i128::from(xy_us) * n - sx * i128::from(sum_us)) * (n - 1) / den) as i64
}

/// A counter's growth since `anchor`, as a window field.
fn delta(now: u64, anchor: u64) -> u32 {
    now.saturating_sub(anchor).min(u64::from(u32::MAX)) as u32
}

/// Wire measure: every received media-plane byte (headers, seals and FEC
/// parity spend the budget) minus speed-test filler.
fn wire_bytes(st: &Stats) -> u64 {
    st.bytes_received.wrapping_sub(st.probe_bytes_received)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The burst's keyframe asks are disowned until a window has none, never
    /// past the budget.
    #[test]
    fn probe_keyframe_asks_are_disowned_until_the_stream_recovers() {
        let mut left = PROBE_AFTERMATH_WINDOWS;
        assert!(probe_aftermath(&mut left, true));
        assert!(probe_aftermath(&mut left, true));
        assert!(!probe_aftermath(&mut left, false), "no asks ends it");
        assert!(
            !probe_aftermath(&mut left, true),
            "asks after recovery are congestion"
        );

        let mut left = 2;
        assert!(probe_aftermath(&mut left, true));
        assert!(probe_aftermath(&mut left, true));
        assert!(
            !probe_aftermath(&mut left, true),
            "a client that never recovers is judged once the budget is spent"
        );
    }

    #[test]
    fn only_observed_repeats_make_an_idle_abr_window() {
        assert_eq!(activity(true, 45, 45), WindowActivity::Active(0));
        assert_eq!(activity(true, 45, 40), WindowActivity::Active(5));
        assert_eq!(activity(true, 0, 0), WindowActivity::Empty);
        assert_eq!(activity(false, 45, 0), WindowActivity::Unmarked);
        assert_eq!(activity(false, 0, 0), WindowActivity::Empty);
    }
}
