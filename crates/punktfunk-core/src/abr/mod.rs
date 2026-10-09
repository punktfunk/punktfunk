//! Adaptive bitrate: the controller behind the Automatic bitrate setting.
//!
//! [`Driver`] is the whole of it: events in — a completed AU, a stats
//! snapshot, a latency sample, an ack — and actions out of [`Driver::tick`].
//! An embedder owns no policy, so two clients cannot drift apart. The pump
//! runs it on the 750 ms cadence of the client's feedback window;
//! FEC absorbs short random loss, and the controller asks the host for a
//! different encoder rate via [`crate::quic::SetBitrate`] when congestion
//! persists.
//!
//! One module per concern: [`window`] assembles a report window, [`sample`]
//! is the closed window, [`verdict`] scores it, [`cap`] holds the learned
//! bounds — its `LearnedCap` is what the host reuses for its own encoder
//! ceiling — [`growth`] the climb law, [`probe`] the startup capacity burst,
//! and [`controller`] the state they all move. [`budget`] is the wire
//! arithmetic the host shares, and [`governor`] the policy it runs above
//! every session on one path. `sim/` drives this same
//! `Driver` against modelled links and pins every decision in a checked-in
//! baseline.

/// The link simulator and the checked-in baseline (`abr/sim/`). It drives the
/// client's frame channel constants, so it builds with the client.
#[cfg(all(test, feature = "quic"))]
mod sim;

pub mod budget;
mod cap;
mod controller;
pub mod governor;
mod growth;
#[cfg(test)]
mod harness;
mod link;
pub mod metrics;
pub(crate) mod probe;
mod sample;
mod shape;
mod verdict;
mod window;

pub use cap::LearnedCap;
use controller::BitrateController;
pub use link::LinkSource;
#[cfg(any(feature = "quic", test))]
pub(crate) use link::LINK_FLOOR_KBPS;
pub use probe::{ProbeReport, RampStep, RampStepEnd, RampSummary};
pub use sample::{DelayTrend, WindowActivity, WindowSample, WINDOW};
pub use shape::Shape;
pub use verdict::{shows_loss_shape, Reason};

use std::time::Instant;

/// What the session negotiated, plus the three environment overrides. Read
/// once, by the embedder, so nothing below the constructor touches the env.
#[derive(Clone, Copy, Debug)]
pub struct DriverConfig {
    /// Welcome-resolved Automatic rate, kbps. `0` = the embedder pinned a
    /// rate or the host predates renegotiation: the controller stays off.
    pub start_kbps: u32,
    /// `PUNKTFUNK_ABR_MAX_MBPS` as kbps. `None` = no cap.
    pub ceiling_cap_kbps: Option<u32>,
    /// Stream-shape bound on every learned ceiling, for the negotiated
    /// geometry. A mode switch recomputes it from the new one.
    pub stream_cap_kbps: u32,
    /// Negotiated refresh: the frame budget the latency thresholds are
    /// sized in.
    pub refresh_hz: u32,
    /// Codec and sample shape, which a mode switch does not change.
    pub codec: u8,
    pub bit_depth: u8,
    pub chroma_format: u8,
    /// Audio's wire reservation, spent whether video flows or not.
    pub audio_reserved_kbps: u32,
    /// Host marks idle-keepalive repeats (`HOST_CAP2_REPEAT_MARK`).
    pub marks_repeats: bool,
    /// Run the startup capacity probe (`PUNKTFUNK_ABR_PROBE`).
    pub probe: bool,
    /// `PUNKTFUNK_ABR_PROBE_KBPS`. `None` = twice the stream-shape cap; with
    /// [`ramp`](Self::ramp) it is the ramp's maximum instead.
    pub probe_target_kbps: Option<u32>,
    /// Host serves probe requests during bring-up
    /// ([`HOST_CAP2_RAMP`](crate::quic::HOST_CAP2_RAMP)): measure the link
    /// before the first frame instead of bursting beside it.
    pub ramp: bool,
    /// A diagnostic session ([`EXT_DELIVERY_PROBE_ONLY`](crate::quic::EXT_DELIVERY_PROBE_ONLY)):
    /// no stream spends the rate, so the ramp climbs to the link's wall, not the stream cap.
    pub probe_only: bool,
    /// PyroWave Automatic: the pin the Welcome resolved, kbps. `Some` runs
    /// the bring-up ramp as a fit check on that pin — a measured wall lowers
    /// it once, every other outcome leaves it. The controller stays off
    /// either way, and the pin is never raised from a measurement.
    pub pin_kbps: Option<u32>,
}

/// What the embedder has to do for the controller. Everything else it does
/// with a window — jump-to-live, standing latency, the frame hand-off — is
/// its own business.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// One window's report: shard loss in ppm, the host's adaptive-FEC input; the
    /// session's packets received, which tell "no loss" from "nothing arrives" and divide a
    /// path two of the host's sessions share; and where the window's loss fell.
    Report {
        loss_ppm: u32,
        packets_received: u64,
        head: u32,
        mid: u32,
        tail: u32,
        sock_drops: u32,
    },
    /// The link rate `L` the host paces at, kbps, whenever it changes.
    LinkRate(u32),
    /// The shape the client asks the host for (`Shape as u8`).
    Shape(u8),
    /// Ask the host for a new encoder rate.
    SetBitrate(u32),
    /// Ask for a capacity burst. `ramp` = a bring-up step, before any video
    /// exists: the embedder keeps counting its bytes after the host's report
    /// lands, because the queue is still draining toward it.
    Probe {
        target_kbps: u32,
        duration_ms: u32,
        ramp: bool,
    },
    /// Ask for a keyframe: the picture needs re-anchoring.
    Keyframe,
    /// Let the in-flight probe go — it was never answered. Reports resume.
    AbandonProbe,
}

/// One pump iteration's worth of decisions.
pub struct Tick {
    /// In the order they should go out.
    pub actions: Vec<Action>,
    /// `Some` when the report window closed on this tick.
    pub window: Option<ClosedWindow>,
}

/// The report window that just closed, for an embedder that has its own
/// per-window duties.
pub struct ClosedWindow {
    /// What the controller judged — or would have, had the window stood.
    pub sample: WindowSample,
    /// The window described a burst tail or a host rebuild, not the link.
    pub discarded: bool,
    /// It delivered everything the link was asked for: no loss, no lost
    /// frame, and not discarded. A standing-latency detector needs exactly
    /// that.
    pub loss_free: bool,
}

/// A closed window with what the controller did about it, for an embedder
/// that writes a session's trajectory down.
///
/// The netem rig reads these rather than re-deriving a window from the wire:
/// a trajectory that disagreed with the controller would be measuring the
/// recorder.
#[derive(Clone, Copy, Debug)]
pub struct WindowRecord {
    /// Milliseconds from the session's start to this window's close.
    pub t_ms: u64,
    /// Rate the session was running at for this window.
    pub rate_kbps: u32,
    /// What the controller asked for on this window, if it asked.
    pub request_kbps: Option<u32>,
    pub sample: WindowSample,
    pub discarded: bool,
    /// What the window was judged to be, and so what named any rate change.
    pub reason: Reason,
}

/// What the bring-up ramp measured, for a client writing it down.
#[derive(Clone, Debug)]
pub struct RampRecord {
    /// Every settled step, oldest first.
    pub steps: Vec<RampStep>,
    pub outcome: RampSummary,
    /// The rate the session opened at, from the measurement.
    pub opening_kbps: u32,
}

impl WindowRecord {
    /// This window as the metrics read it.
    pub fn metric(&self) -> metrics::MetricWindow {
        metrics::MetricWindow {
            t_ms: self.t_ms,
            rate_kbps: self.rate_kbps,
            request_kbps: self.request_kbps,
            dropped: self.sample.dropped,
            discarded: self.discarded,
        }
    }
}

/// Automatic bitrate, whole: the window the embedder feeds, the controller
/// that judges it, and the startup capacity probe.
pub struct Driver {
    abr: BitrateController,
    window: window::WindowAccumulator,
    probe: probe::CapacityProbe,
    /// What this mode can use. A mode switch recomputes it; the ramp's start
    /// rate is a share of it.
    stream_cap_kbps: u32,
    /// A mode switch changes the geometry, not these.
    codec: u8,
    bit_depth: u8,
    chroma_format: u8,
    /// The pin a PyroWave Automatic session negotiated, when it has one. The
    /// ramp's verdict is judged against it in [`on_ramped`](Self::on_ramped).
    pin_kbps: Option<u32>,
    /// Raised between ticks (a probe ended, a burst was abandoned) and sent
    /// on the next one, microseconds later.
    pending: Vec<Action>,
    /// Acks since the last window closed, with the reason each carried.
    acks: Vec<(u32, Option<crate::quic::AckReason>)>,
    /// Picture AUs the embedder has forwarded. `Stats::frames_completed`
    /// cannot answer "has video started": it counts the measurement's own
    /// filler AUs too (`session.rs` completes an AU without asking whose
    /// index space it is in).
    video_aus: u64,
    /// `L`, and the last one the host was told.
    link: link::LinkRate,
    link_sent: u32,
    /// The controller's wall as last read; only a change moves `L`.
    seen_wall: Option<u32>,
    shape: shape::ShapeState,
    /// Consecutive windows of tail loss with no frame lost.
    tail_run: u8,
}

impl Driver {
    pub fn new(cfg: DriverConfig, now: Instant) -> Self {
        let mut abr = BitrateController::new(cfg.start_kbps, cfg.ceiling_cap_kbps);
        // Bound the probe by stream shape, not raw link capacity: a fat LAN
        // otherwise licenses rates no inter-coded stream can use.
        abr.set_stream_cap(cfg.stream_cap_kbps);
        // Encode thresholds in this session's frame budgets, not the 120 Hz
        // durations they were calibrated at.
        abr.set_frame_budget(cfg.refresh_hz);
        let pin_kbps = cfg.pin_kbps.filter(|&pin| pin > 0);
        if let Some(pin) = pin_kbps {
            // A pinned session's running rate is the pin itself. The
            // controller is off, so the number windows are judged against
            // is seeded here and moves only with the host's acks.
            abr.current_kbps = pin;
        }
        Driver {
            abr,
            window: window::WindowAccumulator::new(cfg.audio_reserved_kbps, cfg.marks_repeats, now),
            // A pinned or explicit rate has nothing to measure for — except
            // a PyroWave pin, which the ramp checks once before the first
            // frame. The pinned probe never arms the beside-video burst.
            probe: match pin_kbps {
                Some(pin) => probe::CapacityProbe::for_pinned(
                    cfg.probe && cfg.ramp,
                    cfg.probe_target_kbps,
                    pin,
                    now,
                ),
                None => probe::CapacityProbe::new(
                    cfg.probe && cfg.start_kbps > 0,
                    cfg.ramp,
                    cfg.probe_target_kbps,
                    cfg.stream_cap_kbps,
                    cfg.probe_only,
                    now,
                ),
            },
            stream_cap_kbps: cfg.stream_cap_kbps,
            codec: cfg.codec,
            bit_depth: cfg.bit_depth,
            chroma_format: cfg.chroma_format,
            pin_kbps,
            pending: Vec::new(),
            acks: Vec::new(),
            video_aus: 0,
            link: link::LinkRate::default(),
            link_sent: 0,
            seen_wall: None,
            shape: shape::ShapeState::default(),
            tail_run: 0,
        }
    }

    /// Both ends' ports: the host's from its `StreamConfig`, this device's from the dial.
    pub fn set_ports(&mut self, host: crate::quic::LinkFacts, client: crate::quic::LinkFacts) {
        self.link.set_ports(host, client);
    }

    /// The link rate `L`, kbps, and where it came from.
    pub fn link(&self) -> (u32, LinkSource) {
        (self.link.kbps(), self.link.source())
    }

    /// The wake shape is on.
    pub fn wake(&self) -> bool {
        self.shape.on()
    }

    /// Loss by position, as the session drains it ([`crate::session::Session::take_loss_positions`]).
    pub fn on_loss_positions(&mut self, p: crate::packet::LossPositions) {
        self.window.on_loss_positions(p);
    }

    /// The receiver's own drops since the session opened: the kernel socket's and the demux
    /// queue's. A running total; each window takes its delta.
    pub fn on_sock_drops(&mut self, total: u64) {
        self.window.on_sock_drops(total);
    }

    /// The session counters, once per embedder iteration.
    pub fn on_stats(&mut self, st: &crate::stats::Stats) {
        self.window.on_stats(st);
    }

    /// One completed access unit; `repeat` is the host's idle keepalive mark.
    /// The embedder calls this for picture only — filler never reaches it —
    /// which is what makes it the ramp's "video has started" signal.
    pub fn on_au(&mut self, repeat: bool) {
        self.video_aus += 1;
        self.window.on_au(repeat);
    }

    /// Capture → received for one AU, ns.
    pub fn on_owd(&mut self, ns: i128) {
        self.window.on_owd(ns);
    }

    /// Capture → first-shard arrival for one frame, ns, clock offset already
    /// applied. Fed from [`crate::session::Session::take_shard_delays`]: a
    /// frame that never completes has one of these, so the window's delay
    /// reading survives the overload that produced it.
    pub fn on_shard_owd(&mut self, ns: i128) {
        self.window.on_shard_owd(ns);
    }

    /// The window's client decode-stage total and its sample count.
    pub fn on_decode_latency(&mut self, sum_us: u64, count: u32) {
        self.window.on_decode_latency(sum_us, count);
    }

    /// The window's host encode-stage total and its sample count.
    pub fn on_encode_latency(&mut self, sum_us: u64, count: u32) {
        self.window.on_encode_latency(sum_us, count);
    }

    /// Decode-recovery keyframe asks that went out.
    pub fn on_keyframe_asks(&mut self, n: u32) {
        self.window.on_keyframe_asks(n);
    }

    /// A jump-to-live: the client could not hold the rate.
    pub fn on_flush(&mut self) {
        self.window.on_flush();
    }

    /// The host rebuilt its pipeline. The window in flight describes the gap,
    /// not the link — drop it. A gap that straddled a boundary already fed the
    /// previous window; holding every window back would be a permanent lag.
    pub fn on_pipeline_gap(&mut self, gap_ms: u32) {
        self.discard_window();
        tracing::debug!(
            gap_ms,
            window_ms = self.window.open_ms(),
            "host pipeline gap — the report window in flight is discarded"
        );
    }

    /// Host [`crate::quic::BitrateChanged`], in arrival order. Applied when
    /// the window closes: the rate the controller judges a window against is
    /// the one that was running for it. `why` is `None` from a host that does
    /// not name its limits.
    pub fn on_ack(&mut self, kbps: u32, why: Option<crate::quic::AckReason>) {
        self.acks.push((kbps, why));
    }

    /// A [`Action::SetBitrate`] that never reached the host.
    pub fn on_request_dropped(&mut self, kbps: u32) {
        self.abr.on_request_dropped();
        tracing::warn!(
            kbps,
            "adaptive bitrate: control queue full — re-target dropped"
        );
    }

    /// A [`Action::Probe`] that never reached the host.
    pub fn on_probe_dropped(&mut self) {
        self.probe.on_dropped();
    }

    /// The accepted mode changed. Encoder and decoder knees and the rolling
    /// baselines are properties of the mode; the probe-measured link ceiling
    /// is not, and survives.
    pub fn on_mode_switch(&mut self, width: u32, height: u32, refresh_hz: u32) {
        self.abr.on_mode_switch();
        self.abr.set_frame_budget(refresh_hz);
        self.stream_cap_kbps = stream_ceiling_kbps(
            width,
            height,
            refresh_hz,
            self.codec,
            self.bit_depth,
            self.chroma_format,
        );
        // Rebinds an already-learned ceiling downward for the new geometry.
        self.abr.set_stream_cap(self.stream_cap_kbps);
    }

    /// A burst went in or out of flight — ours or an embedder speed test.
    ///
    /// The burst lands in the packet and byte counters but never in the
    /// decoder, so its end rebases every anchor past it and the window it
    /// straddled is discarded. A burst that took the keyframe with it is
    /// followed by an ask for a new one.
    pub fn on_probe_active(&mut self, active: bool, duration_ms: u32, now: Instant) {
        let frames_completed = self.window.stats().frames_completed;
        let Some(frames_at_start) =
            self.probe
                .on_active(active, duration_ms, frames_completed, now)
        else {
            return;
        };
        self.window.rebase(now);
        // A ramp session never bursts beside video: its steps run before a
        // picture exists, so none of them can have taken a reference.
        if !self.probe.has_ramp() && frames_completed == frames_at_start {
            self.pending.push(Action::Keyframe);
            tracing::warn!(
                "no frame survived the capacity probe — requested a keyframe to re-anchor"
            );
        }
    }

    /// The host's end-of-burst report. A ramp step is still draining when it
    /// lands, so the ramp folds every re-presentation in until the bytes stop.
    pub fn on_probe_result(&mut self, r: ProbeReport, now: Instant) {
        match self.probe.on_result(r, now) {
            probe::Measured::NotOurs => return,
            probe::Measured::Declined => {}
            probe::Measured::Ceiling {
                ceiling_kbps,
                wall_kbps,
            } => {
                self.set_ceiling(ceiling_kbps);
                // The ramp's bargain, for the same reason. The burst reads
                // low for a second one — it counts its own filler while video
                // shares the link — so re-asking it matters more, not less.
                // A burst the link kept up with refused nothing and leaves
                // the ceiling alone, as it always has.
                if let Some(kbps) = wall_kbps {
                    self.abr.note_measured_wall(ceiling_kbps, kbps);
                }
            }
        }
        // Skips video that landed under a suppressed report tick; `rebase`
        // already netted the filler out.
        self.window.rebase_bytes();
    }

    /// What the bring-up ramp came to, once it has stopped.
    #[cfg(all(test, feature = "quic"))]
    pub(crate) fn ramp_summary(&self) -> Option<probe::RampSummary> {
        self.probe.ramp_summary()
    }

    /// What the last judged window was, and so what named the last rate
    /// change. A cut nobody can attribute is a bug; this is what an overlay
    /// shows and a field report quotes.
    pub fn reason(&self) -> Reason {
        self.abr.last_reason()
    }

    /// What the last rate cut was for, until the host grants a climb. Narrower
    /// than [`reason`](Self::reason), which follows every window: an overlay
    /// shows why the rate is where it is, not what just went by.
    pub fn last_cut(&self) -> Option<Reason> {
        self.abr.last_cut()
    }

    /// The rate the session is running at — the host's latest ack, or the
    /// Welcome rate before one. What a window is judged against.
    pub fn target_kbps(&self) -> u32 {
        self.abr.current_kbps
    }

    /// Every bring-up ramp step that settled, oldest first, and what the ramp
    /// came to once it stopped. Read-only: a rig writes the measurement down
    /// from here rather than re-deriving it from the wire.
    pub fn ramp_steps(&self) -> &[RampStep] {
        self.probe.ramp_steps()
    }

    pub fn ramp_outcome(&self) -> Option<RampSummary> {
        self.probe.ramp_summary()
    }

    /// A measured link capacity. The first one binds up or down — the rate the
    /// Welcome resolved is not evidence about the link — and later ones only
    /// raise: a congested moment must not shrink what a measurement proved.
    pub fn set_ceiling(&mut self, kbps: u32) {
        self.abr.set_ceiling(kbps);
    }

    /// This window describes something other than the link.
    pub fn discard_window(&mut self) {
        self.window.discard();
    }

    /// What the bring-up ramp proved, applied once: the authority first, then
    /// the rate the session opens at.
    ///
    /// A wall is a measured link capacity and binds like any other. No wall
    /// means the ramp asked for everything this stream can use and the link
    /// gave it: the stream shape is the only bound left, so authority goes
    /// there. Nothing measured at all licenses nothing.
    ///
    /// A pinned session reads the verdict instead: the pin drops to
    /// `min(pin, 0.7 × delivered)` on a wall, and only on a wall — a floor
    /// under the link, an unreadable step, and a ramp cut short are not
    /// walls, so none of them moves the pin. The one ask goes out before
    /// the first frame; a ramp that never ran produces no ask at all.
    fn on_ramped(&mut self, ramped: probe::Ramped, now: Instant) -> Option<u32> {
        if let Some(pin) = self.pin_kbps {
            let probe::Ramped::Wall { delivered_kbps } = ramped else {
                return None;
            };
            let fit = probe::wall_ceiling_kbps(delivered_kbps).min(pin);
            if fit > 0 && fit < pin {
                tracing::info!(
                    pin_kbps = pin,
                    fit_kbps = fit,
                    delivered_kbps,
                    "adaptive bitrate: PyroWave pin lowered to what the ramp measured"
                );
                return Some(fit);
            }
            return None;
        }
        let proven_kbps = match ramped {
            probe::Ramped::Wall { delivered_kbps } => {
                // The wall licenses a rate as it always has, and that rate is
                // the link cap too, so the clock asks the wall again and
                // carries the ceiling with the answer. A reading is one
                // moment of a link that moves; bound for a session's life it
                // left 45 % of the rig's tunnel unused (§4.1).
                let licensed = probe::wall_ceiling_kbps(delivered_kbps);
                self.set_ceiling(licensed);
                self.abr.note_measured_wall(licensed, delivered_kbps);
                delivered_kbps
            }
            probe::Ramped::NoWall { proven_kbps } if proven_kbps > 0 => {
                if !self.probe.ramp_cut_short() {
                    self.set_ceiling(self.stream_cap_kbps);
                }
                proven_kbps
            }
            // The ramp stopped before it proved anything: the link is as
            // unmeasured as if it had never run.
            probe::Ramped::NoWall { .. } => {
                if !self.probe.ramp_cut_short() {
                    self.abr.no_link_evidence(self.stream_cap_kbps);
                }
                return None;
            }
        };
        // A ramp the first frame cut short proved only a floor; known ports say more.
        let basis = if self.probe.ramp_cut_short() && self.link.wired() {
            self.link.kbps()
        } else {
            proven_kbps
        };
        let start = probe::ramp_start_kbps(basis, self.stream_cap_kbps);
        self.abr.start_from_measurement(start, now)
    }

    /// Everything the session owes right now. Called every embedder
    /// iteration; the report window closes inside it, on its own cadence.
    pub fn tick(&mut self, now: Instant) -> Tick {
        let mut actions = std::mem::take(&mut self.pending);
        if self.probe.expired(now) {
            actions.push(Action::AbandonProbe);
        }
        let ramping = self.probe.ramping();
        if let Some((target_kbps, duration_ms)) =
            self.probe
                .poll(now, self.window.stats().frames_completed, self.video_aus)
        {
            actions.push(Action::Probe {
                target_kbps,
                duration_ms,
                ramp: ramping,
            });
        }
        if self.probe.take_no_evidence() {
            self.abr.no_link_evidence(self.stream_cap_kbps);
        }
        if let Some(ramped) = self.probe.take_ramped(now, self.link.wired()) {
            // A cut-short ramp still proved a floor the link delivered.
            self.link.ramped(ramped);
            if let Some(kbps) = self.on_ramped(ramped, now) {
                actions.push(Action::SetBitrate(kbps));
            }
        }
        if !self.window.due(now, self.probe.active()) {
            self.tell_link(&mut actions);
            return Tick {
                actions,
                window: None,
            };
        }
        let closed = self.window.close(now);
        // The host's answers to what this window's predecessors asked for,
        // learned before the controller judges this one.
        for (kbps, why) in std::mem::take(&mut self.acks) {
            self.abr.on_ack(kbps, why);
        }
        let w = &closed.sample;
        if closed.discarded {
            // The loss report goes with the window: a probe tail would spike
            // host FEC off deliberate overload, and a rebuild window has a
            // near-zero denominator.
            tracing::debug!(
                loss_ppm = w.loss_ppm,
                window_dropped = w.dropped,
                "discarding this ABR window (probe tail or a host pipeline gap)"
            );
        } else {
            actions.push(Action::Report {
                loss_ppm: w.loss_ppm,
                packets_received: closed.packets_received,
                head: w.head,
                mid: w.mid,
                tail: w.tail,
                sock_drops: w.sock_drops,
            });
            if let Some(kbps) = self.abr.on_window(w) {
                // Log the window's signals with the decision, so decode- and
                // encode-driven retargets are separable from network ones.
                tracing::info!(
                    kbps,
                    loss_ppm = w.loss_ppm,
                    owd_mean_us = w.owd_mean_us.unwrap_or(-1),
                    decode_mean_us = w.decode_mean_us.unwrap_or(-1),
                    encode_mean_us = w.encode_mean_us.unwrap_or(-1),
                    actual_kbps = w.actual_kbps,
                    flushed = w.flushed,
                    recovery_kf = w.recovery_kf,
                    reason = ?self.reason(),
                    "adaptive bitrate: requesting encoder re-target"
                );
                actions.push(Action::SetBitrate(kbps));
            }
            self.after_window(w, &mut actions);
        }
        self.tell_link(&mut actions);
        Tick {
            actions,
            window: Some(ClosedWindow {
                // A discarded window is NOT loss-free: probe residue is not
                // evidence of anything.
                loss_free: !closed.discarded && w.loss_ppm == 0 && w.dropped == 0,
                sample: closed.sample,
                discarded: closed.discarded,
            }),
        }
    }
}

impl Driver {
    /// What a judged window says about the link and the receiver: tail loss two windows
    /// running with no frame lost is a mark that takes `L` down a notch, without a cut;
    /// tail loss that kills a frame is the controller's cut and mark, and restarts the
    /// calm count; head loss switches the wake shape; the controller's wall moves `L`.
    fn after_window(&mut self, w: &WindowSample, actions: &mut Vec<Action>) {
        if verdict::tail_mark(w) {
            self.tail_run = self.tail_run.saturating_add(1);
            if self.tail_run >= 2 {
                self.tail_run = 0;
                self.link.tail_mark();
                tracing::info!(
                    tail = w.tail,
                    link_kbps = self.link.kbps(),
                    "tail loss with every frame whole: the link rate comes down a notch"
                );
            }
        } else {
            self.tail_run = 0;
            if verdict::tail_signature(w) {
                self.link.tail_loss();
            } else {
                self.link.calm_window();
            }
        }
        let calm = w.head <= w.mid.saturating_add(w.tail);
        if let Some(s) = self.shape.note(verdict::head_signature(w), calm, w.now) {
            tracing::info!(
                head = w.head,
                mid = w.mid,
                tail = w.tail,
                shape = ?s,
                "delivery shape changed: loss at the frames' heads says the receiver wakes late"
            );
            actions.push(Action::Shape(s as u8));
        }
        let wall = self.abr.link_wall_kbps();
        if wall != self.seen_wall {
            match (self.seen_wall, wall) {
                (Some(was), Some(k)) if k > was => self.link.lifted(k),
                (_, Some(k)) => self.link.wall(k),
                (_, None) => self.link.dropped_cap(),
            }
            self.seen_wall = wall;
        }
    }

    /// `L` to the host whenever it moved, the first one before anything was measured.
    fn tell_link(&mut self, actions: &mut Vec<Action>) {
        let kbps = self.link.kbps();
        if kbps != self.link_sent {
            self.link_sent = kbps;
            actions.push(Action::LinkRate(kbps));
        }
    }
}

/// Upper bound on bitrate this stream's shape could use, in kbps.
///
/// The probe-measured ceiling is pure link capacity (`delivered × 0.7`) with
/// no term for pixels. A CBR encoder fills whatever target it is handed, so
/// utilization never supplies one. Deliberately generous: a bound on the
/// absurd, not a quality opinion. Explicit-bitrate and PyroWave sessions
/// never reach here.
pub(crate) fn stream_ceiling_kbps(
    width: u32,
    height: u32,
    refresh_hz: u32,
    codec: u8,
    bit_depth: u8,
    chroma_format: u8,
) -> u32 {
    let pixel_rate = (width as u64)
        .saturating_mul(height as u64)
        .saturating_mul(refresh_hz.max(1) as u64);
    if pixel_rate == 0 {
        return u32::MAX;
    }
    // Milli-bits per pixel so the arithmetic stays integer. H.264 is the
    // least efficient of the three and is allowed correspondingly more.
    let milli_bpp: u64 = match codec {
        crate::quic::CODEC_H264 => 1_000,
        _ => 750,
    };
    // 10-bit is 25 % more sample depth; 4:4:4 is twice the chroma of 4:2:0
    // → half again as many samples overall.
    let milli_bpp = if bit_depth >= 10 {
        milli_bpp * 5 / 4
    } else {
        milli_bpp
    };
    let milli_bpp = if chroma_format == crate::quic::CHROMA_IDC_444 {
        milli_bpp * 3 / 2
    } else {
        milli_bpp
    };
    // bits/s = pixel_rate × bpp; kbps = that / 1000. The milli- factor and
    // the kbps divisor cancel: pixel_rate × milli_bpp / 1_000_000.
    u32::try_from(pixel_rate.saturating_mul(milli_bpp) / 1_000_000).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::Stats;

    /// The bring-up ramp, driven the way the pump drives it: the step's own
    /// filler AUs complete and move `Stats::frames_completed`, the host's
    /// report lands after the bytes it describes, and picture arrives as
    /// [`Driver::on_au`].
    ///
    /// The counter is the trap this pins. `frames_completed` cannot mean
    /// "video has started" — the reassembler completes a filler AU without
    /// asking whose index space it is in (`session.rs`), so a 5 Mbps step
    /// moves it six times inside 25 ms. Reading it that way ended the ramp
    /// one tick after its first step, before that step's own report arrived:
    /// 40 of 40 runs on the rig, every profile, `proven_kbps=0`.
    #[test]
    fn the_ramp_steps_until_video_arrives() {
        /// Header plus shard, as the reassembler counts a filler packet.
        const WIRE: u64 = 1_448;
        /// The host chunks filler into AUs; six of them make a 5 Mbps step.
        const PACKETS_PER_AU: u64 = 2;
        /// The report rides the control stream and lands after the burst.
        const REPORT_LAG_MS: u64 = 5;

        struct Step {
            target_kbps: u32,
            duration_ms: u64,
            last_filler_ms: u64,
            report_at_ms: u64,
            packets: u64,
            first_ms: u64,
        }

        let base = Instant::now();
        let at = |ms: u64| base + std::time::Duration::from_millis(ms);
        let mut d = Driver::new(
            DriverConfig {
                start_kbps: 20_000,
                ceiling_cap_kbps: None,
                stream_cap_kbps: 200_000,
                refresh_hz: 60,
                codec: crate::quic::CODEC_HEVC,
                bit_depth: 8,
                chroma_format: crate::quic::CHROMA_IDC_420,
                audio_reserved_kbps: 0,
                marks_repeats: true,
                probe: true,
                probe_target_kbps: None,
                ramp: true,
                probe_only: false,
                pin_kbps: None,
            },
            base,
        );
        let mut st = Stats::default();
        let mut step: Option<Step> = None;
        let mut asked: Vec<(u64, u32)> = Vec::new();
        let mut opened_at: Option<(u64, u32)> = None;
        let mut link_rate: Option<u32> = None;
        let mut completed_when_asked: Vec<u64> = Vec::new();
        // Video starts once two steps are done — a bring-up the ramp fits in.
        let mut video_from_ms = u64::MAX;
        let mut first_au_ms = u64::MAX;

        for ms in 0..600 {
            // The link hands the filler over while the step runs.
            if let Some(s) = step.as_mut() {
                if ms <= s.last_filler_ms {
                    s.packets += 1;
                    if s.first_ms == 0 {
                        s.first_ms = ms;
                    }
                    st.packets_received += 1;
                    st.probe_packets_received += 1;
                    st.bytes_received += WIRE;
                    st.probe_bytes_received += WIRE;
                    if s.packets % PACKETS_PER_AU == 0 {
                        st.frames_completed += 1;
                    }
                }
            }
            d.on_stats(&st);
            let probing = step.as_ref().is_some_and(|s| ms < s.report_at_ms);
            d.on_probe_active(probing, 25, at(ms));
            if let Some(s) = step.as_ref() {
                if ms >= s.report_at_ms {
                    let interval = ms.min(s.last_filler_ms).saturating_sub(s.first_ms).max(1);
                    d.on_probe_result(
                        ProbeReport {
                            delivered_bytes: s.packets * WIRE,
                            delivered_packets: s.packets,
                            window_ms: interval as u32,
                            host_duration_ms: s.duration_ms as u32,
                            client_interval_ms: interval as u32,
                            client_interval_us: (interval as u32) * 1_000,
                            host_bytes_sent: u64::from(s.target_kbps) * s.duration_ms / 8,
                            wire_packets_sent: s.packets as u32,
                            send_dropped: 0,
                        },
                        at(ms),
                    );
                }
            }
            for action in d.tick(at(ms)).actions {
                match action {
                    Action::Probe {
                        target_kbps,
                        duration_ms,
                        ramp,
                    } => {
                        assert!(ramp, "a bring-up step must be marked as one");
                        asked.push((ms, target_kbps));
                        completed_when_asked.push(st.frames_completed);
                        step = Some(Step {
                            target_kbps,
                            duration_ms: u64::from(duration_ms),
                            last_filler_ms: ms + u64::from(duration_ms),
                            report_at_ms: ms + u64::from(duration_ms) + REPORT_LAG_MS,
                            packets: 0,
                            first_ms: 0,
                        });
                        if asked.len() == 2 {
                            video_from_ms = ms + u64::from(duration_ms) + 60;
                        }
                    }
                    Action::SetBitrate(kbps) => opened_at = Some((ms, kbps)),
                    Action::LinkRate(kbps) => link_rate = Some(kbps),
                    _ => {}
                }
            }
            // Picture, which is the only thing that reaches `on_au`.
            if ms >= video_from_ms && ms % 16 == 0 {
                first_au_ms = first_au_ms.min(ms);
                d.on_au(false);
            }
        }

        assert!(
            asked.len() >= 2,
            "the ramp took {} step(s): {asked:?}",
            asked.len()
        );
        assert_eq!(asked[1].1, asked[0].1 * 2, "each step doubles the rate");
        let first = step_report_time(&asked, 0);
        assert!(
            asked[1].0 > first,
            "step two went out at {} ms, before step one's report at {first} ms",
            asked[1].0
        );
        assert!(
            completed_when_asked[1] > 0,
            "the filler must have completed AUs by then, or this pins nothing"
        );
        let (opened_ms, opened_kbps) = opened_at.expect("the ramp opens the session");
        assert!(
            opened_kbps > 0 && opened_kbps != 20_000,
            "the session opens at what was measured, not the negotiated rate"
        );
        assert!(
            asked.iter().all(|&(ms, _)| ms <= first_au_ms),
            "no step may go out once picture has started: {asked:?}"
        );
        assert!(
            opened_ms <= first_au_ms + 2,
            "the ramp's verdict is spent at once: opened at {opened_ms} ms, first \
             picture at {first_au_ms} ms"
        );
        assert!(
            link_rate.is_some_and(|k| k >= asked[0].1),
            "the ramp's proof goes to the host as a link rate: {link_rate:?}"
        );
    }

    /// A session that runs no probe, for the link and shape tests below.
    fn quiet_driver(at: Instant) -> Driver {
        Driver::new(
            DriverConfig {
                start_kbps: 20_000,
                ceiling_cap_kbps: None,
                stream_cap_kbps: 200_000,
                refresh_hz: 60,
                codec: crate::quic::CODEC_HEVC,
                bit_depth: 8,
                chroma_format: crate::quic::CHROMA_IDC_420,
                audio_reserved_kbps: 0,
                marks_repeats: true,
                probe: false,
                probe_target_kbps: None,
                ramp: false,
                probe_only: false,
                pin_kbps: None,
            },
            at,
        )
    }

    /// `windows` report windows a millisecond at a time: 20 Mbit/s of video, an AU every
    /// 16 ms, and `loss` (`[head, mid, tail]`) added once per window. Every action, in order.
    fn drive(d: &mut Driver, at: Instant, windows: u64, loss: [u64; 3]) -> Vec<Action> {
        let mut st = Stats::default();
        let mut out = Vec::new();
        for ms in 0..windows * 760 {
            st.packets_received += 2;
            st.bytes_received += 2_500;
            if ms % 750 == 0 {
                d.on_loss_positions(crate::packet::LossPositions {
                    head: loss[0] as u32,
                    mid: loss[1] as u32,
                    tail: loss[2] as u32,
                });
            }
            d.on_stats(&st);
            if ms % 16 == 0 {
                st.frames_completed += 1;
                d.on_au(false);
            }
            out.extend(d.tick(at + std::time::Duration::from_millis(ms)).actions);
        }
        out
    }

    /// `L` goes out on the first tick, before anything is measured: both ports' speed when
    /// both ends are wired, the floor when they are not. It does not repeat.
    #[test]
    fn the_link_rate_goes_out_first_from_the_ports() {
        use crate::transport::IFACE_KIND_ETHERNET as ETH;
        let at = Instant::now();
        let mut d = quiet_driver(at);
        let f = |kind, mbps| crate::quic::LinkFacts { kind, mbps };
        d.set_ports(f(ETH, 2_500), f(ETH, 1_000));
        let told: Vec<u32> = drive(&mut d, at, 2, [0; 3])
            .into_iter()
            .filter_map(|a| match a {
                Action::LinkRate(k) => Some(k),
                _ => None,
            })
            .collect();
        assert_eq!(told, [1_000_000]);
        assert_eq!(d.link(), (1_000_000, LinkSource::Ports));

        let mut d = quiet_driver(at);
        let first = drive(&mut d, at, 1, [0; 3]);
        assert!(first.contains(&Action::LinkRate(LINK_FLOOR_KBPS)));
    }

    /// Two windows running that lose the frames' heads turn the wake shape on; a window that
    /// loses the tail as much does not.
    #[test]
    fn head_loss_two_windows_running_wakes_the_shape() {
        let at = Instant::now();
        let mut d = quiet_driver(at);
        let wake = Action::Shape(Shape::Wake as u8);
        assert!(!drive(&mut d, at, 3, [6, 1, 3]).contains(&wake));
        let mut d = quiet_driver(at);
        let acts = drive(&mut d, at, 3, [6, 0, 0]);
        assert!(acts.contains(&wake), "{acts:?}");
        assert!(d.wake());
    }

    /// Loss at the frames' tails, well past the rest, while every frame decodes takes `L`
    /// down a notch every two windows, before a single frame is lost; the bitrate is not
    /// touched. Random loss, which reaches the tails too, leaves `L` alone.
    #[test]
    fn tail_loss_without_a_lost_frame_lowers_the_link_rate() {
        use crate::transport::IFACE_KIND_ETHERNET as ETH;
        let at = Instant::now();
        let run = |loss| {
            let mut d = quiet_driver(at);
            let f = |kind, mbps| crate::quic::LinkFacts { kind, mbps };
            d.set_ports(f(ETH, 1_000), f(ETH, 1_000));
            let acts = drive(&mut d, at, 8, loss);
            let told: Vec<u32> = acts
                .iter()
                .filter_map(|a| match a {
                    Action::LinkRate(k) => Some(*k),
                    _ => None,
                })
                .collect();
            (d, acts, told)
        };
        assert_eq!(run([2, 4, 6]).2, [1_000_000], "random loss");
        let (d, acts, told) = run([0, 0, 6]);
        assert_eq!(told[..3], [1_000_000, 875_000, 765_625]);
        assert_eq!(d.link().1, LinkSource::Measured);
        let cut = acts
            .iter()
            .any(|a| matches!(a, Action::SetBitrate(k) if *k < 20_000));
        assert!(!cut, "no cut");
        assert_eq!(
            d.target_kbps(),
            20_000,
            "the rate is the controller's own business"
        );
    }

    /// Tail loss that kills a frame is the controller's cut, not a calm window: the
    /// forty calm windows that give a notch back start over, and so does the run toward
    /// the next mark.
    #[test]
    fn tail_loss_with_a_dead_frame_is_not_calm() {
        let at = Instant::now();
        let mut d = quiet_driver(at);
        let mut acts = Vec::new();
        let tails = WindowSample {
            tail: 6,
            ..WindowSample::at(at)
        };
        let dead = WindowSample {
            dropped: 1,
            ..tails
        };
        let calm = WindowSample::at(at);
        d.after_window(&tails, &mut acts);
        d.after_window(&tails, &mut acts);
        let notched = d.link().0;
        assert!(notched < LINK_FLOOR_KBPS);
        for _ in 0..link::NOTCH_CALM_WINDOWS - 1 {
            d.after_window(&calm, &mut acts);
        }
        d.after_window(&tails, &mut acts);
        d.after_window(&dead, &mut acts);
        d.after_window(&tails, &mut acts);
        assert_eq!(d.link().0, notched, "the run starts over");
        d.after_window(&calm, &mut acts);
        assert_eq!(d.link().0, notched, "the calm count starts over");
        for _ in 1..link::NOTCH_CALM_WINDOWS {
            d.after_window(&calm, &mut acts);
        }
        assert_eq!(d.link().0, LINK_FLOOR_KBPS, "forty calm windows after it");
    }

    /// When step `i`'s report reached the client, in the test above.
    fn step_report_time(asked: &[(u64, u32)], i: usize) -> u64 {
        asked[i].0 + 25 + 5
    }

    /// Video that arrives after the startup burst is what the window
    /// measures.
    ///
    /// The embedder mirrors a finished probe's state for as long as it
    /// stands, so the pump hands the driver the same `ProbeResult` on every
    /// iteration — thousands of times a second. Only the first is the
    /// measurement: treating the rest as fresh would re-base the byte anchor
    /// each time, every window after the burst would read as nothing
    /// delivered, and the session would never climb again.
    #[test]
    fn delivery_after_the_burst_is_what_the_window_reports() {
        // 1 250 wire bytes a millisecond is 10 Mbps exactly over any window.
        const BYTES_PER_MS: u64 = 1_250;
        let base = Instant::now();
        let at = |ms: u64| base + std::time::Duration::from_millis(ms);
        let mut d = Driver::new(
            DriverConfig {
                start_kbps: 20_000,
                ceiling_cap_kbps: None,
                stream_cap_kbps: 200_000,
                refresh_hz: 60,
                codec: crate::quic::CODEC_HEVC,
                bit_depth: 8,
                chroma_format: crate::quic::CHROMA_IDC_420,
                audio_reserved_kbps: 0,
                marks_repeats: true,
                probe: true,
                probe_target_kbps: Some(400_000),
                ramp: false,
                probe_only: false,
                pin_kbps: None,
            },
            base,
        );
        let mut st = Stats::default();
        let deliver = |st: &mut Stats, ms: u64| {
            st.bytes_received += BYTES_PER_MS;
            st.packets_received += 1;
            if ms % 16 == 0 {
                st.frames_completed += 1;
            }
        };
        // Two seconds of video, then the burst fires.
        let mut fired = None;
        for ms in 0..=2_000 {
            deliver(&mut st, ms);
            d.on_stats(&st);
            if ms % 16 == 0 {
                d.on_au(false);
            }
            for a in d.tick(at(ms)).actions {
                if let Action::Probe { duration_ms, .. } = a {
                    fired = Some((ms, duration_ms));
                }
            }
        }
        let (fired_ms, burst_ms) = fired.expect("the startup probe fires once video flows");
        // The burst: filler in the counters, never in the decoder.
        for ms in fired_ms + 1..=fired_ms + u64::from(burst_ms) {
            deliver(&mut st, ms);
            st.bytes_received += 40_000;
            st.probe_bytes_received += 40_000;
            d.on_stats(&st);
            d.on_probe_active(true, burst_ms, at(ms));
            d.tick(at(ms));
        }
        // The host's report, and then the same report for as long as the
        // embedder's probe state stands.
        let done_ms = fired_ms + u64::from(burst_ms) + 1;
        let report = ProbeReport {
            delivered_bytes: 32_000_000,
            window_ms: burst_ms,
            host_duration_ms: burst_ms,
            client_interval_ms: burst_ms,
            client_interval_us: (burst_ms) * 1_000,
            ..ProbeReport::default()
        };
        let mut windows = Vec::new();
        for ms in done_ms..done_ms + 2_000 {
            deliver(&mut st, ms);
            d.on_stats(&st);
            d.on_probe_active(false, burst_ms, at(ms));
            d.on_probe_result(report, at(ms));
            if ms % 16 == 0 {
                d.on_au(false);
            }
            let tick = d.tick(at(ms));
            if let Some(w) = tick.window {
                windows.push((w.discarded, w.sample.actual_kbps));
            }
        }
        assert!(
            windows.len() >= 2,
            "two seconds must close at least two windows, closed {}",
            windows.len()
        );
        assert!(
            windows[0].0,
            "the window the burst's tail landed in is discarded"
        );
        let (discarded, actual_kbps) = windows[1];
        assert!(!discarded, "the window after the tail is the link's own");
        assert_eq!(
            actual_kbps, 10_000,
            "the window must report the 10 Mbps the stats delivered, not a byte anchor \
             re-based under it"
        );
    }

    /// Bound cuts an absurd probe ceiling and must not trim a session anyone runs.
    #[test]
    fn the_stream_bound_cuts_the_absurd_and_spares_the_ordinary() {
        use crate::quic::{CHROMA_IDC_420, CHROMA_IDC_444, CODEC_H264, CODEC_HEVC};

        // 1440p120 HEVC Main10 4:2:0: bound must sit under the ~460 Mbps decode knee.
        let field = stream_ceiling_kbps(2560, 1440, 120, CODEC_HEVC, 10, CHROMA_IDC_420);
        assert!(
            field < 657_000,
            "the bound must actually bind on the field case, got {field}"
        );
        assert!(
            field < 460_000,
            "and land under the decode knee this session found, got {field}"
        );

        // 1080p60 HEVC 8-bit: 80–100 Mbps sessions must keep headroom.
        let ordinary = stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 8, CHROMA_IDC_420);
        assert!(
            ordinary >= 90_000,
            "an ordinary 1080p60 session must keep its headroom, got {ordinary}"
        );

        // H.264, 10-bit, and 4:4:4 are each allowed more.
        assert!(
            stream_ceiling_kbps(1920, 1080, 60, CODEC_H264, 8, CHROMA_IDC_420) > ordinary,
            "H.264 is allowed more than HEVC"
        );
        assert!(
            stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 10, CHROMA_IDC_420) > ordinary,
            "10-bit is allowed more than 8-bit"
        );
        assert!(
            stream_ceiling_kbps(1920, 1080, 60, CODEC_HEVC, 8, CHROMA_IDC_444) > ordinary,
            "4:4:4 is allowed more than 4:2:0"
        );
        // Degenerate mode must not bound at zero.
        assert_eq!(
            stream_ceiling_kbps(0, 0, 0, CODEC_HEVC, 8, CHROMA_IDC_420),
            u32::MAX
        );
    }
}
