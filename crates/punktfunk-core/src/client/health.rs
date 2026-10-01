//! The speed test and the network check, one routine each for every shell.
//!
//! The speed test: the ceiling the bring-up ramp proved, then one clean round under it.
//! Toward a [`HOST_CAP2_RAMP`] host the ramp runs during bring-up and proves what the link
//! carries; the clean round then measures loss and jitter at half of that, a rate the link
//! holds. Toward an older host the single blast stays, and its loss is the blast's, so no
//! shell shows it.
//!
//! The network check builds on it: the facts of both ends, two shaped legs at the clean
//! round's rate (what video does, and what a capped profile would), a slow round when the
//! clean one lost anything, and [`judge`], which names what the link does and which
//! delivery profile, if any, helps. Ids and numbers cross every ABI; the words are each
//! shell's.

use super::{NativeClient, ProbeOutcome};
use crate::quic::{HostFacts, ProbeShaped, HOST_CAP2_RAMP};
use crate::transport::ifinfo::LinkFacts;
use crate::transport::IFACE_KIND_WIFI;
use std::time::{Duration, Instant};

pub const CLEAN_ROUND_MS: u32 = 2_000;
/// The clean round's share of the ceiling: under the wall by more than the session itself
/// keeps, so its loss is the path's, not the round's.
pub const CLEAN_ROUND_PCT: u32 = 50;
/// The blast toward a host without a ramp: far more than any link carries, so the link is
/// what limits the answer.
pub const BLAST_KBPS: u32 = 3_000_000;
pub const BLAST_MS: u32 = 2_000;
/// The ramp stops within the bring-up gap; longer means it was cut short or declined.
const RAMP_WAIT: Duration = Duration::from_secs(4);
/// The clean round runs after the first video frame: until then the host serves every
/// probe as a ramp step, 50 ms long, and a round cut to that would also spend the one
/// burst the spacing allows. A pipeline builds in two to three seconds.
const VIDEO_WAIT: Duration = Duration::from_secs(8);
/// A round that never reports is a dead session, not a slow link.
const POLL_BUDGET: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The last shards land after the host's report; counted now, they are not loss.
const SETTLE: Duration = Duration::from_millis(400);

/// One round at a rate the link holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct CleanRound {
    pub rate_kbps: u32,
    pub loss_pct: f32,
    /// p99 − p50 of the probe inter-arrival gap, µs, to a tenth of a millisecond.
    pub jitter_us: u32,
    pub reorders: u32,
    pub outcome: ProbeOutcome,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SpeedReport {
    /// What the link carries, kbps: the ramp's proof, or a blast's delivered rate.
    pub ceiling_kbps: u32,
    /// The ramp found the wall; `false` is a floor under the capacity, or a blast.
    pub wall: bool,
    /// `None` toward a host without a ramp — then nothing honest can be said about loss.
    pub clean: Option<CleanRound>,
    /// The blast's own reading, toward a host without a ramp.
    pub blast: Option<ProbeOutcome>,
}

#[derive(Debug)]
pub enum SpeedError {
    Request(crate::PunktfunkError),
    /// The host answered a round with an all-zero report.
    Declined,
    Timeout,
}

/// Headroom a recommendation keeps under the ceiling: what the session opens against. In
/// this order, so every client recommends the same kilobit.
pub fn recommended_kbps(ceiling_kbps: u32) -> u32 {
    ceiling_kbps / 10 * 7
}

/// The rate the clean round runs at.
pub fn clean_rate_kbps(ceiling_kbps: u32) -> u32 {
    ceiling_kbps / 100 * CLEAN_ROUND_PCT
}

/// Blocking: the ramp's result, then the clean round, polled to its report. `progress` sees
/// the round's live throughput at every poll.
pub fn speed_test(
    c: &NativeClient,
    mut progress: impl FnMut(u32),
) -> Result<SpeedReport, SpeedError> {
    let ramp = (c.host_caps2() & HOST_CAP2_RAMP != 0)
        .then(|| wait_for_ramp(c))
        .flatten();
    let Some(ceiling_kbps) = ramp
        .as_ref()
        .map(|r| r.outcome.proven_kbps)
        .filter(|&k| k > 0)
    else {
        let blast = run_round(c, BLAST_KBPS, BLAST_MS, &mut progress)?;
        return Ok(SpeedReport {
            ceiling_kbps: blast.throughput_kbps,
            wall: false,
            clean: None,
            blast: Some(blast),
        });
    };
    if !c.probe_only() {
        wait_for_video(c);
    }
    let rate_kbps = clean_rate_kbps(ceiling_kbps);
    let outcome = run_round(c, rate_kbps, CLEAN_ROUND_MS, &mut progress)?;
    Ok(SpeedReport {
        ceiling_kbps,
        wall: ramp.is_some_and(|r| r.outcome.wall),
        clean: Some(CleanRound {
            rate_kbps,
            loss_pct: outcome.loss_pct,
            jitter_us: outcome.gap_p99_us.saturating_sub(outcome.gap_p50_us),
            reorders: outcome.reorders,
            outcome,
        }),
        blast: None,
    })
}

fn wait_for_ramp(c: &NativeClient) -> Option<crate::abr::RampRecord> {
    let deadline = Instant::now() + RAMP_WAIT;
    loop {
        if let Some(r) = c.abr_ramp() {
            return Some(r);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The first completed AU, which is the ramp window closing on the host. A session that
/// shows no video within [`VIDEO_WAIT`] goes on anyway: the round then measures what it can.
/// A probe-only session never waits: its host serves every probe in full.
fn wait_for_video(c: &NativeClient) {
    let deadline = Instant::now() + VIDEO_WAIT;
    while Instant::now() < deadline {
        match c.next_frame(Duration::from_millis(100)) {
            Ok(_) => return,
            Err(crate::PunktfunkError::NoFrame) => {}
            Err(_) => return,
        }
    }
}

/// One burst, polled to the host's report. An all-zero report is a decline.
fn run_round(
    c: &NativeClient,
    target_kbps: u32,
    duration_ms: u32,
    progress: &mut impl FnMut(u32),
) -> Result<ProbeOutcome, SpeedError> {
    c.request_probe(target_kbps, duration_ms)
        .map_err(SpeedError::Request)?;
    poll_round(c, progress)
}

fn run_shaped(
    c: &NativeClient,
    shape: ProbeShaped,
    progress: &mut impl FnMut(u32),
) -> Result<ProbeOutcome, SpeedError> {
    c.request_probe_shaped(shape).map_err(SpeedError::Request)?;
    poll_round(c, progress)
}

fn poll_round(
    c: &NativeClient,
    progress: &mut impl FnMut(u32),
) -> Result<ProbeOutcome, SpeedError> {
    let deadline = Instant::now() + POLL_BUDGET;
    loop {
        std::thread::sleep(POLL_INTERVAL);
        let now = c.probe_result();
        if now.done {
            std::thread::sleep(SETTLE);
            let r = c.probe_result();
            if r.wire_packets_sent == 0 && r.host_bytes == 0 {
                return Err(SpeedError::Declined);
            }
            return Ok(r);
        }
        progress(now.throughput_kbps);
        if Instant::now() > deadline {
            return Err(SpeedError::Timeout);
        }
    }
}

/// What this machine's OS and socket say.
#[derive(Clone, Copy, Debug, Default)]
pub struct ClientFacts {
    pub link: LinkFacts,
    /// The data socket's granted receive buffer, KiB.
    pub rcvbuf_kb: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LegShape {
    /// Sixty bursts a second at line rate: what video does.
    FrameBursts,
    /// Sixty bursts a second in 64 KiB groups at 0.8 Gbit/s: the capped profile.
    Capped,
}

/// One shaped leg at the clean round's rate.
#[derive(Clone, Copy, Debug)]
pub struct Leg {
    pub shape: LegShape,
    pub outcome: ProbeOutcome,
    /// This socket's receive-buffer drops across the leg; `None` = not sampled.
    pub socket_drops: Option<u64>,
}

/// What the check found. The id names the text each shell shows; `numbers` are its figures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FindingId {
    /// The host's port is faster than this device's; a capped burst stops the loss.
    SpeedMismatch = 1,
    /// This device loses the head of a line-rate burst, capped or not, and nothing smooth.
    BurstIntolerant = 2,
    /// Loss happened in this device's own receive buffer, or the OS caps it small.
    ReceiveBuffer = 3,
    /// Loss or jitter at a rate no link refuses: cable, port, duplex or driver.
    LinkFault = 4,
    /// The clean round's arrivals spread: something on the path buffers.
    QueueBuildUp = 5,
    /// The host's send buffer refused packets, or the OS caps it small.
    HostSendBuffer = 6,
    /// This device is on Wi-Fi.
    Wifi = 7,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Severity {
    Note = 0,
    Warn = 1,
    Bad = 2,
}

#[derive(Clone, Copy, Debug)]
pub struct Finding {
    pub id: FindingId,
    pub severity: Severity,
    /// The figures behind the finding, in the order the id's text names them.
    pub numbers: [u32; 3],
    /// The delivery profile that helps (`1` capped, `2` smooth), when one does.
    pub profile: Option<u8>,
}

#[derive(Clone, Debug, Default)]
pub struct HealthReport {
    pub speed: SpeedReport,
    pub client: ClientFacts,
    pub host: Option<HostFacts>,
    /// Empty toward a host that did not answer the delivery tag, or without a clean round.
    pub legs: Vec<Leg>,
    /// The slow round, run when the clean round lost anything.
    pub slow: Option<ProbeOutcome>,
    pub findings: Vec<Finding>,
}

pub const LEG_MS: u32 = 1_000;
pub const LEG_HZ: u16 = 60;
pub const CAPPED_GROUP_BYTES: u32 = 64 * 1024;
pub const CAPPED_RATE_KBPS: u32 = 800_000;
/// A rate no link refuses, for the link-fault round.
pub const SLOW_ROUND_KBPS: u32 = 5_000;
pub const SLOW_ROUND_MS: u32 = 3_000;
/// The clean round's loss that sends the check looking for a link fault.
const SLOW_ROUND_TRIGGER_PCT: f32 = 0.1;
/// A leg that loses this much is a finding.
const LOSS_NOTICE_PCT: f32 = 0.5;
/// A buffer under this is what the OS default leaves a 4K frame no room in.
const BUFFER_WANT_KB: u32 = 8 * 1024;
/// Gap spread in the clean round that says something buffers rather than drops.
const QUEUE_JITTER_US: u32 = 20_000;
/// Jitter at the slow round's rate that is the wire's fault.
const FAULT_JITTER_US: u32 = 20_000;

/// Blocking: the speed test, both ends' facts, the shaped legs, the slow round, then the
/// verdicts. `progress` sees every round's live throughput.
pub fn health_check(
    c: &NativeClient,
    mut progress: impl FnMut(u32),
) -> Result<HealthReport, SpeedError> {
    let speed = speed_test(c, &mut progress)?;
    let client = ClientFacts {
        link: c
            .local_ip()
            .map_or_else(LinkFacts::default, crate::transport::ifinfo::link_facts),
        rcvbuf_kb: c.recv_buffer_kb(),
    };
    let host = c.host_facts();
    let mut legs = Vec::new();
    if let (Some(clean), Some(_)) = (speed.clean, c.delivery()) {
        for shape in [LegShape::FrameBursts, LegShape::Capped] {
            let (group_bytes, group_rate_kbps) = match shape {
                LegShape::FrameBursts => (0, 0),
                LegShape::Capped => (CAPPED_GROUP_BYTES, CAPPED_RATE_KBPS),
            };
            let before = c.socket_drops();
            let outcome = run_shaped(
                c,
                ProbeShaped {
                    target_kbps: clean.rate_kbps,
                    duration_ms: LEG_MS,
                    burst_hz: LEG_HZ,
                    group_bytes,
                    group_rate_kbps,
                },
                &mut progress,
            )?;
            let socket_drops = before
                .zip(c.socket_drops())
                .map(|(b, a)| a.saturating_sub(b));
            legs.push(Leg {
                shape,
                outcome,
                socket_drops,
            });
        }
    }
    let slow = match speed.clean {
        Some(cl) if cl.loss_pct >= SLOW_ROUND_TRIGGER_PCT => {
            Some(run_round(c, SLOW_ROUND_KBPS, SLOW_ROUND_MS, &mut progress)?)
        }
        _ => None,
    };
    let findings = judge(&speed, &client, host.as_ref(), &legs, slow.as_ref());
    Ok(HealthReport {
        speed,
        client,
        host,
        legs,
        slow,
        findings,
    })
}

fn pct_x100(pct: f32) -> u32 {
    (pct * 100.0).round().max(0.0) as u32
}

fn jitter_of(o: &ProbeOutcome) -> u32 {
    o.gap_p99_us.saturating_sub(o.gap_p50_us)
}

/// The verdicts, from the measurements alone. A rule that needs a figure nobody sampled
/// does not fire; a rule that can only be half sure says `Warn`.
pub fn judge(
    speed: &SpeedReport,
    client: &ClientFacts,
    host: Option<&HostFacts>,
    legs: &[Leg],
    slow: Option<&ProbeOutcome>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    let clean_loss = speed.clean.map(|c| c.loss_pct);
    let bursts = legs.iter().find(|l| l.shape == LegShape::FrameBursts);
    let capped = legs.iter().find(|l| l.shape == LegShape::Capped);
    let bursts_loss = bursts.map(|l| l.outcome.loss_pct);
    let capped_loss = capped.map(|l| l.outcome.loss_pct);
    let host_mbps = host.map_or(0, |h| h.link_mbps);
    let client_mbps = client.link.mbps;
    let ports_differ = host_mbps > 0 && client_mbps > 0 && host_mbps > client_mbps;
    let drops_in_bursts = bursts.and_then(|l| l.socket_drops);

    // The capped leg heals what the bursts lose, or the ports differ and it helps at all.
    // That loss is explained; the receiver rule below does not read it again.
    let mismatch = matches!((bursts_loss, capped_loss), (Some(b), Some(cp))
        if b >= LOSS_NOTICE_PCT && (cp <= b / 4.0 || (ports_differ && cp < b)));
    if mismatch {
        out.push(Finding {
            id: FindingId::SpeedMismatch,
            severity: Severity::Bad,
            numbers: [host_mbps, client_mbps, pct_x100(bursts_loss.unwrap_or(0.0))],
            profile: Some(1),
        });
    }
    // Loss in this device's own buffer, or a buffer the OS keeps small.
    let rcvbuf_small = client.rcvbuf_kb > 0 && client.rcvbuf_kb < BUFFER_WANT_KB;
    if drops_in_bursts.is_some_and(|d| d > 0) || rcvbuf_small {
        let drops = drops_in_bursts.unwrap_or(0);
        out.push(Finding {
            id: FindingId::ReceiveBuffer,
            severity: if drops > 0 {
                Severity::Bad
            } else {
                Severity::Warn
            },
            numbers: [drops.min(u64::from(u32::MAX)) as u32, client.rcvbuf_kb, 0],
            profile: Some(2),
        });
    }
    // Bursts lose, capped bursts lose about as much, smooth does not, and the loss was not
    // this socket's. Sure only where the socket's drops were sampled; `Warn` where the
    // platform keeps no such figure.
    if let (Some(b), Some(cp), Some(cl)) = (bursts_loss, capped_loss, clean_loss) {
        let own_drops = drops_in_bursts.is_some_and(|d| d > 0);
        if !mismatch && b >= LOSS_NOTICE_PCT && cp >= b / 2.0 && cl <= b / 4.0 && !own_drops {
            out.push(Finding {
                id: FindingId::BurstIntolerant,
                severity: if drops_in_bursts.is_some() {
                    Severity::Bad
                } else {
                    Severity::Warn
                },
                numbers: [pct_x100(b), pct_x100(cp), pct_x100(cl)],
                profile: Some(2),
            });
        }
    }
    // Loss or jitter at a rate no link refuses, on a wire.
    if let Some(s) = slow {
        let jitter = jitter_of(s);
        if (s.loss_pct >= SLOW_ROUND_TRIGGER_PCT || jitter >= FAULT_JITTER_US)
            && client.link.kind != IFACE_KIND_WIFI
        {
            out.push(Finding {
                id: FindingId::LinkFault,
                severity: Severity::Bad,
                numbers: [pct_x100(s.loss_pct), jitter, 0],
                profile: None,
            });
        }
    }
    if let Some(cl) = speed.clean {
        if cl.jitter_us >= QUEUE_JITTER_US {
            out.push(Finding {
                id: FindingId::QueueBuildUp,
                severity: Severity::Warn,
                numbers: [cl.jitter_us, cl.rate_kbps, 0],
                profile: None,
            });
        }
    }
    let send_dropped = legs
        .iter()
        .map(|l| l.outcome.send_dropped)
        .sum::<u32>()
        .saturating_add(speed.clean.map_or(0, |c| c.outcome.send_dropped));
    let sndbuf_kb = host.map_or(0, |h| h.sndbuf_kb);
    if send_dropped > 0 || (sndbuf_kb > 0 && sndbuf_kb < BUFFER_WANT_KB) {
        out.push(Finding {
            id: FindingId::HostSendBuffer,
            severity: Severity::Warn,
            numbers: [send_dropped, sndbuf_kb, 0],
            profile: None,
        });
    }
    if client.link.kind == IFACE_KIND_WIFI {
        let lossy = bursts_loss.is_some_and(|b| b >= LOSS_NOTICE_PCT);
        out.push(Finding {
            id: FindingId::Wifi,
            severity: if lossy {
                Severity::Warn
            } else {
                Severity::Note
            },
            numbers: [pct_x100(bursts_loss.unwrap_or(0.0)), client_mbps, 0],
            profile: lossy.then_some(2),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The clean round runs at half the ceiling and the recommendation keeps 30 % back —
    /// truncating, so neither exceeds what was proved.
    #[test]
    fn the_clean_round_runs_under_the_ceiling() {
        assert_eq!(clean_rate_kbps(940_000), 470_000);
        assert_eq!(clean_rate_kbps(99), 0);
        assert_eq!(recommended_kbps(100_000), 70_000);
        assert_eq!(recommended_kbps(9), 0);
        assert_eq!(recommended_kbps(412_345), 288_638);
        assert!(clean_rate_kbps(1_000_000) < recommended_kbps(1_000_000));
    }

    /// Jitter is the spread of the gap, never negative, and comes from the bucket edges.
    #[test]
    fn jitter_comes_from_the_arrival_ring() {
        let mut buckets = [0u32; crate::stats::PROBE_GAP_BUCKETS];
        buckets[0] = 980;
        buckets[3] = 20;
        let p50 = crate::stats::probe_gap_percentile(&buckets, 0.5);
        let p99 = crate::stats::probe_gap_percentile(&buckets, 0.99);
        assert_eq!((p50, p99), (100, 400));
        assert_eq!(p99.saturating_sub(p50), 300);
        assert_eq!(
            crate::stats::probe_gap_percentile(&[0; crate::stats::PROBE_GAP_BUCKETS], 0.5),
            0
        );
        assert_eq!(crate::stats::probe_gap_bucket(50), 0);
        assert_eq!(crate::stats::probe_gap_bucket(350), 3);
        assert_eq!(
            crate::stats::probe_gap_bucket(1_000_000),
            crate::stats::PROBE_GAP_BUCKETS - 1
        );
    }

    fn outcome(loss_pct: f32, jitter_us: u32) -> ProbeOutcome {
        ProbeOutcome {
            done: true,
            wire_packets_sent: 10_000,
            gap_p50_us: 100,
            gap_p99_us: 100 + jitter_us,
            loss_pct,
            ..Default::default()
        }
    }

    fn speed(clean_loss: f32, jitter_us: u32) -> SpeedReport {
        let o = outcome(clean_loss, jitter_us);
        SpeedReport {
            ceiling_kbps: 940_000,
            wall: true,
            clean: Some(CleanRound {
                rate_kbps: 470_000,
                loss_pct: clean_loss,
                jitter_us: jitter_of(&o),
                reorders: 0,
                outcome: o,
            }),
            blast: None,
        }
    }

    fn legs(bursts_loss: f32, capped_loss: f32, drops: Option<u64>) -> Vec<Leg> {
        vec![
            Leg {
                shape: LegShape::FrameBursts,
                outcome: outcome(bursts_loss, 200),
                socket_drops: drops,
            },
            Leg {
                shape: LegShape::Capped,
                outcome: outcome(capped_loss, 200),
                socket_drops: drops.map(|_| 0),
            },
        ]
    }

    fn wired(mbps: u32, rcvbuf_kb: u32) -> ClientFacts {
        ClientFacts {
            link: LinkFacts {
                kind: crate::transport::IFACE_KIND_ETHERNET,
                mbps,
            },
            rcvbuf_kb,
        }
    }

    fn host(link_mbps: u32, sndbuf_kb: u32) -> HostFacts {
        HostFacts {
            iface_kind: crate::transport::IFACE_KIND_ETHERNET,
            link_mbps,
            sndbuf_kb,
            forced_profile: crate::quic::FORCED_PROFILE_NONE,
        }
    }

    fn ids(f: &[Finding]) -> Vec<FindingId> {
        f.iter().map(|f| f.id).collect()
    }

    #[test]
    fn clean_is_clean() {
        let f = judge(
            &speed(0.0, 300),
            &wired(1000, 32_768),
            Some(&host(1000, 32_768)),
            &legs(0.0, 0.0, Some(0)),
            None,
        );
        assert!(f.is_empty(), "{f:?}");
    }

    /// The capped leg heals what the bursts lose: the ports differ, whatever the facts say.
    #[test]
    fn f1_from_legs() {
        let f = judge(
            &speed(0.0, 300),
            &wired(0, 32_768),
            None,
            &legs(2.0, 0.1, Some(0)),
            None,
        );
        assert_eq!(ids(&f), vec![FindingId::SpeedMismatch]);
        assert_eq!(f[0].profile, Some(1));
        assert_eq!(f[0].numbers[2], 200);
    }

    /// The facts say the host's port is faster, and the capped leg helps at all.
    #[test]
    fn f1_from_facts() {
        let f = judge(
            &speed(0.0, 300),
            &wired(1000, 32_768),
            Some(&host(2500, 32_768)),
            &legs(1.0, 0.6, Some(0)),
            None,
        );
        assert_eq!(ids(&f), vec![FindingId::SpeedMismatch]);
        assert_eq!(&f[0].numbers[..2], &[2500, 1000]);
        // Matched ports: a capped leg that barely helps names nothing.
        let f = judge(
            &speed(0.0, 300),
            &wired(1000, 32_768),
            Some(&host(1000, 32_768)),
            &legs(1.0, 0.6, Some(0)),
            None,
        );
        assert!(!ids(&f).contains(&FindingId::SpeedMismatch));
    }

    /// Bursts lose capped or not, smooth does not, the socket dropped nothing: the receiver.
    #[test]
    fn f2_burst_intolerant() {
        let f = judge(
            &speed(0.0, 300),
            &wired(1000, 32_768),
            Some(&host(1000, 32_768)),
            &legs(1.0, 0.9, Some(0)),
            None,
        );
        assert_eq!(ids(&f), vec![FindingId::BurstIntolerant]);
        assert_eq!((f[0].severity, f[0].profile), (Severity::Bad, Some(2)));
    }

    /// Drops this socket counted explain the loss: the buffer, not the adapter.
    #[test]
    fn f3_socket_drops() {
        let f = judge(
            &speed(0.0, 300),
            &wired(1000, 32_768),
            Some(&host(1000, 32_768)),
            &legs(1.0, 0.9, Some(40)),
            None,
        );
        assert_eq!(ids(&f), vec![FindingId::ReceiveBuffer]);
        assert_eq!((f[0].severity, f[0].numbers[0]), (Severity::Bad, 40));
        // A small grant alone is a warning with the same next move.
        let f = judge(
            &speed(0.0, 300),
            &wired(1000, 208),
            None,
            &legs(0.0, 0.0, Some(0)),
            None,
        );
        assert_eq!(ids(&f), vec![FindingId::ReceiveBuffer]);
        assert_eq!(f[0].severity, Severity::Warn);
    }

    /// Where drops cannot be sampled, the receiver finding is only half sure.
    #[test]
    fn a_finding_needs_its_evidence_sampled() {
        let f = judge(
            &speed(0.0, 300),
            &wired(1000, 32_768),
            None,
            &legs(1.0, 0.9, None),
            None,
        );
        assert_eq!(ids(&f), vec![FindingId::BurstIntolerant]);
        assert_eq!(f[0].severity, Severity::Warn);
    }

    /// Loss at 5 Mbit/s on a wire is the wire's; on Wi-Fi it is the air's and says less.
    #[test]
    fn f4_link_fault() {
        let slow = outcome(0.4, 300);
        let f = judge(
            &speed(0.4, 300),
            &wired(1000, 32_768),
            None,
            &[],
            Some(&slow),
        );
        assert_eq!(ids(&f), vec![FindingId::LinkFault]);
        let wifi = ClientFacts {
            link: LinkFacts {
                kind: IFACE_KIND_WIFI,
                mbps: 0,
            },
            rcvbuf_kb: 32_768,
        };
        let f = judge(&speed(0.4, 300), &wifi, None, &[], Some(&slow));
        assert_eq!(ids(&f), vec![FindingId::Wifi]);
    }

    #[test]
    fn f5_queue_build_up_and_f6_host_send_buffer() {
        let f = judge(
            &speed(0.0, 25_000),
            &wired(1000, 32_768),
            Some(&host(1000, 208)),
            &[],
            None,
        );
        assert_eq!(
            ids(&f),
            vec![FindingId::QueueBuildUp, FindingId::HostSendBuffer]
        );
        assert_eq!(f[1].numbers, [0, 208, 0]);
    }

    /// Wi-Fi is a note until bursts lose, then the smooth profile is offered.
    #[test]
    fn f7_wifi() {
        let wifi = ClientFacts {
            link: LinkFacts {
                kind: IFACE_KIND_WIFI,
                mbps: 866,
            },
            rcvbuf_kb: 32_768,
        };
        let f = judge(&speed(0.0, 300), &wifi, None, &legs(0.0, 0.0, None), None);
        assert_eq!(ids(&f), vec![FindingId::Wifi]);
        assert_eq!((f[0].severity, f[0].profile), (Severity::Note, None));
        let f = judge(&speed(0.0, 300), &wifi, None, &legs(3.0, 2.9, None), None);
        let w = f.iter().find(|x| x.id == FindingId::Wifi).unwrap();
        assert_eq!((w.severity, w.profile), (Severity::Warn, Some(2)));
    }
}
