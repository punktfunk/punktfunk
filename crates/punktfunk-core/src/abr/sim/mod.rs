//! Link simulator: today's controller in a closed loop with models of the
//! link, the host and the client.
//!
//! Integer arithmetic (kbps, bytes, µs) and an inline splitmix64 seeded per
//! scenario, so a run is bit-identical on macOS arm64 and Linux x86_64 and
//! survives a `rand` bump. Time is a 1 ms tick and an `Instant` is
//! `base + Duration`. [`scenarios`] holds the scenario table and the field
//! calibration; [`baseline`] pins what today's controller does on each one.
//!
//! Sessions of one scenario share one modelled link, so the host's
//! [`governor`] runs over them here exactly as it runs over the sessions of
//! one client address in the field — the same function, fed the facts a host
//! has.
//!
//! Nothing here changes production behaviour: the controller is the fixed
//! point, and a behaviour that will not reproduce is a finding about the
//! model, not licence to tune the controller.

mod baseline;
mod client;
mod host;
mod link;
mod scenarios;

use crate::abr::governor;
use crate::abr::metrics::{self, Metrics};
use crate::quic::AckReason;
use client::{Action, Client, ClientCfg, WindowRec, PROBE_FRAME};
use host::{Host, HostCfg};
use link::{Link, LinkCfg};
use std::time::Instant;

/// splitmix64. One line of state, no dependency, identical everywhere.
#[derive(Clone, Debug)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x2545F491_4F6CDD1D) ^ 0x9E3779B9_7F4A7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B9_7F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D_1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB_133111EB);
        z ^ (z >> 31)
    }

    /// Uniform over `0..n`.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    fn chance_ppm(&mut self, ppm: u32) -> bool {
        self.below(1_000_000) < u64::from(ppm)
    }
}

struct SessionCfg {
    /// When this session connects. A newcomer joins a link already in use.
    pub join_ms: u64,
    /// When it disconnects, leaving the path to whoever is left.
    pub leave_ms: u64,
    pub host: HostCfg,
    pub client: ClientCfg,
}

struct Scenario {
    pub name: &'static str,
    pub seed: u64,
    pub duration_ms: u64,
    pub link: LinkCfg,
    pub sessions: Vec<SessionCfg>,
    /// Rate this scenario could hold if nothing went wrong — the yardstick
    /// for "time to 90 % of achievable".
    pub achievable_kbps: u32,
    /// One unrecoverable frame injected here, for the recovery metric.
    pub blip_at_ms: Option<u64>,
}

/// One session's bring-up ramp: every step it asked for, and what it came
/// to (`None` = it never stopped, or never ran).
struct RampTrace {
    pub asks: Vec<(u64, u32)>,
    pub done: Option<(u64, crate::abr::probe::RampSummary)>,
}

struct Run {
    pub metrics: Metrics,
    pub windows: Vec<Vec<WindowRec>>,
    pub ramps: Vec<RampTrace>,
    /// Every `SetBitrate` each session sent, when it sent it: the asks a
    /// window close never saw (the ramp's opening rate, the pin's verdict).
    pub asks: Vec<Vec<(u64, u32)>>,
    pub repair: RepairTally,
}

/// Session 0's loss repair over a run: recovery frames the host sent in
/// answer to an ask, frames the client lost, and the bytes repair spent past
/// the budget (NACK resends, and each wave's excess over an ordinary frame).
#[derive(Clone, Copy, Debug)]
struct RepairTally {
    pub waves: u32,
    pub lost: u64,
    pub above_budget_bytes: u64,
}

impl Run {
    /// Every rate the first session asked the host for, in order.
    fn steps(&self) -> Vec<u32> {
        let mut out = Vec::new();
        let mut last = 0;
        for w in &self.windows[0] {
            if w.rate_kbps != last {
                out.push(w.rate_kbps);
                last = w.rate_kbps;
            }
        }
        out
    }

    /// Every session's rate on one time base, sampled each second: the shape
    /// a shared path is judged by. A session that has not joined, or has
    /// left, reads `0`.
    fn pairs(&self) -> Vec<(u64, Vec<u32>)> {
        let last = self
            .windows
            .iter()
            .filter_map(|w| w.last())
            .map(|w| w.t_ms)
            .max()
            .unwrap_or(0);
        (0..=last / 1_000)
            .map(|s| {
                let t_ms = s * 1_000;
                let rates = self
                    .windows
                    .iter()
                    .map(|ws| match ws.iter().rev().find(|w| w.t_ms <= t_ms) {
                        Some(w) if w.t_ms + 2_000 >= t_ms => w.rate_kbps,
                        _ => 0,
                    })
                    .collect();
                (t_ms, rates)
            })
            .collect()
    }

    /// Windows where the controller asked for less than it had.
    fn cuts(&self) -> Vec<&WindowRec> {
        self.windows[0]
            .iter()
            .filter(|w| w.cut_from_kbps.is_some())
            .collect()
    }
}

struct Session {
    join_ms: u64,
    leave_ms: u64,
    host: Host,
    client: Client,
    /// What the host last told this session, the most its group was seen to
    /// carry between them, and whether it had a group at all — the three the
    /// host keeps per session (`session_status::AbrShare`).
    share_kbps: Option<u32>,
    path_kbps: u32,
    grouped: bool,
    /// When each of this session's up-moves may next go out
    /// (`native/control.rs` `ShareClocks`).
    room_at_ms: u64,
    lift_at_ms: u64,
}

impl Session {
    fn live(&self, now_ms: u64) -> bool {
        (self.join_ms..self.leave_ms).contains(&now_ms)
    }

    /// This session as the host's governor sees it: the encoder target it set,
    /// the wire rate it put out, what the client's last delivery report said
    /// arrived, and whether the source is still.
    fn member(&self, now_ms: u64) -> governor::Member {
        governor::Member {
            automatic: self.client.automatic(),
            current_kbps: self.host.budget_kbps(),
            offered_kbps: self.host.offered_kbps(),
            delivered_kbps: self.host.delivered_kbps(),
            idle: self.host.idle(now_ms),
            share_kbps: self.share_kbps,
            streaming: self.host.streaming(),
        }
    }

    /// The two clocks an up-move rides, taken as the host takes them.
    fn clocks(&mut self, now_ms: u64) -> governor::Clocks {
        let out = governor::Clocks {
            room: now_ms >= self.room_at_ms,
            lift: now_ms >= self.lift_at_ms,
        };
        if out.room {
            self.room_at_ms = now_ms + governor::SHARE_CLOCK.as_millis() as u64;
        }
        if out.lift {
            self.lift_at_ms = now_ms + governor::SHARE_LIFT_CLOCK.as_millis() as u64;
        }
        out
    }
}

/// Session `i`'s delivery report reached the host: it re-takes the shares of the
/// path this session is on and applies its own.
///
/// The report is the only boundary the host has, so this is where the policy
/// runs — `session_status::share_for`, driven by `native/control.rs`. Every
/// member computes the whole group and applies only its own share, because a
/// share is only ever sent down the control stream its own task owns.
fn govern(sessions: &mut [Session], i: usize, now_ms: u64) {
    let group: Vec<usize> = (0..sessions.len())
        .filter(|&k| sessions[k].live(now_ms))
        .collect();
    let Some(mine) = group.iter().position(|&k| k == i) else {
        return;
    };
    let facts: Vec<governor::Member> = group.iter().map(|&k| sessions[k].member(now_ms)).collect();
    if group.len() < 2 {
        // Alone on the path: hand over the whole of what the group proved it
        // carried, once. The wall this session measured beside them was their
        // residual, and nobody but the host knows they have gone.
        let path = std::mem::take(&mut sessions[i].path_kbps);
        if std::mem::take(&mut sessions[i].grouped) && path > 0 {
            sessions[i].share_kbps = Some(path);
            sessions[i].host.govern(now_ms, path);
        }
        return;
    }
    sessions[i].grouped = true;
    // The most the path has been seen to carry, until the group is short of
    // what it offers: that is the path being re-measured, and a figure from
    // before it changed expires there (L1).
    // A member that has not reported yet leaves the group unmeasured, and a
    // figure it is not in must not be remembered as the path.
    let Some(carried) = governor::path_kbps(&facts) else {
        return;
    };
    let path = if governor::crowded(&facts) {
        carried
    } else {
        sessions[i].path_kbps.max(carried)
    };
    sessions[i].path_kbps = path;
    let clocks = sessions[i].clocks(now_ms);
    if let Some(share) = governor::shares(&facts, path, clocks)[mine] {
        sessions[i].share_kbps = (share != governor::NO_SHARE_KBPS).then_some(share);
        sessions[i].host.govern(now_ms, share);
    }
}

fn run(sc: &Scenario) -> Run {
    let base = Instant::now();
    let mut link = Link::new(sc.link.clone(), sc.seed);
    let mut sessions: Vec<Session> = sc
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            // A session's clock starts when it joins, not when the run does:
            // its first report window is 750 ms after its first frame.
            let joined = base + std::time::Duration::from_millis(s.join_ms);
            let seed = sc.seed ^ (0x51_u64 << (i * 8));
            let mut client = Client::new(s.client.clone(), seed, base, joined);
            if let Some(at) = sc.blip_at_ms {
                if i == 0 {
                    client.inject_lost_frame(at);
                }
            }
            Session {
                join_ms: s.join_ms,
                leave_ms: s.leave_ms,
                host: Host::new(
                    s.host.clone(),
                    // A pinned session's Welcome rate is the pin itself.
                    s.client.pin_kbps.unwrap_or(s.client.start_kbps),
                    sc.seed ^ (0x9A << i),
                    joined,
                ),
                client,
                share_kbps: None,
                path_kbps: 0,
                grouped: false,
                room_at_ms: s.join_ms + governor::SHARE_CLOCK.as_millis() as u64,
                lift_at_ms: s.join_ms + governor::SHARE_LIFT_CLOCK.as_millis() as u64,
            }
        })
        .collect();
    let mut drained = Vec::new();
    let mut actions = Vec::new();
    let (mut offered_10s, mut capacity_10s) = (0u64, 0u64);
    // Sessions whose delivery report arrived this millisecond: the host's only
    // chance to re-take their shares.
    let mut reported: Vec<usize> = Vec::new();

    for now in 0..sc.duration_ms {
        if now < 10_000 {
            capacity_10s += u64::from(link.capacity_kbps(now)) / 8;
        }
        for (i, s) in sessions.iter_mut().enumerate() {
            if !s.live(now) {
                continue;
            }
            let id = i as u8;
            let mut offer = |link: &mut Link, client: &mut Client, frame: u32, bytes: u64| {
                if bytes == 0 {
                    return;
                }
                if now < 10_000 {
                    offered_10s += bytes;
                }
                let refused = link.offer(now, id, frame, bytes);
                if refused > 0 {
                    if let Some(shards) = client.refuse(frame, refused) {
                        let draw = link.draw_loss(shards);
                        client.complete(frame, draw, now, link.rtt_ms());
                    }
                }
            };
            // Filler first: the burst pumps between AUs, so a video frame
            // lands on a queue the filler has just refilled. Offering video
            // first would hand it the whole of each tick's drain and the
            // burst would cost the session nothing.
            let filler = s.host.probe_release(now);
            offer(&mut link, &mut s.client, PROBE_FRAME, filler);
            if let Some(f) = s.host.tick(now) {
                s.client.expect(&f, now);
                if let Some((tail, bytes)) = s.host.take_flush() {
                    offer(&mut link, &mut s.client, tail, bytes);
                }
                let burst = s.host.burst_of(&f);
                offer(&mut link, &mut s.client, f.id, burst);
            }
            let (frame, bytes) = s.host.release(now);
            offer(&mut link, &mut s.client, frame, bytes);
        }
        drained.clear();
        link.tick(now, &mut drained);
        for &(id, frame, bytes) in &drained {
            let s = &mut sessions[id as usize];
            if frame == PROBE_FRAME {
                s.client.deliver_probe(bytes, now + link.base_delay_ms());
            } else if let Some(shards) = s.client.deliver(frame, bytes, now + link.base_delay_ms())
            {
                let draw = link.draw_loss(shards);
                s.client
                    .complete(frame, draw, now + link.base_delay_ms(), link.rtt_ms());
            }
        }
        reported.clear();
        for (i, s) in sessions.iter_mut().enumerate() {
            if !s.live(now) {
                continue;
            }
            actions.clear();
            s.client.tick(now, base, &mut actions);
            for a in &actions {
                match *a {
                    Action::SetBitrate(kbps) => s.host.on_set_bitrate(now, kbps),
                    Action::Keyframe => s.host.on_keyframe_request(now),
                    Action::Loss { ppm, unrecovered } => s.host.on_loss_report(ppm, unrecovered),
                    Action::Delivery(packets) => {
                        s.host.on_delivery_report(
                            base + std::time::Duration::from_millis(now),
                            packets,
                        );
                        reported.push(i);
                    }
                    Action::Probe {
                        target_kbps,
                        duration_ms,
                    } => s.host.on_probe_request(now, target_kbps, duration_ms),
                }
            }
            if let Some(done) = s.host.probe_done(now) {
                s.client.on_probe_result(now, done);
            }
            if let Some((kbps, why)) = s.host.apply_pending(now) {
                s.client.push_ack(kbps, why);
            }
            if let Some(share) = s.host.apply_governor(now) {
                s.client.push_ack(share, AckReason::Governor);
            }
        }
        for &i in &reported {
            govern(&mut sessions, i, now);
        }
    }
    let metrics = measure(sc, &sessions, &mut link, offered_10s, capacity_10s);
    let repair = RepairTally {
        waves: sessions[0].host.waves,
        lost: sessions[0].client.frames_dropped(),
        above_budget_bytes: sessions[0].host.wave_excess_bytes + sessions[0].client.resent_bytes,
    };
    let mut windows = Vec::new();
    let mut ramps = Vec::new();
    let mut asks = Vec::new();
    for s in sessions {
        let c = s.client;
        windows.push(c.windows);
        ramps.push(RampTrace {
            asks: c.ramp_asks,
            done: c.ramp_done,
        });
        asks.push(c.set_asks);
    }
    Run {
        metrics,
        windows,
        ramps,
        asks,
        repair,
    }
}

/// Score the run with the metrics the netem rig also prints
/// ([`crate::abr::metrics`]): the two tiers stay comparable because neither
/// owns a definition.
fn measure(
    sc: &Scenario,
    sessions: &[Session],
    link: &mut Link,
    offered_10s: u64,
    capacity_10s: u64,
) -> Metrics {
    let per_session: Vec<Vec<metrics::MetricWindow>> = sessions
        .iter()
        .map(|s| s.client.windows.iter().map(WindowRec::metric).collect())
        .collect();
    let refs: Vec<&[metrics::MetricWindow]> = per_session.iter().map(Vec::as_slice).collect();
    metrics::measure(&metrics::Run {
        sessions: &refs,
        owd_ms: &sessions[0].client.owd_samples,
        base_delay_ms: link.base_delay_ms() as u32,
        duration_ms: sc.duration_ms,
        achievable_kbps: sc.achievable_kbps,
        offered_10s,
        capacity_10s,
        blip_at_ms: sc.blip_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generator is the same stream everywhere, so a baseline row is a
    /// fact and not a platform's opinion.
    #[test]
    fn the_generator_is_pinned() {
        let mut r = Rng::new(7);
        assert_eq!(
            [r.next_u64(), r.next_u64(), r.next_u64()],
            [
                16_557_362_563_216_862_149,
                430_200_180_043_962_517,
                5_998_290_083_107_941_422
            ]
        );
        let mut r = Rng::new(7);
        let hits = (0..10_000).filter(|_| r.chance_ppm(250_000)).count();
        assert_eq!(hits, 2_481, "a quarter of the draws, to the draw");
    }

    /// Ten minutes of a 1.3 Gbps session in under a second, unoptimized — the
    /// budget that keeps a scenario a unit test.
    #[test]
    fn ten_minutes_at_the_top_of_the_range_simulates_in_under_a_second() {
        let started = Instant::now();
        let r = run(&scenarios::fat_pipe_10min());
        let took = started.elapsed();
        assert!(
            r.windows[0].last().is_some_and(|w| w.rate_kbps > 1_000_000),
            "the session must actually reach the top of the range"
        );
        assert!(took.as_millis() < 1_000, "ten minutes took {took:?}");
    }
}
