//! Link simulator: today's controller in a closed loop with models of the
//! link, the host and the client.
//!
//! Integer arithmetic (kbps, bytes, µs) and an inline splitmix64 seeded per
//! scenario, so a run is bit-identical on macOS arm64 and Linux x86_64 and
//! survives a `rand` bump. Time is a 1 ms tick and an `Instant` is
//! `base + Duration`. [`scenarios`] holds the scenario table and the field
//! calibration; [`baseline`] pins what today's controller does on each one.
//!
//! Nothing here changes production behaviour: the controller is the fixed
//! point, and a behaviour that will not reproduce is a finding about the
//! model, not licence to tune the controller.

mod baseline;
mod client;
mod host;
mod link;
mod scenarios;

use crate::abr::metrics::{self, Metrics};
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

struct Run {
    pub metrics: Metrics,
    pub windows: Vec<Vec<WindowRec>>,
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
    host: Host,
    client: Client,
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
            let mut client = Client::new(s.client.clone(), sc.seed ^ (0x51_u64 << (i * 8)), joined);
            if let Some(at) = sc.blip_at_ms {
                if i == 0 {
                    client.inject_lost_frame(at);
                }
            }
            Session {
                join_ms: s.join_ms,
                host: Host::new(s.host.clone(), s.client.start_kbps, sc.seed ^ (0x9A << i)),
                client,
            }
        })
        .collect();
    let mut drained = Vec::new();
    let mut actions = Vec::new();
    let (mut offered_10s, mut capacity_10s) = (0u64, 0u64);

    for now in 0..sc.duration_ms {
        if now < 10_000 {
            capacity_10s += u64::from(link.capacity_kbps(now)) / 8;
        }
        for (i, s) in sessions.iter_mut().enumerate() {
            if now < s.join_ms {
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
                        client.complete(frame, draw, now);
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
            } else if let Some(shards) = s.client.deliver(frame, bytes) {
                let draw = link.draw_loss(shards);
                s.client.complete(frame, draw, now + link.base_delay_ms());
            }
        }
        for s in sessions.iter_mut() {
            if now < s.join_ms {
                continue;
            }
            actions.clear();
            s.client.tick(now, base, &mut actions);
            for a in &actions {
                match *a {
                    Action::SetBitrate(kbps) => s.host.on_set_bitrate(now, kbps),
                    Action::Keyframe => s.host.on_keyframe_request(now),
                    Action::Loss { ppm, unrecovered } => s.host.on_loss_report(ppm, unrecovered),
                    Action::Probe {
                        target_kbps,
                        duration_ms,
                    } => s.host.on_probe_request(now, target_kbps, duration_ms),
                }
            }
            if let Some(host_ms) = s.host.probe_done(now) {
                s.client.on_probe_result(now, host_ms);
            }
            if let Some(kbps) = s.host.apply_pending(now) {
                s.client.push_ack(kbps);
            }
        }
    }
    let metrics = measure(sc, &sessions, &mut link, offered_10s, capacity_10s);
    Run {
        metrics,
        windows: sessions.into_iter().map(|s| s.client.windows).collect(),
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
