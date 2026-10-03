//! Wall-clock skew: the connect-time handshake ([`clock_sync`]), the NTP-style offset
//! estimator ([`clock_offset_ns`]), and the mid-stream re-sync state machine
//! ([`ClockResync`]).

use super::{ClockEcho, ClockProbe};

/// NTP offset (host minus client, ns) and RTT from `(t1, t2, t3, t4)` samples.
/// Picks the **minimum-RTT** sample: least queuing, and it drops the first
/// round's host-setup latency. Add the offset to a client timestamp for host time.
pub fn clock_offset_ns(samples: &[(u64, u64, u64, u64)]) -> Option<(i64, u64)> {
    samples
        .iter()
        .map(|&(t1, t2, t3, t4)| {
            let rtt = ((t4 as i128 - t1 as i128) - (t3 as i128 - t2 as i128)).max(0) as u64;
            let offset = (((t2 as i128 - t1 as i128) + (t3 as i128 - t4 as i128)) / 2) as i64;
            (offset, rtt)
        })
        .min_by_key(|&(_, rtt)| rtt)
}

/// Connect-time [`clock_sync`] result.
pub struct ClockSkew {
    /// Host minus client, ns. Add to a client timestamp to express it in host time.
    pub offset_ns: i64,
    /// RTT of the min-RTT sample, ns — not the last round.
    pub rtt_ns: u64,
    /// Probe rounds the host answered (may be fewer than eight if it went silent).
    pub rounds: usize,
}

/// Client-side skew handshake: `ROUNDS` [`ClockProbe`]/[`ClockEcho`] round-trips.
/// `None` if the host never answers (pre-skew host) — caller assumes a shared
/// clock. Each round is bounded so a silent host cannot wedge session start.
/// Any other message the host sends meanwhile goes back on `recv` for the control task.
///
/// Needs the feature for its runtime; the [`ClockProbe`]/[`ClockEcho`] codecs above do not, and a
/// browser drives the same rounds over its own stream.
#[cfg(feature = "quic")]
pub async fn clock_sync<W, R>(send: &mut W, recv: &mut super::io::MsgReader<R>) -> Option<ClockSkew>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    use super::io;
    use std::time::Duration;
    const ROUNDS: usize = 8;
    let read_timeout = Duration::from_secs(2);
    let mut samples: Vec<(u64, u64, u64, u64)> = Vec::with_capacity(ROUNDS);
    let mut aside = Vec::new();
    'rounds: for _ in 0..ROUNDS {
        let t1 = wall_clock_ns();
        let probe = ClockProbe { t1_ns: t1 }.encode();
        if io::write_msg(send, &probe).await.is_err() {
            break;
        }
        let deadline = tokio::time::Instant::now() + read_timeout;
        let echo = loop {
            match tokio::time::timeout_at(deadline, recv.read_msg()).await {
                Ok(Ok(b)) => match ClockEcho::decode(&b) {
                    Ok(e) => break e,
                    Err(_) => aside.push(b),
                },
                _ => break 'rounds, // timeout / stream error: pre-skew host
            }
        };
        samples.push((echo.t1_ns, echo.t2_ns, echo.t3_ns, wall_clock_ns()));
    }
    for msg in aside {
        recv.hold(msg);
    }
    clock_offset_ns(&samples).map(|(offset_ns, rtt_ns)| ClockSkew {
        offset_ns,
        rtt_ns,
        rounds: samples.len(),
    })
}

/// Unix-epoch ns (`CLOCK_REALTIME`). Not monotonic: steps and slew are what
/// the handshake measures, and they are the same basis the host stamps `pts_ns` with.
pub fn wall_clock_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Next action for the driver after [`ClockResync::on_echo`].
#[derive(Debug, PartialEq, Eq)]
pub enum ResyncStep {
    /// Stale echo (previous batch) or no batch in flight.
    Idle,
    /// Recorded; wait the inter-round gap, then stamp + send [`ClockResync::next_probe`].
    /// Space the rounds so the batch samples several video-burst phases — eight
    /// back-to-back rounds fit in one ~6 ms burst and all read the same instant.
    MoreRounds,
    /// Batch complete: min-RTT estimate, per [`clock_offset_ns`].
    Done { offset_ns: i64, rtt_ns: u64 },
}

/// Mid-stream 8-round [`clock_sync`] as a state machine the control `select!`
/// can drive without blocking. Echoes interleave with other traffic; rounds
/// match on echoed `t1`. Post-connect drift otherwise corrupts jump-to-live
/// and ABR delay; the disarm heuristic is the last backstop.
pub struct ClockResync {
    /// `t1` of the in-flight probe; `None` = idle. A mismatched echo is stale.
    pending_t1: Option<u64>,
    samples: Vec<(u64, u64, u64, u64)>,
}

impl ClockResync {
    /// Same round count as connect-time [`clock_sync`].
    pub const ROUNDS: usize = 8;

    pub fn new() -> ClockResync {
        ClockResync {
            pending_t1: None,
            samples: Vec::with_capacity(Self::ROUNDS),
        }
    }

    /// Start a batch, abandoning any in-flight one — its late echoes will not
    /// match `pending_t1`. Probe is stamped `now_ns`.
    pub fn begin(&mut self, now_ns: u64) -> ClockProbe {
        self.samples.clear();
        self.next_probe(now_ns)
    }

    /// Stamp the next probe at send time (`now_ns`) so the inter-round gap is
    /// not counted in RTT. Call after [`ResyncStep::MoreRounds`].
    pub fn next_probe(&mut self, now_ns: u64) -> ClockProbe {
        self.pending_t1 = Some(now_ns);
        ClockProbe { t1_ns: now_ns }
    }

    /// Inbound echo; `now_ns` is this round's `t4`.
    pub fn on_echo(&mut self, echo: &ClockEcho, now_ns: u64) -> ResyncStep {
        if self.pending_t1 != Some(echo.t1_ns) {
            return ResyncStep::Idle; // stale / unsolicited
        }
        self.samples
            .push((echo.t1_ns, echo.t2_ns, echo.t3_ns, now_ns));
        // Clear until the driver arms the next round — a duplicate echo must
        // not double-record.
        self.pending_t1 = None;
        if self.samples.len() < Self::ROUNDS {
            return ResyncStep::MoreRounds;
        }
        match clock_offset_ns(&self.samples) {
            Some((offset_ns, rtt_ns)) => ResyncStep::Done { offset_ns, rtt_ns },
            None => ResyncStep::Idle, // unreachable: ROUNDS > 0 samples just pushed
        }
    }
}

impl Default for ClockResync {
    fn default() -> Self {
        Self::new()
    }
}

/// Accept a batch iff min RTT `≤ max(2 ms, 1.5 × floor)`. Congestion biases
/// offset by queueing delay. The 2 ms floor keeps a near-zero connect RTT
/// (same-host / LAN) from rejecting every later batch over jitter.
pub fn accept_resync(batch_rtt_ns: u64, floor_rtt_ns: u64) -> bool {
    batch_rtt_ns <= (floor_rtt_ns + floor_rtt_ns / 2).max(2_000_000)
}

/// [`ResyncGuard::admit`] outcome.
#[derive(Debug, PartialEq, Eq)]
pub enum ResyncAdmit {
    /// Within the floor guard band: apply this batch's offset.
    Fresh,
    /// Congested: keep the previous offset. `streak` is consecutive rejections
    /// since the last applied batch.
    Rejected { streak: u32 },
    /// Streak hit [`ResyncGuard::MAX_REJECTED_STREAK`]: apply the streak's
    /// min-RTT batch rather than drift further.
    BestOfStreak { offset_ns: i64, rtt_ns: u64 },
}

/// Mid-stream batch admission.
///
/// Floor is the session min RTT, including rejected batches (their min-RTT
/// round is still path evidence). Connect is idle; comparing loaded batches
/// to it rejects them all. After [`Self::MAX_REJECTED_STREAK`] rejections,
/// apply the streak's min-RTT batch: bounded queueing bias beats unbounded drift.
pub struct ResyncGuard {
    /// Session min RTT, including rejected batches.
    floor_rtt_ns: u64,
    rejected_streak: u32,
    /// Min-RTT estimate in the current rejection streak.
    best_pending: Option<(i64, u64)>,
}

impl ResyncGuard {
    /// Rejections tolerated before the streak's best batch is applied anyway.
    pub const MAX_REJECTED_STREAK: u32 = 3;

    pub fn new(connect_rtt_ns: u64) -> ResyncGuard {
        ResyncGuard {
            floor_rtt_ns: connect_rtt_ns,
            rejected_streak: 0,
            best_pending: None,
        }
    }

    pub fn floor_rtt_ns(&self) -> u64 {
        self.floor_rtt_ns
    }

    /// Judge a completed batch. Caller applies the offset on `Fresh` / `BestOfStreak`
    /// and keeps the old one on `Rejected`.
    pub fn admit(&mut self, offset_ns: i64, rtt_ns: u64) -> ResyncAdmit {
        // Compare to the pre-batch floor, then fold this RTT in. Comparing
        // against a floor that already includes the batch would accept everything.
        let fresh = accept_resync(rtt_ns, self.floor_rtt_ns);
        self.floor_rtt_ns = self.floor_rtt_ns.min(rtt_ns);
        if fresh {
            self.rejected_streak = 0;
            self.best_pending = None;
            return ResyncAdmit::Fresh;
        }
        let best = match self.best_pending {
            Some((o, r)) if r <= rtt_ns => (o, r),
            _ => (offset_ns, rtt_ns),
        };
        self.best_pending = Some(best);
        self.rejected_streak += 1;
        if self.rejected_streak >= Self::MAX_REJECTED_STREAK {
            self.rejected_streak = 0;
            self.best_pending = None;
            return ResyncAdmit::BestOfStreak {
                offset_ns: best.0,
                rtt_ns: best.1,
            };
        }
        ResyncAdmit::Rejected {
            streak: self.rejected_streak,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::quic::*;

    #[test]
    fn clock_offset_picks_min_rtt_and_recovers_offset() {
        // Host +1 ms. Symmetric 200 µs each way recovers OFF; a fat return must lose.
        const OFF: i64 = 1_000_000;
        // t2 = t1 + 200 µs + OFF; t3 = t2 + 50 µs processing; t4 = t3 − OFF + 200 µs.
        let t1 = 0u64;
        let t2 = (t1 as i64 + 200_000 + OFF) as u64;
        let t3 = t2 + 50_000;
        let t4 = (t3 as i64 - OFF + 200_000) as u64;
        let n1 = 1_000_000u64;
        let n2 = (n1 as i64 + 200_000 + OFF) as u64;
        let n3 = n2 + 50_000;
        let n4 = (n3 as i64 - OFF + 5_000_000) as u64; // 5 ms return → big RTT
        let (offset, rtt) =
            clock_offset_ns(&[(n1, n2, n3, n4), (t1, t2, t3, t4)]).expect("non-empty");
        assert_eq!(offset, OFF);
        assert_eq!(rtt, 400_000);
        assert!(clock_offset_ns(&[]).is_none());
    }

    #[test]
    fn clock_resync_collects_rounds_and_ignores_stale_echoes() {
        // Host +1 ms; 100 µs one-way except one congested round.
        const OFF: i64 = 1_000_000;
        let echo_for = |t1: u64, one_way: u64| ClockEcho {
            t1_ns: t1,
            t2_ns: (t1 as i64 + one_way as i64 + OFF) as u64,
            t3_ns: (t1 as i64 + one_way as i64 + OFF) as u64 + 10_000,
        };
        let t4_for = |e: &ClockEcho, one_way: u64| (e.t3_ns as i64 - OFF + one_way as i64) as u64;

        let mut rs = ClockResync::new();
        assert_eq!(
            rs.on_echo(&echo_for(42, 100_000), 500_000),
            ResyncStep::Idle
        );

        let mut probe = rs.begin(1_000_000);
        // t1=42 is the abandoned pre-begin probe.
        assert_eq!(
            rs.on_echo(&echo_for(42, 100_000), 500_000),
            ResyncStep::Idle
        );
        for round in 0..ClockResync::ROUNDS {
            // Round 3: 5 ms one-way, must lose min-RTT.
            let one_way = if round == 3 { 5_000_000 } else { 100_000 };
            let echo = echo_for(probe.t1_ns, one_way);
            let t4 = t4_for(&echo, one_way);
            match rs.on_echo(&echo, t4) {
                ResyncStep::MoreRounds => {
                    assert!(round < ClockResync::ROUNDS - 1, "batch overran its rounds");
                    // No probe in flight until next_probe: a duplicate must Idle.
                    assert_eq!(rs.on_echo(&echo, t4), ResyncStep::Idle);
                    // Stamp at send time so the 7 ms gap is not in the measured RTT.
                    probe = rs.next_probe(t4 + 7_000_000);
                }
                ResyncStep::Done { offset_ns, rtt_ns } => {
                    assert_eq!(round, ClockResync::ROUNDS - 1, "batch ended early");
                    assert_eq!(offset_ns, OFF, "min-RTT round recovers the offset exactly");
                    assert_eq!(rtt_ns, 200_000); // 2×100 µs; host processing (t3−t2) excluded
                }
                ResyncStep::Idle => panic!("matched echo must advance the batch"),
            }
        }
        // Matching-t1 replay after Done must not advance.
        assert_eq!(
            rs.on_echo(&echo_for(probe.t1_ns, 100_000), probe.t1_ns + 300_000),
            ResyncStep::Idle
        );

        // begin() mid-batch: the old probe's echo is stale.
        let old = rs.begin(2_000_000);
        let fresh = rs.begin(3_000_000);
        assert_eq!(
            rs.on_echo(&echo_for(old.t1_ns, 100_000), 2_300_000),
            ResyncStep::Idle
        );
        assert_eq!(
            rs.on_echo(&echo_for(fresh.t1_ns, 100_000), 3_300_000),
            ResyncStep::MoreRounds
        );
    }

    #[test]
    fn resync_guard_floor_tracking_and_bounded_streak() {
        // Connect 400 µs idle; the 2 ms accept_resync floor governs.
        let mut g = ResyncGuard::new(400_000);
        assert_eq!(g.admit(10, 1_500_000), ResyncAdmit::Fresh);
        assert_eq!(g.admit(11, 300_000), ResyncAdmit::Fresh);
        assert_eq!(g.floor_rtt_ns(), 300_000);

        // 4–6 ms all exceed max(2 ms, 1.5 × 300 µs).
        assert_eq!(g.admit(100, 6_000_000), ResyncAdmit::Rejected { streak: 1 });
        assert_eq!(g.admit(200, 4_000_000), ResyncAdmit::Rejected { streak: 2 });
        // Cap applies the streak min-RTT (4 ms / offset 200), not the last batch.
        assert_eq!(
            g.admit(300, 5_000_000),
            ResyncAdmit::BestOfStreak {
                offset_ns: 200,
                rtt_ns: 4_000_000
            }
        );
        assert_eq!(g.admit(400, 5_000_000), ResyncAdmit::Rejected { streak: 1 });
        assert_eq!(g.admit(500, 350_000), ResyncAdmit::Fresh);

        // A batch that *is* the new floor is still Fresh (compared to the pre-batch floor).
        let mut g2 = ResyncGuard::new(10_000_000);
        assert_eq!(g2.admit(1, 8_000_000), ResyncAdmit::Fresh);
        assert_eq!(g2.floor_rtt_ns(), 8_000_000);
    }

    #[test]
    fn clock_resync_acceptance_guard() {
        // 10 ms connect: accept up to 1.5×.
        assert!(accept_resync(14_000_000, 10_000_000));
        assert!(!accept_resync(16_000_000, 10_000_000));
        // 200 µs connect: the 2 ms floor governs.
        assert!(accept_resync(1_900_000, 200_000));
        assert!(!accept_resync(2_100_000, 200_000));
        // Inclusive bound.
        assert!(accept_resync(2_000_000, 0));
        assert!(accept_resync(15_000_000, 10_000_000));
    }

    /// A message the host sends during the rounds neither ends them nor is lost: the
    /// control task reads it next.
    #[cfg(feature = "quic")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clock_sync_keeps_a_message_that_arrives_mid_rounds() {
        let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let host = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            let (mut send, recv) = conn.accept_bi().await.unwrap();
            let mut recv = io::MsgReader::new(recv);
            let early = AudioState { muted: true };
            for round in 0..8 {
                let probe = ClockProbe::decode(&recv.read_msg().await.unwrap()).unwrap();
                if round == 2 {
                    io::write_msg(&mut send, &early.encode()).await.unwrap();
                }
                let t = wall_clock_ns();
                let echo = ClockEcho {
                    t1_ns: probe.t1_ns,
                    t2_ns: t,
                    t3_ns: t,
                };
                io::write_msg(&mut send, &echo.encode()).await.unwrap();
            }
            (server, conn, send)
        });
        let client = endpoint::client_insecure().unwrap();
        let conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
        let (mut send, recv) = conn.open_bi().await.unwrap();
        let mut recv = io::MsgReader::new(recv);
        let skew = clock_sync(&mut send, &mut recv)
            .await
            .expect("host answered");
        assert_eq!(skew.rounds, 8);
        let _host = host.await.unwrap();
        let next = recv.read_msg().await.unwrap();
        assert_eq!(
            AudioState::decode(&next).unwrap(),
            AudioState { muted: true }
        );
    }
}
