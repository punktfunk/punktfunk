//! How both video planes put a frame on the wire: every packet on one session clock at the
//! rate the link carries.
//!
//! The rate is `R = min(max(f × B, 0.9 L), 0.9 L_hard)` ([`rate_bps`]): `B` the stream rate
//! and `f` its factor, `L` the session's link rate ([`LinkRate::resolve`]), `L_hard` the
//! ports' speed, or the link a pinned stream proved. Packets leave in groups of
//! [`group_bytes`]; a wait under the sleep floor spins, so the clock holds at multi-gigabit
//! rates. `PUNKTFUNK_DELIVERY` is the operator's A/B ([`forced`]).
//!
//! `PUNKTFUNK_VIDEO_DROP` and the percentile helper live here too.

use std::time::{Duration, Instant};

/// A knob read and parsed once per process: one static per call site.
macro_rules! env_once {
    ($ty:ty, $init:expr) => {{
        static CELL: std::sync::OnceLock<$ty> = std::sync::OnceLock::new();
        *CELL.get_or_init(|| $init)
    }};
}
pub(crate) use env_once;

/// Native feeds this to the PUNKTFUNK_PERF histogram (pacing tail per frame).
pub(crate) struct PaceStat {
    pub(crate) spread_us: u32,
    pub(crate) paced: bool,
}

/// Ceiling on one frame's spread. Past this the send thread is parked too long; the tail is
/// late but still delivered whole.
pub(crate) const MAX_PACE_SPREAD: Duration = Duration::from_millis(100);

/// `L` with no evidence at all: a fifth under 1 GbE.
pub(crate) const FLOOR_KBPS: u32 = 800_000;

/// The smallest group, and the wake shape's first one: one GSO super-packet.
pub(crate) const GROUP_MIN: usize = 16 * 1024;

/// Packets in one send call: a GSO train.
pub(crate) const GROUP_PACKETS: usize = 64;

/// The wake shape's gap after the first group.
pub(crate) const WAKE_GAP: Duration = Duration::from_micros(300);

/// Waits at or over this sleep; shorter ones spin on the send thread.
const SLEEP_FLOOR: Duration = Duration::from_micros(500);

/// Waits under this are not worth a spin: the clock keeps the debt.
const SPIN_FLOOR: Duration = Duration::from_micros(20);

/// `PUNKTFUNK_PACE_FACTOR`: the multiple of the stream rate a frame may leave at (default 3;
/// the link carries 1× sustained, so a bounded 3× excursion is safe).
pub(crate) fn pace_factor() -> f64 {
    env_once!(
        f64,
        std::env::var("PUNKTFUNK_PACE_FACTOR")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|f: &f64| f.is_finite() && *f >= 0.0)
            .unwrap_or(3.0)
    )
}

/// How a session's frames leave. `Auto` and `Wake` are what a client asks for; `Burst` and
/// `Smooth` only `PUNKTFUNK_DELIVERY` sets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Shape {
    /// Every group on the clock at `R`.
    #[default]
    Auto = 0,
    /// A 16 KiB first group, a gap, then the rest at `R`: for a receiver that loses the
    /// head of a burst while its adapter wakes.
    Wake = 1,
    /// No clock: every frame at line rate.
    Burst = 2,
    /// 16 KiB groups at `f × B` alone, the link ignored.
    Smooth = 3,
}

impl Shape {
    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            1 => Shape::Wake,
            2 => Shape::Burst,
            3 => Shape::Smooth,
            _ => Shape::Auto,
        }
    }
}

/// What `PUNKTFUNK_DELIVERY` pins for every session of both planes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Forced {
    Shape(Shape),
    /// `L` held at [`FLOOR_KBPS`] whatever the link says.
    Capped,
}

impl Forced {
    /// The `StreamConfig` byte: `0` none, then auto, wake, burst, smooth, capped.
    pub(crate) fn wire(f: Option<Forced>) -> u8 {
        match f {
            None => 0,
            Some(Forced::Shape(s)) => s as u8 + 1,
            Some(Forced::Capped) => 5,
        }
    }
}

/// `PUNKTFUNK_DELIVERY=auto|wake|burst|capped|smooth`. Unset leaves the shape to the client.
pub(crate) fn forced() -> Option<Forced> {
    env_once!(Option<Forced>, {
        let s = pf_host_config::knob("PUNKTFUNK_DELIVERY");
        let f = s.as_deref().and_then(parse_forced);
        match (&s, f) {
            (Some(_), Some(f)) => {
                tracing::info!(delivery = ?f, "PUNKTFUNK_DELIVERY pins the delivery")
            }
            (Some(s), None) => tracing::warn!(
                value = %s,
                "PUNKTFUNK_DELIVERY not recognised — auto, wake, burst, capped or smooth"
            ),
            (None, _) => {}
        }
        f
    })
}

fn parse_forced(s: &str) -> Option<Forced> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "auto" => Forced::Shape(Shape::Auto),
        "wake" => Forced::Shape(Shape::Wake),
        "burst" => Forced::Shape(Shape::Burst),
        "smooth" => Forced::Shape(Shape::Smooth),
        "capped" => Forced::Capped,
        _ => return None,
    })
}

/// What both ends' OS said about their ports, kbps; `0` where it said nothing or the port
/// is not Ethernet. A PHY rate is not a capacity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ports {
    pub(crate) host_kbps: u32,
    pub(crate) client_kbps: u32,
}

impl Ports {
    /// From the host's facts and the client's, each `(iface_kind, link_mbps)`.
    pub(crate) fn of(host: (u8, u32), client: (u8, u32)) -> Ports {
        let wired = |(kind, mbps): (u8, u32)| {
            if kind == punktfunk_core::transport::IFACE_KIND_ETHERNET {
                mbps.saturating_mul(1_000)
            } else {
                0
            }
        };
        Ports {
            host_kbps: wired(host),
            client_kbps: wired(client),
        }
    }

    /// `L_hard`: the slower port either end knows; `0` = none.
    pub(crate) fn hard_kbps(&self) -> u32 {
        match (self.host_kbps, self.client_kbps) {
            (0, c) => c,
            (h, 0) => h,
            (h, c) => h.min(c),
        }
    }
}

/// Where a session's `L` came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Source {
    /// The client's feedback.
    Client,
    /// This host's port, while the client says nothing.
    HostPort,
    /// No evidence: [`FLOOR_KBPS`].
    Floor,
}

/// The session's link rate `L`, and the ports' `L_hard` beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LinkRate {
    pub(crate) kbps: u32,
    pub(crate) source: Source,
    pub(crate) hard_kbps: u32,
}

impl LinkRate {
    /// `L`: the client's report wins and this host's port caps it; with no report the
    /// host's port; with neither [`FLOOR_KBPS`], under the port too. `capped` holds the
    /// floor whatever the evidence.
    pub(crate) fn resolve(reported: Option<u32>, ports: Ports, forced: Option<Forced>) -> Self {
        let under_port = |k: u32| match ports.host_kbps {
            0 => k,
            p => k.min(p),
        };
        let (kbps, source) = match (forced, reported.filter(|&k| k > 0)) {
            (Some(Forced::Capped), _) => (FLOOR_KBPS, Source::Floor),
            (_, Some(k)) => (under_port(k), Source::Client),
            _ if ports.host_kbps > 0 => (ports.host_kbps, Source::HostPort),
            _ => (FLOOR_KBPS, Source::Floor),
        };
        LinkRate {
            kbps,
            source,
            hard_kbps: ports.hard_kbps(),
        }
    }
}

/// `R`, bits/s: `min(max(f × B, 0.9 L), 0.9 L_hard)`. `L_hard` is the ports' speed, and for
/// a pinned stream also the link it proved ([`pinned_wall`]).
pub(crate) fn rate_bps(bitrate_kbps: u32, link: &LinkRate, pinned_wall: Option<u32>) -> u64 {
    let stream = (f64::from(bitrate_kbps) * 1_000.0 * pace_factor()) as u64;
    let r = stream.max(u64::from(link.kbps) * 900);
    let hard = [Some(link.hard_kbps), pinned_wall]
        .into_iter()
        .flatten()
        .filter(|&k| k > 0)
        .min();
    hard.map_or(r, |h| r.min(u64::from(h) * 900)).max(1)
}

/// The link a pinned stream proved: the client's report, once the stream fits in 70 % of
/// it. A pinned stream's `f × B` says nothing about the link.
pub(crate) fn pinned_wall(pinned: bool, reported_kbps: u32, bitrate_kbps: u32) -> Option<u32> {
    (pinned && reported_kbps > 0 && u64::from(reported_kbps) * 7 >= u64::from(bitrate_kbps) * 10)
        .then_some(reported_kbps)
}

/// `G`: half a millisecond at `rate_bps`, at least [`GROUP_MIN`]. A group is also never
/// more than [`GROUP_PACKETS`] packets.
pub(crate) fn group_bytes(rate_bps: u64) -> usize {
    usize::try_from(rate_bps / 16_000)
        .unwrap_or(usize::MAX)
        .max(GROUP_MIN)
}

/// Bytes leave on a clock at `rate_bps`, carried across frames so back-to-back frames
/// cannot add up to a blast. A caller late by up to one group keeps that debt, so sleep
/// overshoot does not lower the rate; a clock idle longer restarts at `now`, owing no
/// catch-up, so a frame's head is never two groups.
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

    /// Book `bytes` on the clock: the wait owed before they may leave. A wait the caller
    /// skips stays owed, and so does lateness up to this group's own wire time.
    pub(crate) fn advance(&mut self, bytes: usize, now: Instant) -> Duration {
        let ahead = self.next.saturating_duration_since(now);
        let wire =
            Duration::from_nanos((bytes as u64).saturating_mul(8_000_000_000) / self.rate_bps);
        let late = now.saturating_duration_since(self.next);
        let start = if late <= wire { self.next } else { now };
        self.next = start + wire;
        ahead
    }

    /// Book `bytes` and wait their turn: sleep from the floor up, spin above
    /// [`SPIN_FLOOR`], else go now. `true` when the clock held them back.
    pub(crate) fn wait_or_spin(&mut self, bytes: usize, now: Instant) -> bool {
        let ahead = self.advance(bytes, now);
        match hold(ahead) {
            Hold::Sleep => std::thread::sleep(ahead),
            Hold::Spin => {
                let until = Instant::now() + ahead;
                while Instant::now() < until {
                    std::hint::spin_loop();
                }
            }
            Hold::Go => return false,
        }
        true
    }

    /// How long until the next group may leave; zero when it may now.
    pub(crate) fn ahead(&self, now: Instant) -> Duration {
        self.next.saturating_duration_since(now)
    }

    /// The wake shape's gap: nothing more leaves for [`WAKE_GAP`].
    pub(crate) fn wake_gap(&mut self, now: Instant) {
        self.next = self.next.max(now + WAKE_GAP);
    }
}

/// How a group waits out what the clock says it owes.
#[derive(Debug, PartialEq, Eq)]
enum Hold {
    Sleep,
    Spin,
    /// Too short to be worth a spin: go now; the clock keeps the debt.
    Go,
}

fn hold(ahead: Duration) -> Hold {
    if ahead >= SLEEP_FLOOR {
        Hold::Sleep
    } else if ahead >= SPIN_FLOOR {
        Hold::Spin
    } else {
        Hold::Go
    }
}

/// One session's pacer: every frame of every stream on one clock at `R`, in groups of `G`.
pub(crate) struct Pacer {
    link: LinkRate,
    shape: Shape,
    clock: GroupClock,
    max_spread: Duration,
    ports: Ports,
    forced: Option<Forced>,
    /// `R` from [`Self::update`]; a frame may raise its own.
    base_bps: u64,
    rate_bps: u64,
    group: usize,
    /// The wake shape's first group of this frame is still to leave.
    wake_first: bool,
    started: Option<Instant>,
    paced: bool,
    sock_ns: u64,
}

impl Pacer {
    pub(crate) fn new(ports: Ports, forced: Option<Forced>) -> Self {
        Pacer {
            link: LinkRate::resolve(None, ports, forced),
            shape: Shape::Auto,
            clock: GroupClock::new(1, Instant::now()),
            max_spread: MAX_PACE_SPREAD,
            ports,
            forced,
            base_bps: 1,
            rate_bps: 1,
            group: GROUP_MIN,
            wake_first: false,
            started: None,
            paced: false,
            sock_ns: 0,
        }
    }

    /// Per AU: the stream rate, the client's link report (`0` = none yet), a pinned
    /// stream's wall, the client's shape, and the spread one frame may take. Returns `R`.
    pub(crate) fn update(
        &mut self,
        bitrate_kbps: u32,
        link_kbps: u32,
        pinned_wall: Option<u32>,
        shape: Shape,
        max_spread: Duration,
    ) -> u64 {
        self.link = LinkRate::resolve(Some(link_kbps), self.ports, self.forced);
        self.shape = match self.forced {
            Some(Forced::Shape(s)) => s,
            _ => shape,
        };
        let stream = (f64::from(bitrate_kbps) * 1_000.0 * pace_factor()) as u64;
        self.base_bps = if self.shape == Shape::Smooth && stream > 0 {
            stream
        } else {
            rate_bps(bitrate_kbps, &self.link, pinned_wall)
        };
        self.max_spread = max_spread;
        self.base_bps
    }

    /// The `L` the last [`Self::update`] resolved.
    pub(crate) fn link(&self) -> LinkRate {
        self.link
    }

    /// A frame of `frame_bytes` on the wire begins; `0` when not yet known (a streamed AU).
    /// A frame whose wire time at `R` would pass its spread leaves at the rate that fits it.
    pub(crate) fn begin(&mut self, frame_bytes: usize) {
        let spread_ns = self.max_spread.min(MAX_PACE_SPREAD).as_nanos().max(1) as u64;
        let floor_bps = (frame_bytes as u64).saturating_mul(8_000_000_000) / spread_ns;
        self.rate_bps = self.base_bps.max(floor_bps);
        self.group = match self.shape {
            Shape::Smooth => GROUP_MIN,
            Shape::Burst => usize::MAX,
            _ => group_bytes(self.rate_bps),
        };
        self.clock.set_rate(self.rate_bps);
        self.wake_first = self.shape == Shape::Wake;
        self.started = None;
        self.paced = false;
        self.sock_ns = 0;
    }

    /// Send `pkts` in groups on the clock. A `send` error ends the frame.
    pub(crate) fn send<T: AsRef<[u8]>, E>(
        &mut self,
        pkts: &[T],
        mut send: impl FnMut(&[T]) -> Result<(), E>,
    ) -> Result<(), E> {
        self.started.get_or_insert_with(Instant::now);
        let mut rest = pkts;
        while !rest.is_empty() {
            let max = if self.wake_first {
                GROUP_MIN
            } else {
                self.group
            };
            let n = group_len(rest, max);
            let (group, tail) = rest.split_at(n);
            rest = tail;
            if self.shape != Shape::Burst {
                let bytes: usize = group.iter().map(|p| p.as_ref().len()).sum();
                self.paced |= self.clock.wait_or_spin(bytes, Instant::now());
            }
            let t0 = Instant::now();
            send(group)?;
            self.sock_ns += t0.elapsed().as_nanos() as u64;
            if std::mem::take(&mut self.wake_first) {
                self.clock.wake_gap(Instant::now());
            }
        }
        Ok(())
    }

    /// The frame's spread, first send to now, and the time spent inside `send`.
    pub(crate) fn finish(&mut self) -> (PaceStat, u64) {
        let spread_us = self
            .started
            .take()
            .map_or(0, |s| s.elapsed().as_micros() as u32);
        (
            PaceStat {
                spread_us,
                paced: self.paced,
            },
            std::mem::take(&mut self.sock_ns),
        )
    }
}

/// Packets of the next group: at least one, at most [`GROUP_PACKETS`], no more than
/// `max_bytes` past the first.
fn group_len<T: AsRef<[u8]>>(pkts: &[T], max_bytes: usize) -> usize {
    let mut cum = 0usize;
    let mut n = 0usize;
    for p in pkts.iter().take(GROUP_PACKETS) {
        let len = p.as_ref().len();
        if n > 0 && cum.saturating_add(len) > max_bytes {
            break;
        }
        cum += len;
        n += 1;
    }
    n
}

/// `PUNKTFUNK_FRAME_DRIVEN=0` restores the fixed-cadence tick. On, the loop wakes on
/// the capturer's arrival ([`pf_capture::Capturer::supports_arrival_wait`]) or on the
/// next access unit of an encoder that publishes its own; a backend with neither
/// keeps the tick. Shared by both video planes.
pub(crate) fn frame_driven_enabled() -> bool {
    env_once!(
        bool,
        pf_host_config::env_on("PUNKTFUNK_FRAME_DRIVEN").unwrap_or(true)
    )
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
    env_once!(u32, {
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

    fn packets(n: usize, len: usize) -> Vec<Vec<u8>> {
        (0..n).map(|_| vec![0u8; len]).collect()
    }

    const ETH: u8 = punktfunk_core::transport::IFACE_KIND_ETHERNET;

    fn gbe() -> Ports {
        Ports::of((ETH, 1_000), (ETH, 1_000))
    }

    /// Every group's size and send instant for one frame through `p`.
    fn run(p: &mut Pacer, pkts: &[Vec<u8>]) -> Vec<(usize, Instant)> {
        let mut out = Vec::new();
        p.send(pkts, |g| {
            out.push((g.iter().map(|x| x.len()).sum(), Instant::now()));
            Ok::<(), ()>(())
        })
        .unwrap();
        out
    }

    fn pacer(rate_kbps: u32, shape: Shape) -> Pacer {
        // A stream rate whose 3 × wins over the floor sets `R` directly.
        let mut p = Pacer::new(Ports::default(), None);
        p.update(rate_kbps / 3, 0, None, shape, Duration::from_millis(33));
        p
    }

    /// The client's report wins and this host's port caps it; the host's port stands in for
    /// a silent client; nothing at all is the floor; `capped` is the floor whatever the
    /// evidence. Wi-Fi sets no port, and `L_hard` is the slower port either end knows.
    #[test]
    fn l_comes_from_the_client_then_the_host_port_then_the_floor() {
        let wifi = punktfunk_core::transport::IFACE_KIND_WIFI;
        let asym = Ports::of((ETH, 2_500), (ETH, 1_000));
        assert_eq!((asym.host_kbps, asym.hard_kbps()), (2_500_000, 1_000_000));
        assert_eq!(Ports::of((ETH, 2_500), (wifi, 866)).hard_kbps(), 2_500_000);
        assert_eq!(Ports::of((wifi, 866), (ETH, 1_000)).hard_kbps(), 1_000_000);
        assert_eq!(Ports::of((wifi, 866), (wifi, 866)), Ports::default());

        let l = LinkRate::resolve;
        assert_eq!(
            l(Some(940_000), asym, None),
            LinkRate {
                kbps: 940_000,
                source: Source::Client,
                hard_kbps: 1_000_000,
            }
        );
        assert_eq!(l(Some(9_000_000), asym, None).kbps, 2_500_000);
        let silent = l(None, asym, None);
        assert_eq!((silent.kbps, silent.source), (2_500_000, Source::HostPort));
        assert_eq!(l(Some(0), asym, None).source, Source::HostPort);
        let none = l(None, Ports::default(), None);
        assert_eq!((none.kbps, none.source), (FLOOR_KBPS, Source::Floor));
        assert_eq!(
            l(Some(940_000), asym, Some(Forced::Capped)).kbps,
            FLOOR_KBPS
        );
    }

    /// (a) 1 GbE both ends, 80 Mbit/s: `R` is 0.9 L and a group half a millisecond of it.
    /// (b) a 2.5 GbE host and a 1 GbE client's report: 900 Mbit/s, and the 10 GbE →
    /// 2.5 GbE desk 2.25 Gbit/s. (c) a 12 Mbit/s tunnel under an 8 Mbit/s stream: 3 × B
    /// wins and the group is the floor.
    #[test]
    fn r_is_the_link_rate_unless_the_stream_needs_more() {
        let one_g = LinkRate::resolve(None, gbe(), None);
        assert_eq!(rate_bps(80_000, &one_g, None), 900_000_000);
        assert_eq!(group_bytes(900_000_000), 56_250);

        let asym = Ports::of((ETH, 2_500), (ETH, 1_000));
        let reported = LinkRate::resolve(Some(1_000_000), asym, None);
        assert_eq!(rate_bps(80_000, &reported, None), 900_000_000);
        let desk = Ports::of((ETH, 10_000), (ETH, 2_500));
        let silent = LinkRate::resolve(None, desk, None);
        assert_eq!(rate_bps(80_000, &silent, None), 2_250_000_000);

        let wan = LinkRate::resolve(Some(12_000), Ports::default(), None);
        let r = rate_bps(8_000, &wan, None);
        assert_eq!(r, 24_000_000);
        assert_eq!(group_bytes(r), GROUP_MIN);

        // Smooth ignores the link, until there is no stream rate to pace at.
        let mut p = Pacer::new(gbe(), None);
        let d = Duration::from_millis(33);
        assert_eq!(p.update(80_000, 0, None, Shape::Smooth, d), 240_000_000);
        assert_eq!(p.update(0, 0, None, Shape::Smooth, d), 900_000_000);
        // The operator's shape wins over the client's.
        let mut forced = Pacer::new(gbe(), Some(Forced::Shape(Shape::Smooth)));
        assert_eq!(forced.update(80_000, 0, None, Shape::Wake, d), 240_000_000);
    }

    /// A pinned stream's hard ceiling is the link the client proved once the stream fits
    /// in 70 % of it, under the ports when they are known: a 1.4 Gbit/s PyroWave stream
    /// into a 2.5 GbE client is sent its own port, not 3 × 1.4.
    #[test]
    fn a_pinned_stream_paces_at_the_proven_link_rate() {
        assert_eq!(pinned_wall(true, 2_450_000, 1_427_000), Some(2_450_000));
        let ten_to_two = Ports::of((ETH, 10_000), (ETH, 2_500));
        let link = LinkRate::resolve(Some(2_450_000), ten_to_two, None);
        let wall = pinned_wall(true, 2_450_000, 1_427_000);
        assert_eq!(rate_bps(1_427_000, &link, wall), 2_205_000_000);
        let unknown = LinkRate::resolve(Some(2_450_000), Ports::default(), None);
        assert_eq!(rate_bps(1_427_000, &unknown, wall), 2_205_000_000);
        // A proof the stream does not fit under, or an adaptive stream, sets no ceiling.
        assert_eq!(pinned_wall(true, 1_000_000, 778_000), None);
        assert_eq!(pinned_wall(false, 2_450_000, 1_427_000), None);
        assert_eq!(rate_bps(1_427_000, &unknown, None), 4_281_000_000);
    }

    /// (a) A 167 KB frame at 900 Mbit/s leaves in groups of at most `G`, each on the
    /// clock: about 1.5 ms end to end instead of one line-rate blast.
    #[test]
    fn a_frame_leaves_in_groups_on_the_clock() {
        let t0 = Instant::now();
        let mut p = Pacer::new(gbe(), None);
        p.update(80_000, 0, None, Shape::Auto, Duration::from_millis(33));
        p.begin(0);
        let pkts = packets(116, 1_440);
        let groups = run(&mut p, &pkts);
        let (stat, _) = p.finish();
        assert!(groups.iter().all(|&(b, _)| b <= 56_250), "{groups:?}");
        assert_eq!(groups.iter().map(|g| g.0).sum::<usize>(), 116 * 1_440);
        // The last group waits for the wire time of all before it, from the clock's start.
        let before_last: usize = groups[..groups.len() - 1].iter().map(|g| g.0).sum();
        let owed = Duration::from_nanos(before_last as u64 * 8_000_000_000 / 900_000_000);
        let took = groups.last().unwrap().1 - t0;
        assert!(
            took + Duration::from_micros(30) >= owed,
            "{took:?} < {owed:?}"
        );
        assert!(took < Duration::from_millis(20), "{took:?}");
        assert!(stat.paced);

        // On a simulated clock: the last group is out by 1.6 ms, and each group waits for
        // the wire time of the one before it.
        let (rate, g) = (900_000_000, group_bytes(900_000_000));
        let mut c = GroupClock::new(rate, t0);
        let (mut now, mut rest, mut sent) = (t0, &pkts[..], Vec::new());
        while !rest.is_empty() {
            let n = group_len(rest, g);
            let bytes: usize = rest[..n].iter().map(Vec::len).sum();
            now += c.advance(bytes, now);
            sent.push((now, bytes));
            rest = &rest[n..];
        }
        let wire = |b: usize| Duration::from_nanos(b as u64 * 8_000_000_000 / rate);
        let (last_at, last_bytes) = *sent.last().unwrap();
        assert!(last_at + wire(last_bytes) - t0 <= Duration::from_micros(1_600));
        assert!(sent.windows(2).all(|w| w[1].0 - w[0].0 >= wire(w[0].1)));
    }

    /// (d) The wake shape: a 16 KiB first group, then at least 300 µs, then the rest.
    #[test]
    fn the_wake_shape_sends_a_small_group_then_waits() {
        let mut p = pacer(2_000_000, Shape::Wake);
        p.begin(0);
        let groups = run(&mut p, &packets(100, 1_440));
        assert!(groups[0].0 <= GROUP_MIN, "{}", groups[0].0);
        assert!(groups[1].1 - groups[0].1 >= WAKE_GAP);
        // The next frame takes the shape again.
        p.finish();
        p.begin(0);
        assert!(run(&mut p, &packets(100, 1_440))[0].0 <= GROUP_MIN);
    }

    /// (e) A frame whose wire time at `R` would pass its spread leaves at the rate that
    /// fits it: 600 KB at 24 Mbit/s would take 200 ms, it gets 20.
    #[test]
    fn a_giant_frame_fits_its_spread() {
        let pkts = packets(417, 1_440);
        let bytes = 417 * 1_440;
        let mut p = Pacer::new(Ports::default(), None);
        let wan = LinkRate::resolve(Some(12_000), Ports::default(), None);
        assert_eq!(rate_bps(8_000, &wan, None), 24_000_000);
        p.update(8_000, 12_000, None, Shape::Auto, Duration::from_millis(20));
        p.begin(bytes);
        let t0 = Instant::now();
        run(&mut p, &pkts);
        assert!(
            t0.elapsed() < Duration::from_millis(60),
            "{:?}",
            t0.elapsed()
        );
    }

    /// (f) `Burst` has no clock: the frame leaves in 64-packet trains, nothing waits.
    #[test]
    fn burst_sends_without_waiting() {
        let mut p = pacer(24_000, Shape::Burst);
        p.begin(0);
        let t0 = Instant::now();
        let groups = run(&mut p, &packets(200, 1_440));
        assert!(t0.elapsed() < Duration::from_millis(5));
        assert_eq!(groups.len(), 4);
        assert!(!p.finish().0.paced);
    }

    #[test]
    fn forced_names_parse_and_unknown_is_none() {
        assert_eq!(parse_forced(" Capped "), Some(Forced::Capped));
        assert_eq!(parse_forced("wake"), Some(Forced::Shape(Shape::Wake)));
        assert_eq!(parse_forced("fast"), None);
        for s in [Shape::Auto, Shape::Wake, Shape::Burst, Shape::Smooth] {
            assert_eq!(Shape::from_u8(s as u8), s);
        }
        assert_eq!(Forced::wire(None), 0);
        assert_eq!(Forced::wire(Some(Forced::Shape(Shape::Auto))), 1);
        assert_eq!(Forced::wire(Some(Forced::Capped)), 5);
    }

    /// The clock owes the wire time of what already left, carries it across frames, and
    /// never owes a catch-up after idle. The wake gap holds the next group back.
    #[test]
    fn a_group_clock_carries_its_phase_and_owes_no_catch_up() {
        let t0 = Instant::now();
        let mut c = GroupClock::new(800_000_000, t0);
        assert_eq!(c.advance(64 * 1024, t0), Duration::ZERO);
        // 64 KiB at 0.8 Gbit/s = 655.36 µs.
        assert_eq!(c.advance(64 * 1024, t0), Duration::from_nanos(655_360));
        // A skipped wait stays owed: the third group waits for both.
        assert_eq!(c.advance(64 * 1024, t0), Duration::from_nanos(1_310_720));
        // Idle past the clock: restart at `now`, nothing owed.
        let later = t0 + Duration::from_millis(10);
        assert_eq!(c.advance(64 * 1024, later), Duration::ZERO);
        assert_eq!(c.advance(1, later), Duration::from_nanos(655_360));
        let much_later = t0 + Duration::from_millis(50);
        c.wake_gap(much_later);
        assert_eq!(c.ahead(much_later), WAKE_GAP);
    }

    /// 36 kB at 80 Mbit/s is a 3.6 ms group. A caller 2 ms late books one group after the
    /// last deadline; one 50 ms late restarts at `now`, owing no catch-up.
    #[test]
    fn a_late_caller_keeps_at_most_one_group_of_debt() {
        let group = Duration::from_micros(3_600);
        let t0 = Instant::now();
        let mut c = GroupClock::new(80_000_000, t0);
        c.advance(36_000, t0);
        let late = t0 + group + Duration::from_millis(2);
        assert_eq!(c.advance(36_000, late), Duration::ZERO);
        assert_eq!(c.ahead(late), Duration::from_micros(1_600));

        let idle = late + Duration::from_millis(50);
        assert_eq!(c.advance(36_000, idle), Duration::ZERO);
        assert_eq!(c.ahead(idle), group);
    }

    /// A wait from the sleep floor up sleeps, one above the spin floor spins, a shorter one
    /// goes now and stays owed.
    #[test]
    fn short_waits_spin_and_tiny_ones_go_now() {
        assert_eq!(hold(Duration::from_micros(800)), Hold::Sleep);
        assert_eq!(hold(SLEEP_FLOOR), Hold::Sleep);
        assert_eq!(hold(Duration::from_micros(120)), Hold::Spin);
        assert_eq!(hold(SPIN_FLOOR), Hold::Spin);
        assert_eq!(hold(Duration::from_micros(5)), Hold::Go);
        // At the clock: a 100 µs debt spins and holds the group back.
        let t0 = Instant::now();
        let mut c = GroupClock::new(8_000_000_000, t0);
        c.advance(100_000, t0);
        assert!(c.wait_or_spin(1_000, t0));
        assert!(!c.wait_or_spin(1_000, t0 + Duration::from_millis(5)));
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
