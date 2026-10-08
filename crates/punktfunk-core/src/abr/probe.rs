//! Measuring the link: a bring-up ramp before the first frame, and the
//! legacy in-session burst.
//!
//! The ramp is a sequence of short `ProbeRequest`s, doubling in rate, issued
//! while the host's pipeline is still building — no video exists yet, so
//! nothing it does can cost a frame (law L4). It stops itself at the first
//! step the link does not deliver, at what the stream can use, at a sender
//! that cannot offer the rate, or at the first video frame.
//!
//! The legacy burst runs against a host without [`HOST_CAP2_RAMP`]: one
//! 800 ms burst beside live video, two seconds in. It damages the window it
//! lands in, so that window is discarded; if it took the keyframe with it the
//! session asks for a new one. Every deadline here exists because a host may
//! simply not answer: an unanswered burst that latched `active` would
//! suppress the report tick for the rest of the session.
//!
//! [`HOST_CAP2_RAMP`]: crate::quic::HOST_CAP2_RAMP

use std::time::{Duration, Instant};

/// Burst length. Long enough to fill the bottleneck queue and drain it,
/// short enough that the picture survives it on most links.
const PROBE_MS: u32 = 800;
/// Wait after video flows before bursting. The first frames are the encoder's
/// IDR and the decoder's bring-up; a burst on top of them measures neither.
const PROBE_DELAY: Duration = Duration::from_secs(2);
/// Queue and QUIC loss recovery sit between the host's "complete" and our
/// receipt. A result later than this is not about the burst.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// One ramp step. A 5 Mbps step is a dozen packets at this length, and the
/// whole ramp fits inside one bring-up.
const RAMP_STEP_MS: u32 = 25;
/// The ramp's first rate. Under the controller's floor there is nothing worth
/// measuring.
const RAMP_START_KBPS: u32 = 5_000;
/// A step is capped in bytes as well as in time: 25 ms at 5.1 Gbps is 16 MB,
/// and a step the application cannot drain measures the receive buffer.
/// Multi-gigabit steps keep ~20 ms windows instead of shrinking to a few
/// milliseconds, where fixed scheduling jitter reads as a wall.
const RAMP_STEP_BYTES: u64 = 16_000_000;
/// Delivered ÷ offered under this is a wall.
const RAMP_WALL_PCT: u64 = 90;
/// Loss a refused step may carry and still be asked a second time: one
/// packet — what independent loss puts in the first step's dozen — or this
/// share of a larger step. A policer's step arrives a tenth short or worse.
const RAMP_LOSS_SLACK_PCT: u64 = 5;
/// What a wall licenses. A wall measured once is a snapshot of a link that
/// moves — Wi-Fi by ±30 % — and the 30 % held back is what a 100 ms airtime
/// stall spends instead of frames. It is also what the in-session burst has
/// always kept, so the top of the range is no worse than what people like.
const RAMP_CEILING_PCT: u32 = 70;
/// The session opens at this share of what the ramp proved.
const RAMP_START_PCT: u32 = 50;
/// What a clean picture wants, as a divisor of the stream-shape cap: 0.15 bpp
/// against the cap's 0.75.
const MODE_RATE_DIV: u32 = 5;
/// Arrivals quiet this long mean the step has drained out of the receive
/// buffer and the next step measures its own bytes, not the last one's.
const RAMP_DRAIN_MS: u64 = 20;
/// A step whose result never comes. One RTT plus the step; a host that is
/// slower than this is not going to finish the ramp either.
const RAMP_STEP_TIMEOUT: Duration = Duration::from_millis(1_500);

/// What the burst or step delivered, as the pump's probe state holds it.
///
/// Two domains, deliberately: `delivered_*` and `wire_packets_sent` are wire
/// packets (header plus shard, parity included), `host_bytes_sent` is the
/// payload the host offered. Only compare like with like.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProbeReport {
    /// Wire bytes the burst delivered. `0` = declined.
    pub delivered_bytes: u64,
    /// Wire packets behind those bytes.
    pub delivered_packets: u64,
    /// Throughput denominator: the client receive interval when the burst
    /// produced one, else the host's send window.
    pub window_ms: u32,
    /// Host send-window duration. `0` = the host declined the burst.
    pub host_duration_ms: u32,
    /// The measured client interval, for the log. `0` = none.
    pub client_interval_ms: u32,
    /// The same interval in microseconds: the ramp's own denominator, at the
    /// resolution the arrival stamps have. `0` = none.
    pub client_interval_us: u32,
    /// Payload bytes the host put on the wire, against what was asked.
    pub host_bytes_sent: u64,
    /// Wire packets the host's kernel accepted.
    pub wire_packets_sent: u32,
    /// Wire packets the send buffer refused: the sender was the limit.
    pub send_dropped: u32,
}

/// What a burst's report was worth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Measured {
    /// Not the burst this client asked for — an embedder speed test, or a
    /// report already consumed. Nothing is learned, nothing is rebased.
    NotOurs,
    /// The host declined the burst: the negotiated ceiling stands.
    Declined,
    /// Link capacity, headroom already taken off. `wall_kbps` is `Some` when
    /// the link refused what the burst offered — the same test a ramp step is
    /// judged by, so it is the same kind of evidence about the same wall.
    Ceiling {
        ceiling_kbps: u32,
        wall_kbps: Option<u32>,
    },
}

/// What the ramp proved by the time it stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ramped {
    /// The link refused a step: it holds about `delivered_kbps` and no more.
    Wall { delivered_kbps: u32 },
    /// Nothing refused up to `proven_kbps`, which is a floor under the
    /// capacity and says nothing about a limit. `0` = nothing measured.
    NoWall { proven_kbps: u32 },
}

impl Ramped {
    /// The rate the ramp proved the link carries, whichever way it ended.
    pub(crate) fn proven_kbps(self) -> u32 {
        match self {
            Ramped::Wall { delivered_kbps } => delivered_kbps,
            Ramped::NoWall { proven_kbps } => proven_kbps,
        }
    }
}

/// One settled ramp step, as a client recording the measurement sees it.
///
/// The numbers the step was judged on and what it came to — nothing derived,
/// so a rig reading this and the controller cannot disagree about what the
/// ramp measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RampStep {
    /// Milliseconds from the ramp's start to this step settling.
    pub t_ms: u64,
    /// This step re-asked a rate a previous one was refused at.
    pub repeat: bool,
    pub target_kbps: u32,
    pub asked_bytes: u64,
    pub host_bytes_sent: u64,
    pub wire_packets_sent: u32,
    pub delivered_packets: u64,
    pub delivered_bytes: u64,
    pub client_interval_ms: u32,
    pub host_duration_ms: u32,
    pub send_dropped: u32,
    /// What this step decided.
    pub end: RampStepEnd,
}

/// What a settled step came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RampStepEnd {
    /// Proved its rate; the ramp doubled and went on.
    Continued,
    /// Delivered under the bar: the link's wall.
    Wall { delivered_kbps: u32 },
    /// The sender could not offer the rate: a floor under the link, never a
    /// wall, because the link was never asked.
    SenderLimit { proven_kbps: u32 },
    /// Too little arrived to read a rate over — the ramp keeps what it had.
    Unreadable,
    /// Refused with nothing lost: asked again at the same rate before the
    /// refusal is allowed to be a wall.
    RefusedReAsking { delivered_kbps: u32 },
    /// Proved the most this stream can use; capacity above it is not the
    /// session's business.
    ReachedMax { proven_kbps: u32 },
    /// The report never arrived, so nothing judged it.
    NoReport,
    /// The host answered without sending: nothing will drain.
    Declined,
}

/// What the ramp came to, for a test that pins its arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RampSummary {
    pub wall: bool,
    pub proven_kbps: u32,
    pub steps: u32,
    /// Payload bytes asked for over every step.
    pub asked_bytes: u64,
    /// Wall clock from the first step to the last settling.
    pub took_ms: u64,
}

/// What one settled step says.
enum Verdict {
    /// The link did not carry it. `timing_only` = what went missing is one
    /// packet or inside [`RAMP_LOSS_SLACK_PCT`], so the reading rests on when
    /// the packets arrived rather than on how many.
    Refused {
        delivered_kbps: u32,
        timing_only: bool,
    },
    /// The host could not offer the rate; what it managed is a floor.
    Sender(u32),
    /// Too few packets, or no interval: nothing can be read from it.
    Unreadable,
}

/// One step in flight.
struct Step {
    target_kbps: u32,
    /// Re-asking a rate that was refused without loss.
    repeat: bool,
    /// How long the host was asked to send for, microseconds. The offered
    /// side of the ratio is measured against this rather than against the
    /// host's own `duration_ms`: that is a wire field it rounds down to whole
    /// milliseconds off its own clock, which is 4 % of a 25 ms step, and the
    /// sender-limit rule below has already established that the host put
    /// what was asked on the wire (`send_dropped == 0`, offered ≈ asked).
    asked_us: u64,
    /// Payload bytes the host was asked for: `target_kbps` over the step.
    asked_bytes: u64,
    /// Delivered bytes the last report showed, and when they last grew.
    /// Arrivals going quiet, after some arrived, is the client's own drain.
    seen_bytes: u64,
    seen_at: Instant,
    last: Option<ProbeReport>,
    deadline: Instant,
}

/// The bring-up ramp: short steps at doubling rates until one of the stop
/// rules fires.
struct Ramp {
    /// The ramp never asks above this: what the stream can use, or
    /// `PUNKTFUNK_ABR_PROBE_KBPS`.
    max_kbps: u32,
    /// Rate the next step asks for.
    next_kbps: u32,
    step: Option<Step>,
    /// Highest rate the link delivered whole.
    proven_kbps: u32,
    /// Payload bytes asked for over the whole ramp, and steps issued.
    spent_bytes: u64,
    steps: u32,
    started: Instant,
    /// The ramp has stopped, and whether it stopped at a wall. Kept apart
    /// from `outcome`, which the driver takes: a consumed verdict must not
    /// look like an unfinished ramp.
    done: bool,
    /// Wall clock from the first step to the stop, frozen there: a reader that
    /// asked later would otherwise be told how long it waited to ask.
    stopped_ms: u64,
    wall: bool,
    /// A refusal waiting on its repeat, and whether this ramp has spent the
    /// one repeat it gets.
    confirming: Option<u32>,
    re_ask_spent: bool,
    /// Stopped before it had asked everything it meant to: video arrived, or
    /// a step went unanswered. Capacity above `proven_kbps` is unmeasured.
    cut_short: bool,
    outcome: Option<Ramped>,
    /// Every settled step, for a client writing the measurement down. Bounded
    /// by the ramp itself: it doubles from 5 Mbps and stops at the stream's
    /// need, so this is a handful of entries.
    history: Vec<RampStep>,
}

/// What one step proves, kbps of wire: its delivered bytes over the interval
/// they arrived in, and never more than the step offered.
///
/// The clamp is the whole of what a short step can honestly say. A 5 Mbps
/// step is a dozen packets; whether the last one lands 6 ms or 15 ms after
/// the first swings the implied rate 2.5× (rig, every profile), and an
/// unclamped reading opened sessions on a rate no step ever offered.
///
/// What a step offered is its wire when that is more than it asked: filler
/// frames are small, so parity and headers put a 10 Mbps step on the wire
/// at ~17 Mbps, and a wall read against the asked rate sat a fifth under
/// the link.
fn step_rate_kbps(step: &Step, r: &ProbeReport) -> u32 {
    let us = u64::from(r.client_interval_us.max(1));
    let rate = r.delivered_bytes.saturating_mul(8_000) / us;
    // The host reports packets, not their size; what arrived carries it.
    let per_packet = r.delivered_bytes / r.delivered_packets.max(1);
    let wire =
        (u64::from(r.wire_packets_sent) * per_packet).saturating_mul(8_000) / step.asked_us.max(1);
    let offered = wire.max(u64::from(step.target_kbps));
    rate.min(offered).min(u64::from(u32::MAX)) as u32
}

/// Did the link refuse what was offered? The legacy burst's version of the
/// test a ramp step is judged by: the host sent `wire_packets_sent` over its
/// window, we received `delivered_packets` over ours. A queue that stretches
/// the arrivals and loss that thins them both land here.
///
/// The burst carries no asked span of its own, so the host's send window
/// stands in for one — 800 ms of it, where a step's is 25 ms, so the
/// first-to-last correction a step needs is under a tenth of a percent here.
/// A sender that could not offer the rate is not a refusal: the link was
/// never asked.
fn link_refused(r: &ProbeReport) -> bool {
    let interval = u64::from(r.client_interval_ms);
    if interval == 0 || r.delivered_packets < 2 || r.wire_packets_sent == 0 || r.send_dropped > 0 {
        return false;
    }
    r.delivered_packets * u64::from(r.host_duration_ms) * 100
        < u64::from(r.wire_packets_sent) * interval * RAMP_WALL_PCT
}

/// Step length for a rate: the step's own duration, shortened when the byte
/// cap binds first.
fn step_ms(target_kbps: u32) -> u32 {
    let by_bytes = (RAMP_STEP_BYTES * 8 / u64::from(target_kbps.max(1))) as u32;
    by_bytes.clamp(1, RAMP_STEP_MS)
}

impl Ramp {
    fn new(max_kbps: u32, now: Instant) -> Self {
        Ramp {
            max_kbps: max_kbps.max(RAMP_START_KBPS),
            next_kbps: RAMP_START_KBPS,
            step: None,
            proven_kbps: 0,
            spent_bytes: 0,
            steps: 0,
            started: now,
            done: false,
            stopped_ms: 0,
            wall: false,
            confirming: None,
            re_ask_spent: false,
            cut_short: false,
            outcome: None,
            history: Vec::new(),
        }
    }

    /// Write a settled step down. Records what was judged, never a judgement.
    fn note(&mut self, step: &Step, r: Option<&ProbeReport>, end: RampStepEnd, now: Instant) {
        let d = ProbeReport::default();
        let r = r.unwrap_or(&d);
        self.history.push(RampStep {
            t_ms: now.duration_since(self.started).as_millis() as u64,
            repeat: step.repeat,
            target_kbps: step.target_kbps,
            asked_bytes: step.asked_bytes,
            host_bytes_sent: r.host_bytes_sent,
            wire_packets_sent: r.wire_packets_sent,
            delivered_packets: r.delivered_packets,
            delivered_bytes: r.delivered_bytes,
            client_interval_ms: r.client_interval_ms,
            host_duration_ms: r.host_duration_ms,
            send_dropped: r.send_dropped,
            end,
        });
    }

    /// Stop here. The first stop wins: a later one would overwrite evidence
    /// with the absence of it.
    fn stop(&mut self, outcome: Ramped) {
        self.step = None;
        if !self.done {
            self.done = true;
            self.stopped_ms = self.started.elapsed().as_millis() as u64;
            self.wall = matches!(outcome, Ramped::Wall { .. });
            self.proven_kbps = outcome.proven_kbps();
            tracing::info!(
                proven_kbps = outcome.proven_kbps(),
                wall = matches!(outcome, Ramped::Wall { .. }),
                steps = self.steps,
                asked_kb = self.spent_bytes / 1_000,
                took_ms = self.stopped_ms,
                "adaptive bitrate: bring-up ramp done"
            );
            self.outcome = Some(outcome);
        }
    }

    /// Stop with nothing above `proven_kbps` measured: video arrived, a step
    /// went unanswered, or one came back unreadable. The link's wall, if it
    /// has one, is still out there.
    fn no_wall(&mut self) {
        self.cut_short = true;
        self.stop(Ramped::NoWall {
            proven_kbps: self.proven_kbps,
        });
    }

    /// The ramp asked everything it meant to and nothing refused it.
    fn finished(&mut self) {
        self.stop(Ramped::NoWall {
            proven_kbps: self.proven_kbps,
        });
    }

    /// A step the link did not carry.
    ///
    /// A step the link thinned by more than one packet and more than
    /// [`RAMP_LOSS_SLACK_PCT`] is decisive: it dropped them, and no second
    /// reading makes that untrue.
    /// Inside the slack the verdict rests on WHEN the packets arrived, and a
    /// few milliseconds of scheduling on a 25 ms window is the difference
    /// between 0.86 and 0.91. So the same rate goes out once more and only a
    /// second refusal is a wall; a repeat that passes carries the ramp on.
    /// One repeat per ramp, so it costs one step at one rate.
    fn on_refusal(
        &mut self,
        delivered_kbps: u32,
        timing_only: bool,
        now: Instant,
    ) -> Option<(u32, u32)> {
        if let Some(first) = self.confirming.take() {
            // Refused twice at the same rate: the link carried at best the
            // better of the two readings.
            self.stop(Ramped::Wall {
                delivered_kbps: delivered_kbps.max(first),
            });
            return None;
        }
        if timing_only && !self.re_ask_spent {
            self.re_ask_spent = true;
            self.confirming = Some(delivered_kbps);
            tracing::info!(
                delivered_kbps,
                "adaptive bitrate: ramp asking the refused rate once more before calling it a wall"
            );
            // `next_kbps` still holds the rate that was just asked.
            return Some(self.begin(now));
        }
        self.stop(Ramped::Wall { delivered_kbps });
        None
    }

    /// Judge a settled step. `None` = it proved the rate and the ramp goes on.
    fn judge(&self, step: &Step, r: &ProbeReport) -> Option<Verdict> {
        let interval = u64::from(r.client_interval_us);
        // Under two packets there is no interval, so there is no rate either.
        if interval == 0 || r.delivered_packets < 2 || r.wire_packets_sent == 0 {
            return Some(Verdict::Unreadable);
        }
        let delivered_kbps = step_rate_kbps(step, r);
        // The SENDER could not offer the rate: what it managed is a floor
        // under the link, never a wall (the link was never asked).
        if r.send_dropped > 0 || r.host_bytes_sent * 100 < step.asked_bytes * 90 {
            tracing::info!(
                target_kbps = step.target_kbps,
                delivered_kbps,
                send_dropped = r.send_dropped,
                host_bytes_sent = r.host_bytes_sent,
                asked_bytes = step.asked_bytes,
                "adaptive bitrate: ramp step limited by the sender, not the link"
            );
            // A complete answer: this host cannot send faster, whatever the
            // link would take.
            return Some(Verdict::Sender(delivered_kbps));
        }
        // Delivered ÷ offered as packets a microsecond on each side. Both
        // spans are first-to-last: `n` packets paced over the asked window
        // leave it across `n - 1` gaps, and the arrivals are timed the same
        // way, so a clean step reads 1.00 instead of 1.04. A queue that
        // stretches the arrivals and loss that thins them both land here.
        let offered_span =
            step.asked_us * (u64::from(r.wire_packets_sent) - 1) / u64::from(r.wire_packets_sent);
        let delivered = r.delivered_packets * offered_span;
        let offered = u64::from(r.wire_packets_sent) * interval;
        if delivered * 100 < offered * RAMP_WALL_PCT {
            let sent = u64::from(r.wire_packets_sent);
            let timing_only = r.delivered_packets + 1 >= sent
                || r.delivered_packets * 100 >= sent * (100 - RAMP_LOSS_SLACK_PCT);
            tracing::info!(
                target_kbps = step.target_kbps,
                delivered_kbps,
                client_interval_us = r.client_interval_us,
                offered_span_us = offered_span,
                delivered_packets = r.delivered_packets,
                wire_packets_sent = r.wire_packets_sent,
                timing_only,
                "adaptive bitrate: ramp step refused"
            );
            return Some(Verdict::Refused {
                delivered_kbps,
                timing_only,
            });
        }
        None
    }

    /// Fold a report into the step in flight. Reports repeat: the pump
    /// re-presents the probe state every iteration, and the bytes keep
    /// growing while the receive buffer drains.
    ///
    /// A step the host declined put nothing on the wire, so there is no drain
    /// to wait for: it ends the ramp on the report, not on the step's deadline.
    fn on_report(&mut self, r: ProbeReport, now: Instant) {
        if r.host_duration_ms == 0 && r.host_bytes_sent == 0 {
            if let Some(step) = self.step.take() {
                self.note(&step, Some(&r), RampStepEnd::Declined, now);
                self.no_wall();
            }
            return;
        }
        let Some(step) = self.step.as_mut() else {
            return;
        };
        if r.delivered_bytes > step.seen_bytes {
            step.seen_bytes = r.delivered_bytes;
            step.seen_at = now;
        }
        step.last = Some(r);
    }

    /// A step is over once its bytes stopped arriving. Returns the next step
    /// to ask for.
    fn settle(&mut self, now: Instant) -> Option<(u32, u32)> {
        let step = self.step.as_ref()?;
        // Bytes have to have ARRIVED before their absence means drained: a
        // step queued behind 450 ms of someone else's traffic is late, not
        // empty, and judging it empty reads a busy link as no link at all.
        let drained = step.seen_bytes > 0
            && now.duration_since(step.seen_at).as_millis() as u64 >= RAMP_DRAIN_MS;
        if !drained {
            return None;
        }
        let step = self.step.take().expect("present on this branch");
        let Some(report) = step.last else {
            self.note(&step, None, RampStepEnd::NoReport, now);
            self.no_wall();
            return None;
        };
        match self.judge(&step, &report) {
            Some(Verdict::Refused {
                delivered_kbps,
                timing_only,
            }) => {
                // What the repeat was credited against, before the refusal
                // consumes it: the record reproduces the decision, never
                // makes it.
                let first = self.confirming;
                let again = self.on_refusal(delivered_kbps, timing_only, now);
                let end = if self.confirming.is_some() {
                    RampStepEnd::RefusedReAsking { delivered_kbps }
                } else {
                    RampStepEnd::Wall {
                        delivered_kbps: first.map_or(delivered_kbps, |f| delivered_kbps.max(f)),
                    }
                };
                self.note(&step, Some(&report), end, now);
                again
            }
            Some(Verdict::Sender(delivered_kbps)) => {
                let proven_kbps = self.proven_kbps.max(delivered_kbps);
                self.note(
                    &step,
                    Some(&report),
                    RampStepEnd::SenderLimit { proven_kbps },
                    now,
                );
                self.stop(Ramped::NoWall { proven_kbps });
                None
            }
            Some(Verdict::Unreadable) => {
                self.note(&step, Some(&report), RampStepEnd::Unreadable, now);
                self.no_wall();
                None
            }
            None => {
                // A rate that passes refutes any refusal of it.
                self.confirming = None;
                self.proven_kbps = step_rate_kbps(&step, &report);
                if step.target_kbps >= self.max_kbps {
                    // The ramp asked for everything this stream can use and
                    // got it. Capacity above that is not this session's
                    // business.
                    self.note(
                        &step,
                        Some(&report),
                        RampStepEnd::ReachedMax {
                            proven_kbps: self.proven_kbps,
                        },
                        now,
                    );
                    self.finished();
                    return None;
                }
                self.note(&step, Some(&report), RampStepEnd::Continued, now);
                self.next_kbps = step.target_kbps.saturating_mul(2).min(self.max_kbps);
                Some(self.begin(now))
            }
        }
    }

    /// Arm the next step and say what to ask the host for.
    fn begin(&mut self, now: Instant) -> (u32, u32) {
        let target_kbps = self.next_kbps.min(self.max_kbps);
        let duration_ms = step_ms(target_kbps);
        let asked_bytes = u64::from(target_kbps) * u64::from(duration_ms) / 8;
        let asked_us = u64::from(duration_ms) * 1_000;
        self.spent_bytes += asked_bytes;
        self.steps += 1;
        self.step = Some(Step {
            target_kbps,
            // `on_refusal` arms the repeat just before it asks again.
            repeat: self.confirming.is_some(),
            asked_us,
            asked_bytes,
            seen_bytes: 0,
            seen_at: now,
            last: None,
            deadline: now + RAMP_STEP_TIMEOUT,
        });
        (target_kbps, duration_ms)
    }
}

/// The measurement's whole life: the ramp before the first frame, or the
/// legacy burst armed, in flight, answered or abandoned.
pub(crate) struct CapacityProbe {
    /// Burst target. `PUNKTFUNK_ABR_PROBE_KBPS`, or twice the stream cap.
    target_kbps: u32,
    /// The bring-up ramp, against a host that serves it.
    ramp: Option<Ramp>,
    /// When to fire the legacy burst. `None` = fired already, or never armed.
    fire_at: Option<Instant>,
    /// Our own burst is out and its result is still owed.
    result_by: Option<Instant>,
    /// Any burst is in flight, ours or an embedder speed test.
    active: bool,
    /// An in-flight burst that outlives this is unanswered: let it go, or the
    /// report tick stays suppressed forever.
    watchdog: Option<Instant>,
    /// `frames_completed` when the burst started: "did any frame survive",
    /// not "has one ever arrived".
    frames_at_start: u64,
    /// Nothing is going to measure this link: no probe was armed, the host
    /// declined it, it timed out, or the ramp ended with nothing. Taken once.
    no_evidence: bool,
    /// A pinned session's ramp: it sizes the pin, and nothing follows — no
    /// burst beside video, and no controller a measured ceiling would serve.
    /// A cut-short ramp must not arm the burst it was never going to fire.
    pinned: bool,
}

impl CapacityProbe {
    /// `target_kbps` of `None` sizes the burst from the stream cap, and caps
    /// the ramp at what the stream can use — or, `probe_only`, at the link's
    /// wall. `armed` is `PUNKTFUNK_ABR_PROBE` plus the session being
    /// Automatic at all; `ramp` is the host's
    /// [`HOST_CAP2_RAMP`](crate::quic::HOST_CAP2_RAMP).
    pub(crate) fn new(
        armed: bool,
        ramp: bool,
        target_kbps: Option<u32>,
        stream_cap_kbps: u32,
        probe_only: bool,
        now: Instant,
    ) -> Self {
        let ramp_max = if probe_only {
            target_kbps.map_or(LINK_RAMP_MAX_KBPS, |k| k.min(LINK_RAMP_MAX_KBPS))
        } else {
            ramp_max_kbps(stream_cap_kbps, target_kbps)
        };
        CapacityProbe {
            target_kbps: target_kbps.unwrap_or_else(|| probe_target_kbps(stream_cap_kbps)),
            ramp: (armed && ramp).then(|| Ramp::new(ramp_max, now)),
            // The ramp replaces the burst; it never runs beside video.
            fire_at: (armed && !ramp).then(|| now + PROBE_DELAY),
            result_by: None,
            active: false,
            watchdog: None,
            frames_at_start: 0,
            no_evidence: !armed,
            pinned: false,
        }
    }

    /// A pinned session's ramp. It climbs to [`PINNED_RAMP_HEADROOM`] times
    /// the pin: a wall under the pin sizes the pin, and the rate it proves is
    /// what the host paces the stream at. `armed` is `PUNKTFUNK_ABR_PROBE`
    /// plus the host's `HOST_CAP2_RAMP`; an old host runs no measurement and
    /// keeps the pin it resolved.
    ///
    /// No burst follows: a cut-short ramp leaves the pin as it is rather
    /// than costing a started picture a measurement nobody would use.
    pub(crate) fn for_pinned(
        armed: bool,
        target_kbps: Option<u32>,
        pin_kbps: u32,
        now: Instant,
    ) -> Self {
        CapacityProbe {
            // Never fired: `fire_at` stays `None` for a pinned session.
            target_kbps: 0,
            ramp: armed.then(|| Ramp::new(pinned_ramp_max_kbps(pin_kbps, target_kbps), now)),
            fire_at: None,
            result_by: None,
            active: false,
            watchdog: None,
            frames_at_start: 0,
            no_evidence: !armed,
            pinned: true,
        }
    }

    pub(crate) fn active(&self) -> bool {
        self.active
    }

    /// A burst went in or out of flight. `Some(frames_at_start)` on the
    /// trailing edge: the caller compares it against the frames completed to
    /// see whether the burst took every picture with it.
    pub(crate) fn on_active(
        &mut self,
        active: bool,
        duration_ms: u32,
        frames_completed: u64,
        now: Instant,
    ) -> Option<u64> {
        let ended = self.active && !active;
        if !self.active && active {
            let burst = Duration::from_millis(u64::from(duration_ms));
            self.watchdog = Some(now + burst + PROBE_TIMEOUT);
            self.frames_at_start = frames_completed;
        }
        if !active {
            self.watchdog = None;
        }
        self.active = active;
        ended.then_some(self.frames_at_start)
    }

    /// Fire what is due: the next ramp step, or the legacy burst once video
    /// is actually flowing. A slow host bring-up is still emitting its first
    /// IDR, so the burst waits another delay.
    ///
    /// Two counts, deliberately. `frames_completed` is the session's, which
    /// includes probe filler AUs — the reassembler has no probe/video split at
    /// the completion (`session.rs`). `video_aus` is what the embedder
    /// forwarded as picture. The ramp reads the second, or its own filler
    /// would end it one tick after its first step, and so does the burst a
    /// cut-short ramp arms behind that filler. A burst with no ramp before it
    /// reads the first, which no filler precedes.
    pub(crate) fn poll(
        &mut self,
        now: Instant,
        frames_completed: u64,
        video_aus: u64,
    ) -> Option<(u32, u32)> {
        if self.ramping() {
            return self.poll_ramp(now, video_aus);
        }
        let due = self.fire_at.is_some_and(|at| now >= at);
        if !due {
            return None;
        }
        let pictures = if self.ramp.is_some() {
            video_aus
        } else {
            frames_completed
        };
        if self.active || pictures == 0 {
            self.fire_at = Some(now + PROBE_DELAY);
            return None;
        }
        self.fire_at = None;
        self.result_by = Some(now + PROBE_TIMEOUT);
        tracing::info!(
            target_kbps = self.target_kbps,
            duration_ms = PROBE_MS,
            "adaptive bitrate: startup link-capacity probe"
        );
        Some((self.target_kbps, PROBE_MS))
    }

    /// One ramp step per call: the first, or the next once the last drained.
    /// The first video frame ends the ramp wherever it stands — from then on
    /// nothing the controller does may cost a frame.
    fn poll_ramp(&mut self, now: Instant, video_aus: u64) -> Option<(u32, u32)> {
        let r = self.ramp.as_mut()?;
        if r.done {
            return None;
        }
        if video_aus > 0 {
            r.no_wall();
            return None;
        }
        if r.step.is_some() {
            return r.settle(now);
        }
        Some(r.begin(now))
    }

    /// The request never reached the control task: nothing is in flight, so
    /// nothing is owed. A dropped burst measured nothing, as a timed-out one.
    pub(crate) fn on_dropped(&mut self) {
        self.result_by = None;
        match self.ramp.as_mut() {
            Some(r) if !r.done => r.no_wall(),
            _ => self.no_evidence = true,
        }
    }

    /// A burst nobody answered. `true` when the embedder's probe state has to
    /// be released so reports resume.
    pub(crate) fn expired(&mut self, now: Instant) -> bool {
        if let Some(r) = self.ramp.as_mut() {
            if r.step.as_ref().is_some_and(|s| now >= s.deadline) {
                tracing::info!(
                    "adaptive bitrate: ramp step unanswered — keeping what the ramp proved"
                );
                r.no_wall();
                self.active = false;
                self.watchdog = None;
                return true;
            }
        }
        if self.watchdog.is_some_and(|at| now >= at) {
            self.watchdog = None;
            self.active = false;
            tracing::warn!(
                "speed-test probe unanswered — clearing it so loss reports and ABR resume"
            );
            return true;
        }
        if self.result_by.is_some_and(|at| now >= at) {
            self.result_by = None;
            self.no_evidence = true;
            tracing::info!(
                "adaptive bitrate: capacity probe timed out — nothing measured the link"
            );
            return true;
        }
        false
    }

    /// Nothing measured the link and nothing will. Taken once.
    pub(crate) fn take_no_evidence(&mut self) -> bool {
        std::mem::take(&mut self.no_evidence)
    }

    /// The ramp's verdict, once. `Some` exactly one tick after it stopped.
    ///
    /// A ramp that did not finish leaves the link's wall unmeasured, and a
    /// session that treats "no wall seen" as "no wall" climbs into it: on the
    /// rig, 38 % over a 237 Mbps link and 19 cuts in ten minutes. So the
    /// legacy burst is armed to finish the job the moment video flows. It
    /// costs the picture what it has always cost, in the only case that needs
    /// it, and it is a measurement rather than a guess. A pinned session
    /// skips that: it has no ceiling for the burst to set, and the pin it
    /// could not check stands. So does a session whose ports both ends know:
    /// they bound the link already, and a burst past them only loses frames.
    pub(crate) fn take_ramped(&mut self, now: Instant, wired: bool) -> Option<Ramped> {
        let r = self.ramp.as_mut()?;
        let out = r.outcome.take()?;
        if r.cut_short && !self.pinned && !wired {
            self.fire_at = Some(now + PROBE_DELAY);
            self.no_evidence = false;
        }
        Some(out)
    }

    /// Every step the ramp settled, oldest first.
    pub(crate) fn ramp_steps(&self) -> &[RampStep] {
        self.ramp.as_ref().map_or(&[], |r| r.history.as_slice())
    }

    /// What the ramp proved, once it has stopped.
    pub(crate) fn ramp_summary(&self) -> Option<RampSummary> {
        let r = self.ramp.as_ref().filter(|r| r.done)?;
        Some(RampSummary {
            wall: r.wall,
            proven_kbps: r.proven_kbps,
            steps: r.steps,
            asked_bytes: r.spent_bytes,
            took_ms: r.stopped_ms,
        })
    }

    /// The next burst this fires would be a ramp step.
    pub(crate) fn ramping(&self) -> bool {
        self.ramp.as_ref().is_some_and(|r| !r.done)
    }

    /// The ramp stopped before it had measured what it meant to.
    pub(crate) fn ramp_cut_short(&self) -> bool {
        self.ramp.as_ref().is_some_and(|r| r.cut_short)
    }

    /// This session measures with a ramp, so it never bursts beside video.
    pub(crate) fn has_ramp(&self) -> bool {
        self.ramp.is_some()
    }

    /// The host's end-of-burst report. A ramp step folds it in (the bytes are
    /// still arriving); the legacy burst is answered once, because the
    /// embedder mirrors a finished probe's state for as long as it stands and
    /// the same report arrives again on the next iteration.
    pub(crate) fn on_result(&mut self, r: ProbeReport, now: Instant) -> Measured {
        if self.ramping() {
            self.ramp.as_mut().expect("ramping").on_report(r, now);
            return Measured::NotOurs;
        }
        if self.result_by.take().is_none() {
            return Measured::NotOurs;
        }
        if r.host_duration_ms == 0 || r.delivered_bytes == 0 {
            tracing::info!("adaptive bitrate: capacity probe declined — nothing measured it");
            self.no_evidence = true;
            return Measured::Declined;
        }
        // Over the CLIENT receive interval: the host send window closes while
        // the bottleneck queue is still draining, so its duration overstates.
        let delivered_kbps =
            (r.delivered_bytes.saturating_mul(8) / u64::from(r.window_ms.max(1))) as u32;
        let ceiling = delivered_kbps.saturating_mul(7) / 10;
        // The burst asks for twice what the stream can use, so a link that
        // hands over much less than it was offered has a wall and this is
        // where it is. A link that kept up refused nothing and says nothing.
        let wall_kbps = link_refused(&r).then_some(delivered_kbps);
        tracing::info!(
            delivered_kbps,
            ceiling_kbps = ceiling,
            wall = wall_kbps.is_some(),
            client_interval_ms = r.client_interval_ms,
            host_duration_ms = r.host_duration_ms,
            "adaptive bitrate: link-capacity probe done — climb ceiling set"
        );
        Measured::Ceiling {
            ceiling_kbps: ceiling,
            wall_kbps,
        }
    }
}

/// Capacity-probe burst target in kbps. `set_ceiling` clamps to the stream
/// cap, so bits above `cap / 0.7` are discarded; ×2 clears the 1.43× bar with
/// margin.
///
/// `u32::MAX` (a mode [`stream_ceiling_kbps`](super::stream_ceiling_kbps)
/// declines to size) keeps 2 Gbps — also the hard ceiling; this can only
/// lower the target.
pub(crate) fn probe_target_kbps(stream_cap_kbps: u32) -> u32 {
    stream_cap_kbps.saturating_mul(2).min(2_000_000)
}

/// Where the ramp stops climbing: the rate that proves the stream cap
/// (`cap / 0.7`), or `PUNKTFUNK_ABR_PROBE_KBPS` when it is lower. Capacity
/// above what the stream can use is not this session's business, so the ramp
/// bounds itself and needs no absolute constant.
fn ramp_max_kbps(stream_cap_kbps: u32, env_kbps: Option<u32>) -> u32 {
    let by_stream = (u64::from(stream_cap_kbps) * 10)
        .div_ceil(7)
        .min(u64::from(u32::MAX)) as u32;
    env_kbps.map_or(by_stream, |k| k.min(by_stream))
}

/// A probe-only session's ramp ceiling: the host's own clamp on a probe.
const LINK_RAMP_MAX_KBPS: u32 = 10_000_000;

/// How far past its pin a pinned ramp climbs. The host paces a pinned
/// stream at the rate the ramp proved, so the proof has to reach the link's
/// rate, not the stream's: 8× takes a 1440p120 PyroWave pin to 10 GbE in
/// three more steps.
const PINNED_RAMP_HEADROOM: u32 = 8;

/// A pinned ramp's ceiling: the pin times [`PINNED_RAMP_HEADROOM`], or
/// `PUNKTFUNK_ABR_PROBE_KBPS` when it is lower.
fn pinned_ramp_max_kbps(pin_kbps: u32, env_kbps: Option<u32>) -> u32 {
    let by_pin = pin_kbps.saturating_mul(PINNED_RAMP_HEADROOM);
    env_kbps.map_or(by_pin, |k| k.min(by_pin))
}

/// What the session opens at, given what the ramp proved: half of it, and
/// never more than a clean picture at this mode wants. The caller floors it.
///
/// A wire budget, like `SetBitrate` and the measurement itself — at the
/// bottom of the range the two-shard parity floor puts ~25 % of it on FEC,
/// and mixing the two domains would open every weak link over its wall.
pub(crate) fn ramp_start_kbps(proven_kbps: u32, stream_cap_kbps: u32) -> u32 {
    let mode_rate = stream_cap_kbps / MODE_RATE_DIV;
    mode_rate.min(proven_kbps / (100 / RAMP_START_PCT))
}

/// The ceiling a wall licenses: well under what the link actually delivered,
/// because the reading is one moment of it.
pub(crate) fn wall_ceiling_kbps(delivered_kbps: u32) -> u32 {
    (u64::from(delivered_kbps) * u64::from(RAMP_CEILING_PCT) / 100) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quic::{CHROMA_IDC_420, CODEC_H264, CODEC_HEVC};

    fn report(delivered_bytes: u64, window_ms: u32) -> ProbeReport {
        ProbeReport {
            delivered_bytes,
            window_ms,
            host_duration_ms: 800,
            client_interval_ms: window_ms,
            client_interval_us: (window_ms) * 1_000,
            ..ProbeReport::default()
        }
    }

    /// A ramp driver: fire steps, answer each with a link of `capacity_kbps`,
    /// and return every step's target plus the verdict.
    struct Rig {
        p: CapacityProbe,
        now: Instant,
        asked: Vec<u32>,
        /// The step `settle` armed while answering the last one.
        pending: Option<(u32, u32)>,
        /// `Stats::frames_completed`, which the filler moves like any other
        /// AU. The ramp must not read it as video.
        completed: u64,
    }

    impl Rig {
        fn new(stream_cap_kbps: u32, env_kbps: Option<u32>) -> Self {
            let now = Instant::now();
            Rig {
                p: CapacityProbe::new(true, true, env_kbps, stream_cap_kbps, false, now),
                now,
                asked: Vec::new(),
                pending: None,
                completed: 0,
            }
        }

        fn at(&mut self, ms: u64) -> Instant {
            self.now += Duration::from_millis(ms);
            self.now
        }

        /// One step, answered by a link that carries `capacity_kbps` and a
        /// sender that manages `sender_kbps` of what was asked.
        fn step(&mut self, capacity_kbps: u32, sender_kbps: u32) -> Option<Ramped> {
            let now = self.now;
            let completed = self.completed;
            let Some((target, duration_ms)) = self
                .pending
                .take()
                .or_else(|| self.p.poll(now, completed, 0))
            else {
                return self.p.take_ramped(self.now, false);
            };
            self.asked.push(target);
            let sent_kbps = target.min(sender_kbps);
            let packets = (u64::from(sent_kbps) * u64::from(duration_ms) / 8 / 1_408).max(1);
            // The link stretches the arrivals when it cannot take the rate.
            let interval = (u64::from(duration_ms) * u64::from(sent_kbps)
                / u64::from(capacity_kbps.min(sent_kbps).max(1)))
            .max(1) as u32;
            let r = ProbeReport {
                delivered_bytes: packets * 1_448,
                delivered_packets: packets,
                window_ms: interval,
                host_duration_ms: duration_ms,
                client_interval_ms: interval,
                client_interval_us: (interval) * 1_000,
                host_bytes_sent: u64::from(sent_kbps) * u64::from(duration_ms) / 8,
                wire_packets_sent: packets as u32,
                send_dropped: 0,
            };
            let at = self.at(u64::from(interval));
            // Every filler AU completes, exactly as the session counts it.
            self.completed += packets;
            self.p.on_result(r, at);
            let at = self.at(RAMP_DRAIN_MS + 1);
            let completed = self.completed;
            self.pending = self.p.poll(at, completed, 0);
            self.p.take_ramped(self.now, false)
        }

        /// Run until the ramp stops, or the step budget runs out.
        fn run(&mut self, capacity_kbps: u32, sender_kbps: u32) -> Ramped {
            for _ in 0..24 {
                if let Some(end) = self.step(capacity_kbps, sender_kbps) {
                    return end;
                }
            }
            panic!("the ramp never stopped: {:?}", self.asked);
        }
    }

    /// The ramp doubles from 5 Mbps and stops at the first step the link
    /// cannot carry, with the rate that step actually delivered. The refused
    /// rate goes out twice: nothing the slack cannot explain went missing, so
    /// one reading of it is as much a reading of the client's scheduler as of
    /// the link.
    #[test]
    fn the_ramp_stops_at_the_first_step_the_link_refuses() {
        let mut rig = Rig::new(46_656, None); // 1080p30 HEVC
        let end = rig.run(12_500, u32::MAX);
        assert_eq!(
            rig.asked,
            [5_000, 10_000, 20_000, 20_000],
            "{:?}",
            rig.asked
        );
        let Ramped::Wall { delivered_kbps } = end else {
            panic!("a 12.5 Mbps link is a wall: {end:?}");
        };
        assert!(
            (11_000..=14_000).contains(&delivered_kbps),
            "the wall read {delivered_kbps} kbps"
        );
        // What it costs the link: four steps of 25 ms.
        let spent = rig.p.ramp_summary().expect("it stopped").asked_bytes;
        assert!(spent < 180_000, "the ramp asked for {spent} bytes");
    }

    /// A link with room to spare: the ramp stops at the rate that proves what
    /// the stream can use and latches no wall.
    #[test]
    fn the_ramp_stops_at_what_the_stream_can_use() {
        let cap = super::super::stream_ceiling_kbps(3840, 2160, 120, CODEC_HEVC, 8, CHROMA_IDC_420);
        let mut rig = Rig::new(cap, None);
        let end = rig.run(10_000_000, u32::MAX);
        let Ramped::NoWall { proven_kbps } = end else {
            panic!("a 10 GbE link has no wall: {end:?}");
        };
        assert!(
            proven_kbps >= cap,
            "{proven_kbps} kbps does not prove a {cap} kbps stream cap"
        );
        assert_eq!(
            *rig.asked.last().expect("steps"),
            ramp_max_kbps(cap, None),
            "the last step is the one that proves the cap"
        );
    }

    /// A speed test's 720p60 connect has no stream to stop at: on 2.5 GbE
    /// the ramp climbs to the link's wall, not to the ~79 Mbps 720p60 uses.
    #[test]
    fn a_probe_only_ramp_climbs_to_the_link() {
        let cap = super::super::stream_ceiling_kbps(1280, 720, 60, CODEC_H264, 8, CHROMA_IDC_420);
        let mut rig = Rig::new(cap, None);
        rig.p = CapacityProbe::new(true, true, None, cap, true, rig.now);
        let end = rig.run(2_500_000, u32::MAX);
        let Ramped::Wall { delivered_kbps } = end else {
            panic!("2.5 GbE is a wall to a speed test: {end:?}");
        };
        assert!(
            (2_000_000..=3_000_000).contains(&delivered_kbps),
            "the wall read {delivered_kbps} kbps"
        );
    }

    /// A sender that cannot offer the asked rate is not a wall: the link was
    /// never asked for it, so nothing about the link was learned.
    #[test]
    fn a_sender_limit_is_not_a_wall() {
        let mut rig = Rig::new(2_000_000, None);
        let end = rig.run(10_000_000, 60_000);
        let Ramped::NoWall { proven_kbps } = end else {
            panic!("the send path is the limit, not the link: {end:?}");
        };
        assert!(
            proven_kbps >= 50_000,
            "what the sender managed is still proved: {proven_kbps} kbps"
        );
    }

    /// `PUNKTFUNK_ABR_PROBE_KBPS` is the ramp's maximum — webOS pins 320 Mbps
    /// against a 4K165 stream cap of ~1 Gbps.
    #[test]
    fn the_env_target_caps_the_ramp() {
        let cap = super::super::stream_ceiling_kbps(3840, 2160, 165, CODEC_HEVC, 8, CHROMA_IDC_420);
        let mut rig = Rig::new(cap, Some(320_000));
        rig.run(10_000_000, u32::MAX);
        assert_eq!(*rig.asked.last().expect("steps"), 320_000);
        assert!(
            rig.asked.iter().all(|&k| k <= 320_000),
            "{:?} went past the pinned maximum",
            rig.asked
        );
    }

    /// A pinned ramp climbs past the pin: the proof is what the host paces
    /// the stream at. The env cap still binds.
    #[test]
    fn a_pinned_ramp_climbs_to_eight_times_the_pin() {
        assert_eq!(pinned_ramp_max_kbps(778_000, None), 6_224_000);
        assert_eq!(pinned_ramp_max_kbps(778_000, Some(320_000)), 320_000);
        assert_eq!(pinned_ramp_max_kbps(u32::MAX, None), u32::MAX);
    }

    /// The burst a cut-short ramp arms waits for a picture; the ramp's own
    /// filler in the session's completions is not one.
    #[test]
    fn a_cut_short_ramps_burst_waits_for_video_not_filler() {
        let mut rig = Rig::new(1_000_000, None);
        assert_eq!(rig.step(1_000_000, u32::MAX), None);
        rig.p.on_dropped();
        assert!(
            rig.p.take_ramped(rig.now, false).is_some(),
            "cut short, burst armed"
        );
        let completed = rig.completed;
        assert!(completed > 0, "step one's filler completed");
        let wait = PROBE_DELAY.as_millis() as u64 + 1;
        let at = rig.at(wait);
        assert!(rig.p.poll(at, completed, 0).is_none(), "no picture yet");
        let at = rig.at(wait);
        assert!(
            rig.p.poll(at, completed, 1).is_some(),
            "video flows: it fires"
        );
    }

    /// Video is the end of the ramp, whatever step is in flight: from the
    /// first frame nothing the controller does may cost a picture (L4).
    #[test]
    fn video_ends_the_ramp_where_it_stands() {
        let mut rig = Rig::new(1_000_000, None);
        assert_eq!(rig.step(1_000_000, u32::MAX), None);
        assert!(rig.pending.is_some(), "a second step went out");
        let (now, completed) = (rig.now, rig.completed);
        assert!(
            rig.p.poll(now, completed, 1).is_none(),
            "no step once video is here"
        );
        let Some(Ramped::NoWall { proven_kbps }) = rig.p.take_ramped(rig.now, false) else {
            panic!("the first frame ends the ramp with what it had")
        };
        assert!(proven_kbps >= 4_000, "step one still counts: {proven_kbps}");
    }

    /// A step nobody answers ends the ramp on its own deadline, and releases
    /// the embedder's probe state so the report tick resumes.
    #[test]
    fn an_unanswered_step_ends_the_ramp() {
        let mut rig = Rig::new(100_000, None);
        rig.p.poll(rig.now, 0, 0).expect("the first step goes out");
        let at = rig.at(RAMP_STEP_TIMEOUT.as_millis() as u64 + 1);
        assert!(rig.p.expired(at), "the pump's probe state must be released");
        assert_eq!(
            rig.p.take_ramped(rig.now, false),
            Some(Ramped::NoWall { proven_kbps: 0 }),
            "nothing was measured, and nothing is claimed"
        );
    }

    /// A step the host declined ends the ramp when the answer lands, with what
    /// the steps before it proved — not 1.5 s later on the step's deadline.
    #[test]
    fn a_declined_step_ends_the_ramp_at_once() {
        let mut rig = Rig::new(1_000_000, None);
        assert_eq!(rig.step(1_000_000, u32::MAX), None);
        rig.pending.take().expect("a second step went out");
        let at = rig.at(5);
        rig.p.on_result(ProbeReport::default(), at);
        let Some(Ramped::NoWall { proven_kbps }) = rig.p.take_ramped(at, false) else {
            panic!("a declined step ends the ramp with what it had")
        };
        assert!(proven_kbps >= 4_000, "step one still counts: {proven_kbps}");
        assert!(
            rig.p.ramp_cut_short(),
            "nothing above step one was measured"
        );
        assert_eq!(
            rig.p.ramp_steps().last().map(|s| s.end),
            Some(RampStepEnd::Declined)
        );
    }

    /// A step is over when its bytes stop arriving, not when the host says it
    /// stopped sending: 25 ms at 1 Gbps is 3 MB and fits the receive buffer,
    /// so "all bytes arrived" proves nothing about when.
    #[test]
    fn a_step_ends_on_the_clients_drain() {
        let mut rig = Rig::new(1_000_000, None);
        let (target, duration_ms) = rig.p.poll(rig.now, 0, 0).expect("the first step");
        let mut r = ProbeReport {
            delivered_packets: 2,
            delivered_bytes: 2 * 1_448,
            window_ms: duration_ms,
            host_duration_ms: duration_ms,
            client_interval_ms: duration_ms,
            client_interval_us: (duration_ms) * 1_000,
            host_bytes_sent: u64::from(target) * u64::from(duration_ms) / 8,
            wire_packets_sent: 8,
            send_dropped: 0,
        };
        // The host's report is in, but the buffer is still filling.
        for _ in 0..4 {
            let at = rig.at(RAMP_DRAIN_MS);
            r.delivered_packets += 2;
            r.delivered_bytes += 2 * 1_448;
            rig.p.on_result(r, at);
            assert!(
                rig.p.poll(at, 0, 0).is_none(),
                "a step whose bytes are still arriving is not over"
            );
        }
        let at = rig.at(RAMP_DRAIN_MS + 1);
        rig.p.on_result(r, at);
        assert!(
            rig.p.poll(at, 0, 0).is_some(),
            "and once they stop, the next step goes out"
        );
    }

    /// The verdict is read in microseconds, because a millisecond of
    /// rounding is 4 % of a 25 ms step and the decision turns on 10 %.
    ///
    /// Two steps that differ by 600 µs — one arrival pattern inside the same
    /// millisecond — land on opposite sides of the bar. Four of the rig's 28
    /// walls sat in exactly that band, one of them latching 80 Mbps on a
    /// 237 Mbps link.
    #[test]
    fn a_step_is_judged_in_microseconds() {
        for (interval_us, wall) in [(27_000u32, false), (27_600u32, true)] {
            let mut rig = Rig::new(1_026_432, None);
            let (target, duration_ms) = rig.p.poll(rig.now, 0, 0).expect("a step");
            assert_eq!(duration_ms, RAMP_STEP_MS, "the arithmetic below assumes it");
            let r = ProbeReport {
                delivered_bytes: 100 * 1_448,
                delivered_packets: 100,
                window_ms: interval_us / 1_000,
                host_duration_ms: duration_ms,
                client_interval_ms: interval_us / 1_000,
                client_interval_us: interval_us,
                host_bytes_sent: u64::from(target) * u64::from(duration_ms) / 8,
                wire_packets_sent: 100,
                send_dropped: 0,
            };
            // The loss is inside the slack either way, so a refusal is
            // asked again.
            for _ in 0..2 {
                let at = rig.at(u64::from(interval_us) / 1_000);
                rig.p.on_result(r, at);
                let at = rig.at(RAMP_DRAIN_MS + 1);
                rig.p.poll(at, 0, 0);
            }
            assert_eq!(
                matches!(rig.p.take_ramped(rig.now, false), Some(Ramped::Wall { .. })),
                wall,
                "{interval_us} us of arrivals against a 25 000 us ask"
            );
        }
    }

    /// A refused step missing a packet or two is the same evidence as one
    /// missing none: independent loss explains it and the reading still rests
    /// on the arrival times, so the rate goes out once more. A step the link
    /// thinned by a tenth or more is a wall on the spot — no ramp may spend a
    /// whole session under a policer because one packet went missing.
    #[test]
    fn a_refusal_missing_a_packet_is_asked_again_and_a_thinned_one_is_not() {
        for (sent, delivered, again) in [
            (24u32, 24u64, true),
            (24, 23, true),
            // The first step's dozen: one packet is 8 % of it.
            (12, 11, true),
            (12, 10, false),
            (60, 53, false),
            (112, 55, false),
        ] {
            let mut rig = Rig::new(46_656, None);
            let (target, duration_ms) = rig.p.poll(rig.now, 0, 0).expect("the first step");
            // Four times the asked span: refused whatever the packet count.
            let interval_ms = duration_ms * 4;
            let r = ProbeReport {
                delivered_bytes: delivered * 1_448,
                delivered_packets: delivered,
                window_ms: interval_ms,
                host_duration_ms: duration_ms,
                client_interval_ms: interval_ms,
                client_interval_us: interval_ms * 1_000,
                host_bytes_sent: u64::from(target) * u64::from(duration_ms) / 8,
                wire_packets_sent: sent,
                send_dropped: 0,
            };
            let at = rig.at(1);
            rig.p.on_result(r, at);
            let at = rig.at(RAMP_DRAIN_MS + 1);
            assert_eq!(
                rig.p.poll(at, 0, 0).is_some(),
                again,
                "{sent} sent, {delivered} delivered"
            );
            assert_eq!(
                matches!(rig.p.take_ramped(rig.now, false), Some(Ramped::Wall { .. })),
                !again,
                "{sent} sent, {delivered} delivered"
            );
        }
    }

    /// A short step's implied rate is noise: the rig measured the same 24
    /// packets arriving over 6 ms and over 15 ms, 2.5× apart. Whatever the
    /// arithmetic says, a step cannot have proved more than it offered — and
    /// one step alone must never open a session above the 20 000 it would
    /// have opened at with no measurement at all.
    #[test]
    fn a_step_cannot_prove_more_than_it_offered() {
        let mut rig = Rig::new(46_656, None);
        let (target, duration_ms) = rig.p.poll(rig.now, 0, 0).expect("the first step");
        assert_eq!(target, RAMP_START_KBPS);
        // The rig's own numbers: a 5 Mbps step's 24 wire packets, all of them,
        // in 6 ms. Filler parity puts 11 120 kbps of it on the wire.
        let r = ProbeReport {
            delivered_bytes: 34_752,
            delivered_packets: 24,
            window_ms: 6,
            host_duration_ms: duration_ms,
            client_interval_ms: 6,
            client_interval_us: (6) * 1_000,
            host_bytes_sent: u64::from(target) * u64::from(duration_ms) / 8,
            wire_packets_sent: 24,
            send_dropped: 0,
        };
        let at = rig.at(6);
        rig.p.on_result(r, at);
        let at = rig.at(RAMP_DRAIN_MS + 1);
        rig.p.poll(at, 0, 0);
        // Video arrives before step two can answer: one step is all there is.
        let at = rig.at(1);
        assert!(rig.p.poll(at, 0, 1).is_none());
        let Some(Ramped::NoWall { proven_kbps }) = rig.p.take_ramped(rig.now, false) else {
            panic!("one step, cut short by video, proves no wall")
        };
        assert_eq!(
            proven_kbps, 11_120,
            "the wire the step offered, not 46 Mbps of arithmetic from its 6 ms"
        );
        assert!(
            ramp_start_kbps(proven_kbps, 46_656) < 20_000,
            "a one-step ramp must not open a session above the unmeasured rate"
        );
    }

    /// A wall is what the link carried of the step's wire. The rig's tunnel
    /// (12.5 Mbit) refused a 10 Mbps step whose filler put 16.7 Mbps on the
    /// wire, twice, delivering 35 of 36 packets in 34 ms each time: the link
    /// carried 11.9 Mbps, and a wall read as the asked 10 Mbps sat a fifth low.
    #[test]
    fn a_wall_is_the_wire_the_link_carried() {
        let mut rig = Rig::new(93_312, None);
        let report = |packets: u64, sent: u32, bytes: u64, us: u32, asked: u32| ProbeReport {
            delivered_bytes: bytes,
            delivered_packets: packets,
            window_ms: us / 1_000,
            host_duration_ms: 25,
            client_interval_ms: us / 1_000,
            client_interval_us: us,
            host_bytes_sent: u64::from(asked) * 25 / 8,
            wire_packets_sent: sent,
            send_dropped: 0,
        };
        let (first, _) = rig.p.poll(rig.now, 0, 0).expect("the first step");
        let at = rig.at(20);
        rig.p.on_result(report(24, 24, 34_752, 20_000, first), at);
        let at = rig.at(RAMP_DRAIN_MS + 1);
        let (second, _) = rig.p.poll(at, 0, 0).expect("the second step");
        assert_eq!(second, 10_000);
        for _ in 0..2 {
            let at = rig.at(34);
            rig.p.on_result(report(35, 36, 50_680, 34_000, second), at);
            let at = rig.at(RAMP_DRAIN_MS + 1);
            rig.p.poll(at, 0, 0);
        }
        let Some(Ramped::Wall { delivered_kbps }) = rig.p.take_ramped(rig.now, false) else {
            panic!("refused twice is a wall")
        };
        assert_eq!(delivered_kbps, 11_924, "not the asked 10 000");
    }

    /// A ramp cut short by video hands the job to the legacy burst: it leaves
    /// the wall unmeasured, and a session that reads "no wall seen" as "no
    /// wall" climbs into it.
    #[test]
    fn an_unfinished_ramp_falls_back_to_the_burst() {
        let mut rig = Rig::new(1_026_432, None);
        assert_eq!(rig.step(245_000, u32::MAX), None);
        let now = rig.now;
        assert!(rig.p.poll(now, 0, 1).is_none(), "video ends the ramp");
        assert!(matches!(
            rig.p.take_ramped(now, false),
            Some(Ramped::NoWall { .. })
        ));
        assert!(
            !rig.p.take_no_evidence(),
            "a burst is coming, so the link is not unmeasurable"
        );
        // Two seconds after video, the burst the ramp could not replace.
        let at = rig.at(PROBE_DELAY.as_millis() as u64);
        let (target, duration_ms) = rig.p.poll(at, 40, 40).expect("the burst fires");
        assert_eq!(duration_ms, PROBE_MS);
        assert_eq!(target, probe_target_kbps(1_026_432));
        // And its answer binds, as an old host's always has.
        assert_eq!(
            rig.p.on_result(report(30_000_000, 800), at),
            Measured::Ceiling {
                ceiling_kbps: 210_000,
                wall_kbps: None
            }
        );
    }

    /// The legacy burst measures the link the way a ramp step does, so a
    /// burst the link refused is the same kind of mark toward the link cap.
    /// A burst the link kept up with refused nothing and marks nothing, and
    /// neither does one the host could not fill.
    #[test]
    fn a_burst_the_link_refused_reports_the_wall_it_found() {
        let refused = |delivered_packets: u64, wire_packets_sent: u32, send_dropped: u32| {
            let now = Instant::now();
            let mut p = CapacityProbe::new(true, false, Some(400_000), 100_000, false, now);
            assert!(p.poll(now + PROBE_DELAY, 1, 1).is_some(), "the burst fires");
            let r = ProbeReport {
                delivered_bytes: delivered_packets * 1_448,
                delivered_packets,
                window_ms: 800,
                host_duration_ms: 800,
                client_interval_ms: 800,
                client_interval_us: 800_000,
                host_bytes_sent: 40_000_000,
                wire_packets_sent,
                send_dropped,
            };
            match p.on_result(r, now) {
                Measured::Ceiling { wall_kbps, .. } => wall_kbps,
                other => panic!("the burst must measure: {other:?}"),
            }
        };
        // A tenth of what was offered came back: the link refused the rest.
        assert_eq!(refused(2_000, 20_000, 0), Some(28_960));
        // Everything offered came back: nothing was refused.
        assert_eq!(refused(20_000, 20_000, 0), None);
        // The host never put it on the wire, so the link was never asked.
        assert_eq!(refused(2_000, 20_000, 7), None);
        // One packet is no interval and no rate.
        assert_eq!(refused(1, 20_000, 0), None);
    }

    /// A ramp that finished has nothing left to ask: no burst is armed, and
    /// the picture is never touched.
    #[test]
    fn a_finished_ramp_never_bursts() {
        let mut rig = Rig::new(46_656, None);
        assert!(matches!(rig.run(12_500, u32::MAX), Ramped::Wall { .. }));
        let at = rig.at(2 * PROBE_DELAY.as_millis() as u64);
        assert!(
            rig.p.poll(at, 40, 40).is_none(),
            "the ramp measured the link; nothing may burst beside the picture"
        );
    }

    /// A step queued behind someone else's 450 ms of buffer is late, not
    /// empty. Judging it on the host's report alone reads a busy link as no
    /// link at all, and a newcomer would open on top of its sibling.
    #[test]
    fn a_step_still_in_the_queue_is_not_a_step_that_delivered_nothing() {
        let mut rig = Rig::new(46_656, None);
        let (target, duration_ms) = rig.p.poll(rig.now, 0, 0).expect("the first step");
        // The host says it is done; not one byte has reached us yet.
        let empty = ProbeReport {
            window_ms: duration_ms,
            host_duration_ms: duration_ms,
            host_bytes_sent: u64::from(target) * u64::from(duration_ms) / 8,
            wire_packets_sent: 11,
            ..ProbeReport::default()
        };
        for _ in 0..8 {
            let at = rig.at(RAMP_DRAIN_MS * 2);
            rig.p.on_result(empty, at);
            assert!(rig.p.poll(at, 0, 0).is_none(), "the step is not over");
            assert_eq!(
                rig.p.take_ramped(rig.now, false),
                None,
                "and the ramp has no verdict"
            );
        }
        // The queue hands them over, late and stretched. Nothing the slack
        // cannot explain went missing, so the rate is asked once more and
        // refused again: that is the wall.
        let late = ProbeReport {
            delivered_bytes: 11 * 1_448,
            delivered_packets: 11,
            client_interval_ms: duration_ms * 4,
            client_interval_us: (duration_ms * 4) * 1_000,
            ..empty
        };
        for pass in 0..2 {
            let at = rig.at(1);
            rig.p.on_result(late, at);
            let at = rig.at(RAMP_DRAIN_MS + 1);
            let again = rig.p.poll(at, 0, 0);
            assert_eq!(
                again.is_some(),
                pass == 0,
                "the refused rate goes out once more, and only once"
            );
        }
        assert!(
            matches!(rig.p.take_ramped(rig.now, false), Some(Ramped::Wall { .. })),
            "a rate that took four times its window twice over is a wall"
        );
    }

    /// The embedder's probe state keeps saying "done" until the next burst
    /// overwrites it, so the same report arrives on every iteration. Reading
    /// it twice would re-base the byte anchor forever, and the session would
    /// never see a window it could climb on.
    #[test]
    fn a_finished_burst_is_measured_exactly_once() {
        let now = Instant::now();
        let mut p = CapacityProbe::new(true, false, Some(400_000), 100_000, false, now);
        assert_eq!(p.poll(now + PROBE_DELAY, 1, 1), Some((400_000, PROBE_MS)));
        // 1 MB over 800 ms is 10 Mbps; the ceiling keeps 70 % of it.
        assert_eq!(
            p.on_result(report(1_000_000, 800), now),
            Measured::Ceiling {
                ceiling_kbps: 7_000,
                wall_kbps: None
            }
        );
        assert_eq!(
            p.on_result(report(1_000_000, 800), now),
            Measured::NotOurs,
            "the same report must not be read twice"
        );
    }

    /// A host without `HOST_CAP2_RAMP` gets exactly today's session: no
    /// steps, and the 800 ms burst two seconds after video flows.
    #[test]
    fn an_old_host_still_gets_the_legacy_burst() {
        let now = Instant::now();
        let mut p = CapacityProbe::new(true, false, None, 100_000, false, now);
        assert!(
            p.poll(now, 0, 0).is_none(),
            "nothing goes out during bring-up"
        );
        assert!(
            p.poll(now + PROBE_DELAY, 0, 0).is_none(),
            "nor before video"
        );
        assert_eq!(
            p.poll(now + 2 * PROBE_DELAY, 1, 1),
            Some((probe_target_kbps(100_000), PROBE_MS))
        );
        assert_eq!(
            p.take_ramped(now, false),
            None,
            "no ramp ran, so none has a verdict"
        );
    }

    /// `PUNKTFUNK_ABR_PROBE=0` measures nothing, whatever the host serves.
    #[test]
    fn a_disabled_probe_neither_ramps_nor_bursts() {
        let now = Instant::now();
        let mut p = CapacityProbe::new(false, true, None, 100_000, false, now);
        assert!(p.poll(now, 0, 0).is_none());
        assert!(p.poll(now + PROBE_DELAY, 1, 1).is_none());
        assert_eq!(p.take_ramped(now, false), None);
        assert!(!p.ramping());
    }

    /// A burst the control queue dropped measured nothing, as a timed-out one:
    /// the controller must not stay blind at its starting ceiling.
    #[test]
    fn a_dropped_burst_is_no_evidence() {
        let now = Instant::now();
        let mut p = CapacityProbe::new(true, false, None, 100_000, false, now);
        assert!(!p.take_no_evidence());
        assert!(
            p.poll(now + PROBE_DELAY, 1, 1).is_some(),
            "the burst goes out"
        );
        p.on_dropped();
        assert!(p.take_no_evidence());
    }

    /// An embedder speed test finishes too, and its numbers are not the
    /// controller's to learn from.
    #[test]
    fn a_probe_nobody_asked_for_teaches_nothing() {
        let now = Instant::now();
        let mut p = CapacityProbe::new(false, false, None, 100_000, false, now);
        assert_eq!(p.on_result(report(9_000_000, 800), now), Measured::NotOurs);
    }

    /// Burst must prove the stream cap and no more. Above `cap / 0.7`
    /// is discarded by `set_ceiling` (see
    /// `abr::tests::the_stream_bound_clamps_a_learned_ceiling_only`).
    #[test]
    fn the_probe_target_proves_the_stream_cap_without_overshooting_it() {
        for (w, h, hz, codec, depth) in [
            (1280, 720, 60, CODEC_HEVC, 8),
            (1920, 1080, 60, CODEC_H264, 8),
            (2560, 1440, 120, CODEC_HEVC, 8),
            (3840, 2160, 120, CODEC_HEVC, 10),
        ] {
            let cap = super::super::stream_ceiling_kbps(w, h, hz, codec, depth, CHROMA_IDC_420);
            let target = probe_target_kbps(cap);
            assert!(
                target.saturating_mul(7) / 10 >= cap,
                "{w}x{h}@{hz}: a {target} kbps burst cannot prove a {cap} kbps cap"
            );
            assert!(
                target <= cap.saturating_mul(2),
                "{w}x{h}@{hz}: {target} kbps chases capacity the clamp discards"
            );
            // And the ramp's own maximum proves the same cap with no margin.
            assert!(u64::from(ramp_max_kbps(cap, None)) * 7 / 10 >= u64::from(cap));
        }
        assert_eq!(probe_target_kbps(u32::MAX), 2_000_000);
        assert_eq!(probe_target_kbps(1_500_000), 2_000_000);
    }

    /// The byte cap shortens a step before the receive buffer can hide it.
    #[test]
    fn a_step_is_capped_in_bytes_as_well_as_time() {
        assert_eq!(step_ms(5_000), RAMP_STEP_MS);
        assert_eq!(
            step_ms(5_120_000),
            RAMP_STEP_MS,
            "16 MB is 25 ms at 5.12 Gbps"
        );
        assert_eq!(step_ms(8_000_000), 16);
        for kbps in [5_000u32, 100_000, 960_000, 2_000_000, 8_000_000] {
            let bytes = u64::from(kbps) * u64::from(step_ms(kbps)) / 8;
            assert!(bytes <= RAMP_STEP_BYTES, "{kbps} kbps sends {bytes} bytes");
        }
    }

    /// A wall licenses a ceiling under what the link actually delivered, and
    /// the session opens at the smaller of half that and what the mode wants.
    #[test]
    fn a_wall_licenses_less_than_it_delivered() {
        assert_eq!(wall_ceiling_kbps(12_500), 8_750);
        assert_eq!(wall_ceiling_kbps(1_000_000), 700_000);

        let cap_1080p60 =
            super::super::stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 8, CHROMA_IDC_420);
        // A fat link: the mode's own want binds, at ~0.15 bpp.
        assert_eq!(ramp_start_kbps(400_000, cap_1080p60), cap_1080p60 / 5);
        assert!((18_000..20_000).contains(&ramp_start_kbps(400_000, cap_1080p60)));
        // A 12.5 Mbps wall: half of what was delivered binds instead.
        assert_eq!(ramp_start_kbps(12_500, cap_1080p60), 6_250);
    }
}
