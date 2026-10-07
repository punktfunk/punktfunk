//! Shared packet scheduling for native and GameStream video sends.
//!
//! Both planes microburst, then pace overflow against a bitrate-derived budget;
//! sockets and framing stay with the plane. Native coarsens chunks so sleep
//! intervals stay above the scheduler floor; GameStream bounds sleep-step count
//! on its non-realtime thread. Tests pin both schedules.
//!
//! `PUNKTFUNK_VIDEO_DROP` and the percentile helper live here too.

use std::time::{Duration, Instant};

/// Native feeds this to the PUNKTFUNK_PERF histogram (pacing tail per frame).
pub(crate) struct PaceStat {
    pub(crate) spread_us: u32,
    pub(crate) paced: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ChunkPolicy {
    /// Native: 16-packet chunks; step count scales with the frame.
    Fixed(usize),
    /// Coarsen from `base` so `budget / steps` stays ≥ the sleep floor, capped
    /// at `max` (64-segment GSO). Zero budget takes `max` (fewest syscalls).
    /// Without this, high rates skip every sub-floor wait and go out unpaced.
    Adaptive { base: usize, max: usize },
    /// `chunk = max(min_chunk, ceil(n / max_steps))` (GameStream: 16 / 12).
    /// Caps per-frame sleep overshoot independent of bitrate.
    Bounded { min_chunk: usize, max_steps: usize },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PaceBudget {
    /// `min((deadline − now-after-burst) × fraction, cap)`. Zero slack → 0.
    /// Native: fraction 0.9. `cap` is wire time at a rate the link already
    /// carries; `Duration::MAX` leaves the deadline term uncapped.
    UntilDeadline {
        deadline: Instant,
        fraction: f32,
        cap: Duration,
    },
    /// Precomputed spread (GameStream: ¾ of the frame interval; native:
    /// [`native_budget`]).
    Fixed(Duration),
}

/// Ceiling on one native frame's paced spread. Past this the send thread is
/// parked too long; the tail is late but still delivered whole.
pub(crate) const MAX_PACE_SPREAD: Duration = Duration::from_millis(100);

/// Native pace budget for one frame. With a pace rate, overflow spreads across
/// its wire time at that rate, bounded by [`MAX_PACE_SPREAD`] and `max_spread`
/// — never under-cut by the frame deadline. Cutting an oversized frame into the
/// remainder of one interval blasts it and overruns the tx buffer.
///
/// `pace_rate_bps == 0` (`PUNKTFUNK_PACE_FACTOR=0`) or no overflow keeps the
/// deadline-only spread (`UntilDeadline` 0.9, uncapped).
///
/// `max_spread` is what encode|send `sync_channel(3)` can absorb (~2 frame
/// intervals). A longer stall backs the channel into `cadence_degraded` and
/// the host refuses every ABR climb. Pass [`MAX_PACE_SPREAD`] when unknown.
pub(crate) fn native_budget(
    deadline: Instant,
    pace_rate_bps: u64,
    overflow_bytes: u64,
    max_spread: Duration,
) -> PaceBudget {
    if pace_rate_bps > 0 && overflow_bytes > 0 {
        let cap = Duration::from_nanos(
            (overflow_bytes * 8).saturating_mul(1_000_000_000) / pace_rate_bps,
        );
        PaceBudget::Fixed(cap.min(max_spread).min(MAX_PACE_SPREAD))
    } else {
        PaceBudget::UntilDeadline {
            deadline,
            fraction: 0.9,
            cap: Duration::MAX,
        }
    }
}

/// Smallest microburst: one GSO super-packet. [`DeliveryProfile::Smooth`]'s whole allowance.
pub(crate) const BURST_MIN: usize = 16 * 1024;

/// Native microburst: bytes that leave unpaced, sized as 10 ms at the pace
/// rate, clamped to [16 KiB, 256 KiB]. An absolute floor (128 KiB) swallows
/// whole frames at Wi-Fi rates, so the train goes out unpaced. 5 Mbps stream
/// (~15 Mbps pace) → ~19 KiB; 30 Mbps LAN (~90 Mbps) → ~112 KiB; ≥205 Mbps
/// clamps at 256 KiB. `pace_rate_bps == 0` keeps `max(128 KiB, wire/4)`.
pub(crate) fn auto_burst_bytes(pace_rate_bps: u64, wire_bytes: usize) -> usize {
    const BURST_MS: u64 = 10;
    const BURST_MAX: usize = 256 * 1024;
    if pace_rate_bps == 0 {
        return (wire_bytes / 4).max(128 * 1024);
    }
    usize::try_from(pace_rate_bps * BURST_MS / 8000)
        .unwrap_or(BURST_MAX)
        .clamp(BURST_MIN, BURST_MAX)
}

/// How one session's packets leave the socket. A client asks for one per session;
/// [`forced_delivery`] pins one for every session. Nothing here picks one on its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum DeliveryProfile {
    /// The microburst leaves at line rate; only overflow is paced.
    #[default]
    Burst = 0,
    /// The microburst leaves on a [`GroupClock`] at `max(CAP_FLOOR_BPS, pace rate)`, so a
    /// host whose port is faster than the client's cannot fill the switch queue between.
    Capped = 1,
    /// The allowance is one super-packet ([`BURST_MIN`]); the rest spreads at the pace
    /// rate. About a third of a frame interval of extra latency, for a receiver that
    /// loses the head of every line-rate burst.
    Smooth = 2,
}

impl DeliveryProfile {
    /// The floor of `Capped`'s clock: a fifth under 1 GbE.
    pub(crate) const CAP_FLOOR_BPS: u64 = 800_000_000;

    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => DeliveryProfile::Capped,
            2 => DeliveryProfile::Smooth,
            _ => DeliveryProfile::Burst,
        }
    }

    /// Bytes of one AU that leave unpaced at `pace_rate_bps`.
    pub(crate) fn burst_bytes(self, pace_rate_bps: u64, wire_bytes: usize) -> usize {
        match self {
            DeliveryProfile::Smooth => BURST_MIN,
            _ => auto_burst_bytes(pace_rate_bps, wire_bytes),
        }
    }

    /// The rate `Capped` holds the microburst to; `None` for the others.
    pub(crate) fn cap_bps(self, pace_rate_bps: u64) -> Option<u64> {
        (self == DeliveryProfile::Capped).then(|| pace_rate_bps.max(Self::CAP_FLOOR_BPS))
    }
}

/// `PUNKTFUNK_DELIVERY=burst|capped|smooth`, read once: pins a profile for every session of
/// both planes, over whatever a client asks. Unset leaves the choice to the client.
pub(crate) fn forced_delivery() -> Option<DeliveryProfile> {
    static FORCED: std::sync::OnceLock<Option<DeliveryProfile>> = std::sync::OnceLock::new();
    *FORCED.get_or_init(|| {
        let s = pf_host_config::knob("PUNKTFUNK_DELIVERY")?;
        let profile = parse_delivery(&s);
        match profile {
            Some(p) => tracing::info!(profile = ?p, "PUNKTFUNK_DELIVERY pins the delivery profile"),
            None => tracing::warn!(
                value = %s,
                "PUNKTFUNK_DELIVERY not recognised — burst, capped or smooth"
            ),
        }
        profile
    })
}

fn parse_delivery(s: &str) -> Option<DeliveryProfile> {
    match s.trim().to_ascii_lowercase().as_str() {
        "burst" => Some(DeliveryProfile::Burst),
        "capped" => Some(DeliveryProfile::Capped),
        "smooth" => Some(DeliveryProfile::Smooth),
        _ => None,
    }
}

/// Line-rate cap on a microburst: bytes leave on a clock at `rate_bps`, carried across
/// AUs so back-to-back frames cannot add up to a blast. A clock in the past restarts at
/// `now`: a late frame is owed no catch-up.
#[derive(Debug)]
pub(crate) struct GroupClock {
    rate_bps: u64,
    next: Instant,
}

impl GroupClock {
    pub(crate) fn new(rate_bps: u64, now: Instant) -> Self {
        GroupClock {
            rate_bps: rate_bps.max(1),
            next: now,
        }
    }

    pub(crate) fn set_rate(&mut self, rate_bps: u64) {
        self.rate_bps = rate_bps.max(1);
    }

    /// The wait owed before `bytes` may leave; the clock advances by their wire time.
    /// A sub-floor wait the caller skips stays owed, so the rate holds over a few groups.
    pub(crate) fn wait(&mut self, bytes: usize, now: Instant) -> Duration {
        let ahead = self.next.saturating_duration_since(now);
        let start = if ahead.is_zero() { now } else { self.next };
        self.next = start
            + Duration::from_nanos((bytes as u64).saturating_mul(8_000_000_000) / self.rate_bps);
        ahead
    }
}

/// One session's pacing inputs, resolved per AU by its send loop.
pub(crate) struct Pacing {
    /// `PUNKTFUNK_PACE_BURST_KB`: a pinned allowance over the profile's.
    pub(crate) burst_cap: Option<usize>,
    pub(crate) pace_rate_bps: u64,
    pub(crate) max_spread: Duration,
    pub(crate) profile: DeliveryProfile,
    /// `Capped`'s clock, carried across AUs; `None` under the other profiles.
    pub(crate) clock: Option<GroupClock>,
}

impl Pacing {
    pub(crate) fn new(burst_cap: Option<usize>) -> Self {
        Pacing {
            burst_cap,
            pace_rate_bps: 0,
            max_spread: MAX_PACE_SPREAD,
            profile: DeliveryProfile::Burst,
            clock: None,
        }
    }

    /// Per AU: the live rate, the spread bound and the profile. The clock follows the
    /// profile and keeps its phase across rate changes.
    pub(crate) fn update(
        &mut self,
        pace_rate_bps: u64,
        max_spread: Duration,
        profile: DeliveryProfile,
    ) {
        self.pace_rate_bps = pace_rate_bps;
        self.max_spread = max_spread;
        self.profile = profile;
        match profile.cap_bps(pace_rate_bps) {
            Some(rate) => self
                .clock
                .get_or_insert_with(|| GroupClock::new(rate, Instant::now()))
                .set_rate(rate),
            None => self.clock = None,
        }
    }

    /// Unpaced bytes of an AU of `wire_bytes`.
    pub(crate) fn burst_bytes(&self, wire_bytes: usize) -> usize {
        self.burst_cap
            .unwrap_or_else(|| self.profile.burst_bytes(self.pace_rate_bps, wire_bytes))
    }

    /// Bytes per paced group. `Smooth` bounds a group to its allowance, or half a
    /// millisecond at the rate, so the first paced group cannot re-grow the burst; the
    /// others keep the caller's packet-count group.
    pub(crate) fn group_bytes(&self) -> usize {
        match self.profile {
            DeliveryProfile::Smooth => BURST_MIN.max((self.pace_rate_bps / 16_000) as usize),
            _ => usize::MAX,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PaceCfg {
    /// Bytes that leave immediately before pacing; `None` = no burst (GameStream).
    /// `Some(0)` still bursts the first packet — the packet that crosses the cap
    /// goes with the burst.
    pub(crate) burst_bytes: Option<usize>,
    pub(crate) chunk: ChunkPolicy,
    /// Sleeps shorter than this are skipped (scheduler-jitter floor; both planes: 500 µs).
    pub(crate) sleep_floor: Duration,
}

/// Burst `[0..burst_len)` immediately in `chunk`-sized groups; overflow in
/// `steps` chunks, chunk `j` sleeping toward `budget × (j+1)/steps`.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PaceSchedule {
    pub(crate) burst_len: usize,
    pub(crate) chunk: usize,
    pub(crate) steps: usize,
}

/// Only [`ChunkPolicy::Adaptive`] reads `pace_budget`; `Fixed`/`Bounded` ignore it.
pub(crate) fn schedule<T: AsRef<[u8]>>(
    packets: &[T],
    cfg: &PaceCfg,
    pace_budget: Duration,
) -> PaceSchedule {
    let burst_len = match cfg.burst_bytes {
        None => 0,
        Some(cap) => {
            // The packet that crosses the cap still bursts (`split = k + 1`);
            // the whole frame bursts when it never crosses.
            let mut cum = 0usize;
            let mut split = packets.len();
            for (k, p) in packets.iter().enumerate() {
                cum += p.as_ref().len();
                if cum >= cap {
                    split = k + 1;
                    break;
                }
            }
            split
        }
    };
    let overflow = packets.len() - burst_len;
    let (chunk, steps) = match cfg.chunk {
        ChunkPolicy::Fixed(c) => (c, overflow.div_ceil(c).max(1)),
        ChunkPolicy::Adaptive { base, max } => {
            let c = if overflow == 0 {
                base
            } else if pace_budget.is_zero() {
                max
            } else {
                // interval = budget/steps ≈ budget·c/overflow ≥ sleep_floor ⇔
                // c ≥ overflow·floor/budget — smallest such c, clamped to [base, max].
                let c_min = (overflow as u128 * cfg.sleep_floor.as_nanos())
                    .div_ceil(pace_budget.as_nanos());
                c_min.clamp(base as u128, max as u128) as usize
            };
            (c, overflow.div_ceil(c).max(1))
        }
        ChunkPolicy::Bounded {
            min_chunk,
            max_steps,
        } => {
            let c = min_chunk.max(overflow.div_ceil(max_steps));
            (c, overflow.div_ceil(c).max(1))
        }
    };
    PaceSchedule {
        burst_len,
        chunk,
        steps,
    }
}

/// Burst, then sleep each overflow chunk toward its slice of the budget
/// (sub-`sleep_floor` waits skipped). With a `clock` the burst's chunks wait on it
/// instead of leaving at line rate ([`DeliveryProfile::Capped`]). A `send` error
/// aborts the frame — native bails the session, GameStream stops the stream.
pub(crate) fn pace_frame<T: AsRef<[u8]>, E>(
    packets: &[T],
    budget: PaceBudget,
    cfg: &PaceCfg,
    mut clock: Option<&mut GroupClock>,
    mut send: impl FnMut(&[T]) -> Result<(), E>,
) -> Result<PaceStat, E> {
    let start = Instant::now();
    // Adaptive chunk sizing needs the budget before the burst leaves. The
    // paced loop re-anchors at `pace_start`; this overshoots by the burst's
    // few µs — sub-floor, skipped.
    let budget_est = match budget {
        PaceBudget::UntilDeadline {
            deadline,
            fraction,
            cap,
        } => deadline
            .checked_duration_since(start)
            .unwrap_or_default()
            .mul_f32(fraction)
            .min(cap),
        PaceBudget::Fixed(d) => d,
    };
    let sched = schedule(packets, cfg, budget_est);
    for chunk in packets[..sched.burst_len].chunks(sched.chunk) {
        if let Some(clock) = clock.as_deref_mut() {
            let bytes: usize = chunk.iter().map(|p| p.as_ref().len()).sum();
            let ahead = clock.wait(bytes, Instant::now());
            if ahead >= cfg.sleep_floor {
                std::thread::sleep(ahead);
            }
        }
        send(chunk)?;
    }
    let paced = sched.burst_len < packets.len();
    if paced {
        let pace_start = Instant::now();
        let budget = match budget {
            PaceBudget::UntilDeadline {
                deadline,
                fraction,
                cap,
            } => deadline
                .checked_duration_since(pace_start)
                .unwrap_or_default()
                .mul_f32(fraction)
                .min(cap),
            PaceBudget::Fixed(d) => d,
        };
        for (j, chunk) in packets[sched.burst_len..].chunks(sched.chunk).enumerate() {
            send(chunk)?;
            let target = pace_start + budget.mul_f64((j + 1) as f64 / sched.steps as f64);
            if let Some(ahead) = target.checked_duration_since(Instant::now()) {
                if ahead >= cfg.sleep_floor {
                    std::thread::sleep(ahead);
                }
            }
        }
    }
    Ok(PaceStat {
        spread_us: start.elapsed().as_micros() as u32,
        paced,
    })
}

/// `PUNKTFUNK_FRAME_DRIVEN=0` restores the fixed-cadence tick. On, the loop wakes on
/// the capturer's arrival ([`pf_capture::Capturer::supports_arrival_wait`]) or on the
/// next access unit of an encoder that publishes its own; a backend with neither
/// keeps the tick. Shared by both video planes.
pub(crate) fn frame_driven_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| pf_host_config::env_on("PUNKTFUNK_FRAME_DRIVEN").unwrap_or(true))
}

/// Wire-rate credit bucket for arrival-wait capture, shared by both planes.
///
/// Floor-only (0.9 × interval) lets a source that always has a frame pending
/// settle at 1.11× the negotiated rate. Credit accrues at one frame per
/// interval (capped at [`Self::CAP`]); a grab may run early only against
/// banked credit, so per-gap jitter still passes while the average cannot
/// exceed the pacing rate. At or below that rate, credit banks faster than
/// it spends and the source is never delayed.
pub(crate) struct CaptureCredit {
    /// Banked frames, in `[-1.0, CAP]`. Dips below 0 when a grab spent credit
    /// it had only partly banked; the owed fraction is repaid before the next grab.
    credit: f32,
    last: Instant,
}

impl CaptureCredit {
    /// At most this many frames may follow a stall back-to-back. One frame of
    /// instant catch-up plus the floor's own headroom.
    pub(crate) const CAP: f32 = 1.25;

    pub(crate) fn new(now: Instant) -> CaptureCredit {
        CaptureCredit {
            credit: Self::CAP,
            last: now,
        }
    }

    /// `now` once a full frame is banked, else the missing fraction of an interval.
    pub(crate) fn earliest(&mut self, now: Instant, interval: Duration) -> Instant {
        let secs = interval.as_secs_f32();
        if secs > 0.0 {
            let accrued = now.duration_since(self.last).as_secs_f32() / secs;
            self.credit = (self.credit + accrued).min(Self::CAP);
        }
        self.last = now;
        now + interval.mul_f32((1.0 - self.credit).max(0.0))
    }

    pub(crate) fn charge(&mut self) {
        self.credit -= 1.0;
    }
}

/// Parsed-once `PUNKTFUNK_VIDEO_DROP` (1..=90, else off): discard N % of
/// sealed wire packets before send. Honored by both video planes.
pub(crate) fn video_drop_pct() -> u32 {
    static PCT: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *PCT.get_or_init(|| {
        let pct = std::env::var("PUNKTFUNK_VIDEO_DROP")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .filter(|p| (1..=90).contains(p))
            .unwrap_or(0);
        if pct > 0 {
            tracing::warn!(
                pct,
                "PUNKTFUNK_VIDEO_DROP: injecting wire-packet loss (FEC test)"
            );
        }
        pct
    })
}

pub(crate) fn inject_video_drop<T>(packets: &mut Vec<T>) -> u64 {
    let pct = video_drop_pct();
    if pct == 0 {
        return 0;
    }
    use rand::RngExt;
    let mut rng = rand::rng();
    let before = packets.len();
    packets.retain(|_| rng.random_range(0..100) >= pct);
    (before - packets.len()) as u64
}

/// Percentile of a slice (`q` in `0.0..=1.0`). Sorts in place.
pub(crate) fn percentile(v: &mut [u32], q: f64) -> u32 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    let i = ((v.len() as f64 * q) as usize).min(v.len() - 1);
    v[i]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native_cfg(burst_cap: usize) -> PaceCfg {
        PaceCfg {
            burst_bytes: Some(burst_cap),
            chunk: ChunkPolicy::Fixed(16),
            sleep_floor: Duration::from_micros(500),
        }
    }

    /// Mirrors `gamestream::stream::spawn_sender`: auto burst + bounded overflow.
    fn gs_cfg(burst_cap: usize) -> PaceCfg {
        PaceCfg {
            burst_bytes: Some(burst_cap),
            chunk: ChunkPolicy::Bounded {
                min_chunk: 16,
                max_steps: 12,
            },
            sleep_floor: Duration::from_micros(500),
        }
    }

    fn packets(n: usize, len: usize) -> Vec<Vec<u8>> {
        (0..n).map(|_| vec![0u8; len]).collect()
    }

    /// Burst split + 16-packet chunks + `m = ceil(overflow/16).max(1)` must
    /// match the pre-shared-module `paced_submit` math.
    #[test]
    fn native_schedule_matches_legacy_paced_submit() {
        let legacy = |sizes: &[usize], burst_cap: usize| -> (usize, usize) {
            let mut cum = 0usize;
            let mut split = sizes.len();
            for (k, len) in sizes.iter().enumerate() {
                cum += len;
                if cum >= burst_cap {
                    split = k + 1;
                    break;
                }
            }
            let m = (sizes.len() - split).div_ceil(16).max(1);
            (split, m)
        };
        for (n, len, cap) in [
            (1usize, 1200usize, 128 * 1024usize), // tiny ≪ cap → all burst
            (109, 1200, 128 * 1024),              // cap boundary
            (110, 1200, 128 * 1024),              // one past
            (600, 1200, 128 * 1024),              // burst + overflow
            (3300, 1200, 128 * 1024),             // multi-MB IDR
            (600, 1200, 0),                       // cap 0: first packet still bursts
            (0, 1200, 128 * 1024),                // empty (post-drop) frame
        ] {
            let pkts = packets(n, len);
            let sizes: Vec<usize> = pkts.iter().map(|p| p.len()).collect();
            let (split, m) = legacy(&sizes, cap);
            // Fixed schedules must not read the budget at all.
            for budget in [Duration::ZERO, Duration::from_millis(7)] {
                let s = schedule(&pkts, &native_cfg(cap), budget);
                assert_eq!(s.burst_len, split, "n={n} cap={cap}: burst split");
                assert_eq!(s.chunk, 16, "n={n} cap={cap}: chunk size");
                assert_eq!(s.steps, m, "n={n} cap={cap}: paced step count");
            }
        }
    }

    /// Burst follows the native cap-crossing rule; overflow stays
    /// `chunk = max(16, ceil(overflow/12))`, ≤ 12 steps.
    #[test]
    fn gamestream_schedule_bursts_then_bounds_the_overflow() {
        // 20 Mbps × 3 pace → auto burst 75 000 B (10 ms at the rate).
        let cap = auto_burst_bytes(60_000_000, 0);
        assert_eq!(cap, 75_000);
        // Steady-state 60 fps at 20 Mbps (~42 KB, ~35 packets): under the cap,
        // whole frame leaves immediately.
        let pkts = packets(35, 1200);
        for budget in [Duration::ZERO, Duration::from_millis(7)] {
            let s = schedule(&pkts, &gs_cfg(cap), budget);
            assert_eq!(s.burst_len, 35, "steady-state frame bursts whole");
        }
        // IDR (~600 KB, 500 packets): cum crosses 75 000 at packet 63; 437
        // overflow in the bounded layout.
        let pkts = packets(500, 1200);
        for budget in [Duration::ZERO, Duration::from_millis(7)] {
            let s = schedule(&pkts, &gs_cfg(cap), budget);
            assert_eq!(s.burst_len, 63, "burst split at the cap crossing");
            let overflow: usize = 500 - 63;
            let chunk = 16usize.max(overflow.div_ceil(12));
            assert_eq!(s.chunk, chunk, "overflow keeps the bounded chunking");
            assert_eq!(s.steps, overflow.div_ceil(chunk));
            assert!(s.steps <= 12, "step count stays bounded");
            assert!(s.chunk >= 16, "chunk floor");
        }
        // Bounded-layout invariants across sizes.
        for &n in &[64usize, 146, 610, 5000, 50_000] {
            let pkts = packets(n, 1200);
            let s = schedule(&pkts, &gs_cfg(cap), Duration::ZERO);
            let overflow = n - s.burst_len;
            assert!(s.steps <= 12, "n={n}: step count bounded");
            assert!(s.chunk >= 16, "n={n}: chunk floor");
            assert!(
                s.chunk * s.steps >= overflow,
                "n={n}: layout covers the overflow"
            );
        }
    }

    /// 16-packet chunks until the per-chunk interval would drop under 500 µs,
    /// then coarsen, capped at the 64-segment GSO limit; zero budget takes max.
    #[test]
    fn adaptive_chunk_coarsens_with_rate() {
        let cfg = PaceCfg {
            burst_bytes: Some(12_000),
            chunk: ChunkPolicy::Adaptive { base: 16, max: 64 },
            sleep_floor: Duration::from_micros(500),
        };
        // 210 × 1200 B: packets 0..=9 burst (cum hits 12 000 at #10), 200 overflow.
        let pkts = packets(210, 1200);
        // Ample budget (100 ms): a 16-packet interval is ≫ floor → base.
        let s = schedule(&pkts, &cfg, Duration::from_millis(100));
        assert_eq!((s.burst_len, s.chunk, s.steps), (10, 16, 13));
        // 2.5 ms budget: c ≥ 200 × 500 µs / 2.5 ms = 40 → 5 steps × 500 µs each.
        let s = schedule(&pkts, &cfg, Duration::from_micros(2_500));
        assert_eq!((s.chunk, s.steps), (40, 5));
        // 1 ms budget: c ≥ 100 → capped at 64 (GSO segment limit).
        let s = schedule(&pkts, &cfg, Duration::from_millis(1));
        assert_eq!((s.chunk, s.steps), (64, 4));
        // Zero budget (blast): max chunk = fewest syscalls.
        let s = schedule(&pkts, &cfg, Duration::ZERO);
        assert_eq!((s.chunk, s.steps), (64, 4));
        // Whole frame under the cap: no overflow → base chunk for the burst sends.
        let s = schedule(&packets(5, 1200), &cfg, Duration::ZERO);
        assert_eq!((s.burst_len, s.chunk, s.steps), (5, 16, 1));
    }

    /// Chunk sequence matches the schedule. Zero budget, so this never sleeps.
    #[test]
    fn pace_frame_sends_the_scheduled_chunk_sequence() {
        // Native, 40 × 1 KB with a 10 KB cap: packets 0..=9 burst (cum hits 10 KB at #10),
        // then 30 overflow → chunks of 16: [10..26), [26..40).
        let pkts = packets(40, 1024);
        let mut seen: Vec<usize> = Vec::new();
        let stat = pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &native_cfg(10 * 1024),
            None,
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(seen, vec![10, 16, 14]);
        assert!(stat.paced);

        // Frame under the cap: one immediate burst (chunked at 16), nothing paced.
        let pkts = packets(20, 100);
        let mut seen: Vec<usize> = Vec::new();
        let stat = pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &native_cfg(128 * 1024),
            None,
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(seen, vec![16, 4]);
        assert!(!stat.paced);

        // Adaptive, zero budget: burst in one ≤64-packet chunk, overflow in
        // 64-packet super-chunks (blast path takes the coarsest syscall batch).
        let pkts = packets(210, 1200);
        let mut seen: Vec<usize> = Vec::new();
        let stat = pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &PaceCfg {
                burst_bytes: Some(12_000),
                chunk: ChunkPolicy::Adaptive { base: 16, max: 64 },
                sleep_floor: Duration::from_micros(500),
            },
            None,
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(seen, vec![10, 64, 64, 64, 8]);
        assert!(stat.paced);

        // GameStream, 146 × 1 KB with a 16 KB burst cap: packets 0..=15 burst (cum hits 16 KB
        // at #16, sent as one 16-packet chunk), then 130 overflow → bounded chunks of
        // max(16, ceil(130/12)=11) = 16: 9 chunks (8 × 16 + 2).
        let pkts = packets(146, 1024);
        let mut seen: Vec<usize> = Vec::new();
        pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &gs_cfg(16 * 1024),
            None,
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(seen.len(), 10);
        assert_eq!(seen.iter().sum::<usize>(), 146);
        assert!(seen[..9].iter().all(|&c| c == 16));
        assert_eq!(*seen.last().unwrap(), 2);

        let pkts = packets(64, 1024);
        let mut calls = 0;
        let r = pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &gs_cfg(16 * 1024),
            None,
            |_chunk| {
                calls += 1;
                if calls == 2 {
                    Err(std::io::Error::other("client gone"))
                } else {
                    Ok(())
                }
            },
        );
        assert!(r.is_err());
        assert_eq!(calls, 2, "no sends after the failing chunk");
    }

    /// Sleep target is `budget × (j+1) / steps`. GameStream's
    /// `(budget / steps) × (j+1)` agrees to ≤ `steps` nanoseconds.
    #[test]
    fn sleep_targets_match_legacy_formulas() {
        let budget = Duration::from_micros(12_500); // ¾ of a 60 Hz frame interval
        for steps in [1usize, 2, 10, 12] {
            for j in 0..steps {
                let unified = budget.mul_f64((j + 1) as f64 / steps as f64);
                assert_eq!(unified, budget.mul_f64((j + 1) as f64 / steps as f64));
                // GameStream: per_step rounds to ns first; ≤ `steps` ns apart.
                let gs_legacy = budget.mul_f64(1.0 / steps as f64).mul_f64((j + 1) as f64);
                let diff = unified.abs_diff(gs_legacy);
                assert!(
                    diff <= Duration::from_nanos(steps as u64),
                    "steps={steps} j={j}: {diff:?} off legacy"
                );
            }
        }
    }

    /// `UntilDeadline` cap bounds the spread from above. `Duration::MAX`
    /// reproduces the deadline-only schedule.
    #[test]
    fn until_deadline_cap_bounds_the_budget() {
        let cfg = PaceCfg {
            burst_bytes: Some(12_000),
            chunk: ChunkPolicy::Adaptive { base: 16, max: 64 },
            sleep_floor: Duration::from_micros(500),
        };
        // 210 × 1200 B: 10 burst, 200 overflow.
        let pkts = packets(210, 1200);

        // Zero cap + far deadline: budget collapses to 0 → blast schedule
        // even though the deadline alone would have spread ~90 ms.
        let mut seen: Vec<usize> = Vec::new();
        let stat = pace_frame(
            &pkts,
            PaceBudget::UntilDeadline {
                deadline: Instant::now() + Duration::from_millis(100),
                fraction: 0.9,
                cap: Duration::ZERO,
            },
            &cfg,
            None,
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(seen, vec![10, 64, 64, 64, 8], "zero cap = blast schedule");
        assert!(stat.paced);
        assert!(
            stat.spread_us < 50_000,
            "zero cap must not sleep toward the deadline"
        );

        // 2.5 ms cap under a ~90 ms deadline: the cap sizes the chunks
        // (c ≥ 200 × 500 µs / 2.5 ms = 40) and the frame drains in ~2.5 ms.
        let mut seen: Vec<usize> = Vec::new();
        let stat = pace_frame(
            &pkts,
            PaceBudget::UntilDeadline {
                deadline: Instant::now() + Duration::from_millis(100),
                fraction: 0.9,
                cap: Duration::from_micros(2_500),
            },
            &cfg,
            None,
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(
            seen,
            vec![10, 40, 40, 40, 40, 40],
            "cap drives chunk sizing"
        );
        assert!(
            stat.spread_us < 50_000,
            "capped spread must be ~2.5 ms, nowhere near the 90 ms deadline budget"
        );

        // Uncapped: no-slack deadline still collapses to the blast path.
        let mut seen: Vec<usize> = Vec::new();
        pace_frame(
            &pkts,
            PaceBudget::UntilDeadline {
                deadline: Instant::now(),
                fraction: 0.9,
                cap: Duration::MAX,
            },
            &cfg,
            None,
            |chunk| {
                seen.push(chunk.len());
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(
            seen,
            vec![10, 64, 64, 64, 8],
            "MAX cap = legacy no-slack blast"
        );
    }

    /// Rate-cap budget is overflow wire time at the pace rate — a fixed spread
    /// the deadline cannot under-cut — bounded by [`MAX_PACE_SPREAD`]. Rate 0
    /// / no overflow keep deadline-only.
    #[test]
    fn native_budget_is_rate_bound_never_deadline_cut() {
        // 3 MB overflow at 3×240 Mbps needs ~33 ms; a 4 ms deadline must not
        // shrink that to a blast.
        let deadline = Instant::now() + Duration::from_millis(4); // 240 fps interval
        let b = native_budget(deadline, 720_000_000, 3_000_000, MAX_PACE_SPREAD);
        assert_eq!(b, PaceBudget::Fixed(Duration::from_nanos(33_333_333)));

        // Steady-state 90 KB at 3×240 Mbps = 1 ms.
        let b = native_budget(deadline, 720_000_000, 90_000, MAX_PACE_SPREAD);
        assert_eq!(b, PaceBudget::Fixed(Duration::from_micros(1_000)));

        // 3 MB at 60 Mbps pace = 400 ms; [`MAX_PACE_SPREAD`] bounds the stall.
        let b = native_budget(deadline, 60_000_000, 3_000_000, MAX_PACE_SPREAD);
        assert_eq!(b, PaceBudget::Fixed(MAX_PACE_SPREAD));

        // `PUNKTFUNK_PACE_FACTOR=0`: deadline-only spread, uncapped.
        let b = native_budget(deadline, 0, 3_000_000, MAX_PACE_SPREAD);
        assert!(matches!(
            b,
            PaceBudget::UntilDeadline {
                fraction,
                cap: Duration::MAX,
                ..
            } if fraction == 0.9
        ));

        // No overflow (whole frame bursts): budget is never consulted.
        let b = native_budget(deadline, 720_000_000, 0, MAX_PACE_SPREAD);
        assert!(matches!(b, PaceBudget::UntilDeadline { .. }));
    }

    /// A paced IDR must not park the send thread past ~2 frame intervals —
    /// longer backs encode|send `sync_channel(3)` into `cadence_degraded`.
    #[test]
    fn native_budget_spread_capped_to_frame_intervals() {
        let deadline = Instant::now() + Duration::from_millis(8);
        // 3 MB IDR at 60 Mbps pace wants 400 ms; two 120 Hz intervals (16.6 ms) win.
        let two_intervals = Duration::from_nanos(2 * 8_333_333);
        let b = native_budget(deadline, 60_000_000, 3_000_000, two_intervals);
        assert_eq!(b, PaceBudget::Fixed(two_intervals));

        let b = native_budget(deadline, 720_000_000, 90_000, two_intervals);
        assert_eq!(b, PaceBudget::Fixed(Duration::from_micros(1_000)));

        // Absolute ceiling still binds when the interval cap is larger
        // (5 fps must not re-license a 400 ms stall).
        let b = native_budget(deadline, 60_000_000, 3_000_000, Duration::from_millis(400));
        assert_eq!(b, PaceBudget::Fixed(MAX_PACE_SPREAD));
    }

    /// Unpaced allowance is 10 ms at the pace rate, clamped to [16 KiB, 256 KiB].
    #[test]
    fn auto_burst_is_time_at_pace_rate() {
        // 5 Mbps × 3 = 15 Mbps pace → 18.75 KiB.
        assert_eq!(auto_burst_bytes(15_000_000, 40_000), 18_750);
        // 30 Mbps × 3 = 90 Mbps → ~112 KiB (near the 128 KiB LAN floor).
        assert_eq!(auto_burst_bytes(90_000_000, 250_000), 112_500);
        // Below ~13 Mbps pace the 16 KiB floor binds.
        assert_eq!(auto_burst_bytes(3_000_000, 10_000), 16 * 1024);
        // Gigabit-class pace clamps at 256 KiB — the rest rides the 3×-rate spread.
        assert_eq!(auto_burst_bytes(3_000_000_000, 4_000_000), 256 * 1024);
        // `PUNKTFUNK_PACE_FACTOR=0`: fraction-of-frame burst.
        assert_eq!(auto_burst_bytes(0, 4_000_000), 1_000_000);
        assert_eq!(auto_burst_bytes(0, 40_000), 128 * 1024);
    }

    /// `Smooth` is one super-packet whatever the rate; the other profiles keep the
    /// 10 ms allowance; only `Capped` has a clock, floored at 0.8 Gbit/s.
    #[test]
    fn smooth_is_the_sixteen_kib_allowance() {
        for rate in [15_000_000u64, 240_000_000, 3_000_000_000] {
            assert_eq!(
                DeliveryProfile::Smooth.burst_bytes(rate, 500_000),
                BURST_MIN
            );
            assert_eq!(
                DeliveryProfile::Burst.burst_bytes(rate, 500_000),
                auto_burst_bytes(rate, 500_000)
            );
            assert_eq!(
                DeliveryProfile::Capped.burst_bytes(rate, 500_000),
                auto_burst_bytes(rate, 500_000)
            );
            assert_eq!(DeliveryProfile::Burst.cap_bps(rate), None);
            assert_eq!(DeliveryProfile::Smooth.cap_bps(rate), None);
        }
        assert_eq!(
            DeliveryProfile::Capped.cap_bps(240_000_000),
            Some(DeliveryProfile::CAP_FLOOR_BPS)
        );
        assert_eq!(
            DeliveryProfile::Capped.cap_bps(2_400_000_000),
            Some(2_400_000_000)
        );
        // `Smooth` bounds a paced group to its allowance, or half a millisecond at the rate.
        let mut p = Pacing::new(None);
        p.update(
            240_000_000,
            Duration::from_millis(33),
            DeliveryProfile::Smooth,
        );
        assert_eq!(p.group_bytes(), BURST_MIN);
        p.update(
            1_000_000_000,
            Duration::from_millis(33),
            DeliveryProfile::Smooth,
        );
        assert_eq!(p.group_bytes(), 62_500);
        p.update(
            1_000_000_000,
            Duration::from_millis(33),
            DeliveryProfile::Burst,
        );
        assert_eq!(p.group_bytes(), usize::MAX);
        // A pinned `PUNKTFUNK_PACE_BURST_KB` wins over the profile's allowance.
        let mut p = Pacing::new(Some(32 * 1024));
        p.update(
            240_000_000,
            Duration::from_millis(33),
            DeliveryProfile::Smooth,
        );
        assert_eq!(p.burst_bytes(500_000), 32 * 1024);
    }

    #[test]
    fn an_unknown_delivery_value_is_burst() {
        assert_eq!(parse_delivery(" Capped "), Some(DeliveryProfile::Capped));
        assert_eq!(parse_delivery("smooth"), Some(DeliveryProfile::Smooth));
        assert_eq!(parse_delivery("burst"), Some(DeliveryProfile::Burst));
        assert_eq!(parse_delivery("paced"), None);
        assert_eq!(DeliveryProfile::from_u8(7), DeliveryProfile::Burst);
        assert_eq!(DeliveryProfile::from_u8(1), DeliveryProfile::Capped);
    }

    /// The clock owes the wire time of what already left, carries it across AUs, and
    /// never owes a catch-up after idle. The profile owns the clock: it appears under
    /// `Capped` and goes with it.
    #[test]
    fn a_group_clock_carries_its_phase_and_owes_no_catch_up() {
        let t0 = Instant::now();
        let mut c = GroupClock::new(800_000_000, t0);
        assert_eq!(c.wait(64 * 1024, t0), Duration::ZERO);
        // 64 KiB at 0.8 Gbit/s = 655.36 µs.
        assert_eq!(c.wait(64 * 1024, t0), Duration::from_nanos(655_360));
        // A skipped sub-floor wait stays owed: the third group waits for both.
        assert_eq!(c.wait(64 * 1024, t0), Duration::from_nanos(1_310_720));
        // Idle past the clock: restart at `now`, nothing owed.
        let later = t0 + Duration::from_millis(10);
        assert_eq!(c.wait(64 * 1024, later), Duration::ZERO);
        assert_eq!(c.wait(1, later), Duration::from_nanos(655_360));

        let mut p = Pacing::new(None);
        p.update(
            240_000_000,
            Duration::from_millis(33),
            DeliveryProfile::Capped,
        );
        assert!(p.clock.is_some());
        p.update(
            240_000_000,
            Duration::from_millis(33),
            DeliveryProfile::Burst,
        );
        assert!(p.clock.is_none());
    }

    /// Under a clock the burst's chunks leave on it: 219 × 1200 B (all under the 256 KiB
    /// cap) take ≥ 2 ms at 0.8 Gbit/s instead of one blast, and the next frame starts
    /// behind the first one's clock.
    #[test]
    fn capped_burst_leaves_in_groups_on_one_clock() {
        let pkts = packets(219, 1200);
        let cfg = native_cfg(256 * 1024);
        let mut clock = GroupClock::new(800_000_000, Instant::now());
        let t0 = Instant::now();
        let mut sends = 0usize;
        let stat = pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &cfg,
            Some(&mut clock),
            |chunk| {
                sends += chunk.len();
                Ok::<(), std::io::Error>(())
            },
        )
        .unwrap();
        assert_eq!(sends, 219);
        assert!(!stat.paced, "under the cap nothing is overflow");
        // 262 800 B × 8 / 0.8 Gbit/s = 2.63 ms, less the last chunk's own wire time.
        assert!(
            t0.elapsed() >= Duration::from_millis(2),
            "capped burst left in {:?}",
            t0.elapsed()
        );
        assert!(
            clock.wait(0, Instant::now()) <= Duration::from_micros(400),
            "the clock ends with the last chunk"
        );
        // Without a clock the same frame is one blast.
        let t1 = Instant::now();
        pace_frame(
            &pkts,
            PaceBudget::Fixed(Duration::ZERO),
            &cfg,
            None,
            |_chunk| Ok::<(), std::io::Error>(()),
        )
        .unwrap();
        assert!(t1.elapsed() < Duration::from_millis(1));
    }

    #[test]
    fn drop_injection_off_by_default() {
        let mut pkts = packets(100, 64);
        assert_eq!(inject_video_drop(&mut pkts), 0);
        assert_eq!(pkts.len(), 100);
    }

    /// Saturated source: grab the instant the gate opens, then `earliest`
    /// again (encode folded into the wait). Returns grab instants.
    fn grab_saturated(
        b: &mut CaptureCredit,
        start: Instant,
        interval: Duration,
        n: usize,
    ) -> Vec<Instant> {
        let mut now = start;
        let mut grabs = Vec::with_capacity(n);
        for _ in 0..n {
            let gate = b.earliest(now, interval);
            let grab = gate.max(now);
            b.charge();
            grabs.push(grab);
            now = grab;
        }
        grabs
    }

    #[test]
    fn capture_credit_pins_a_saturated_source_at_the_interval() {
        let interval = Duration::from_millis(10);
        let t0 = Instant::now();
        let mut b = CaptureCredit::new(t0);
        let grabs = grab_saturated(&mut b, t0, interval, 120);
        // Total may exceed the on-rate schedule by at most the burst cap —
        // 120 grabs span no less than (120 - 1 - CAP) intervals.
        let span = grabs[119].duration_since(grabs[0]);
        assert!(
            span >= interval.mul_f32(120.0 - 1.0 - CaptureCredit::CAP),
            "span {span:?} admits more than CAP frames of overshoot"
        );
        // Steady state is the interval, not 0.9 × interval.
        for w in grabs[20..].windows(2) {
            let gap = w[1].duration_since(w[0]);
            assert!(
                gap >= interval.mul_f32(0.999) && gap <= interval.mul_f32(1.001),
                "steady-state gap {gap:?} != interval {interval:?}"
            );
        }
    }

    #[test]
    fn capture_credit_never_delays_an_on_rate_or_slow_source() {
        let interval = Duration::from_millis(10);
        let t0 = Instant::now();
        let mut b = CaptureCredit::new(t0);
        // Half the pacing rate (60 fps game on a 120 fps session): every arrival
        // banks two frames and spends one — the gate is always already open.
        let mut now = t0;
        for _ in 0..50 {
            now += interval * 2;
            assert_eq!(
                b.earliest(now, interval),
                now,
                "slow source must not be gated"
            );
            b.charge();
        }
        // On-rate: never gated (credit hovers at the cap, never below 1).
        let mut b = CaptureCredit::new(t0);
        let mut now = t0;
        for _ in 0..50 {
            now += interval;
            assert_eq!(
                b.earliest(now, interval),
                now,
                "on-rate source must not be gated"
            );
            b.charge();
        }
    }

    #[test]
    fn capture_credit_burst_after_a_stall_is_capped() {
        let interval = Duration::from_millis(10);
        let t0 = Instant::now();
        let mut b = CaptureCredit::new(t0);
        // Settle into the gated steady state, then stall for 10 intervals.
        let grabs = grab_saturated(&mut b, t0, interval, 20);
        let stall_end = grabs[19] + interval * 10;
        // Recovery may run ahead of on-rate by at most CAP frames: the second
        // post-stall grab is already re-gated.
        let after = grab_saturated(&mut b, stall_end, interval, 3);
        assert_eq!(after[0], stall_end, "first post-stall grab is immediate");
        assert!(
            after[1].duration_since(after[0]) >= interval.mul_f32(2.0 - CaptureCredit::CAP),
            "second post-stall grab spent more than the burst cap"
        );
        assert!(
            after[2].duration_since(after[1]) >= interval.mul_f32(0.999),
            "third post-stall grab must be back on the interval grid"
        );
    }

    #[test]
    fn percentile_picks_expected_ranks() {
        let mut v = vec![90, 10, 50, 70, 30];
        assert_eq!(percentile(&mut v, 0.0), 10);
        assert_eq!(percentile(&mut v, 0.5), 50);
        assert_eq!(percentile(&mut v, 0.99), 90);
        assert_eq!(percentile(&mut [], 0.5), 0);
    }
}
