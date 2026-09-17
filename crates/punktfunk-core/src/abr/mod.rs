//! Adaptive bitrate: the controller behind the Automatic bitrate setting.
//!
//! [`Driver`] is the whole of it: events in — a completed AU, a stats
//! snapshot, a latency sample, an ack — and actions out of [`Driver::tick`].
//! An embedder owns no policy, so two clients cannot drift apart. The pump
//! runs it on the 750 ms cadence it shares with [`crate::quic::LossReport`];
//! FEC absorbs short random loss, and the controller asks the host for a
//! different encoder rate via [`crate::quic::SetBitrate`] when congestion
//! persists.
//!
//! One module per concern: [`window`] assembles a report window, [`sample`]
//! is the closed window, [`verdict`] scores it, [`cap`] holds the learned
//! bounds, [`growth`] the climb law, [`probe`] the startup capacity burst,
//! and [`controller`] the state they all move. [`budget`] is the wire
//! arithmetic the host shares. `sim/` drives this same
//! `Driver` against modelled links and pins every decision in a checked-in
//! baseline.

/// The link simulator and the checked-in baseline (`abr/sim/`).
#[cfg(test)]
mod sim;

pub mod budget;
mod cap;
mod controller;
mod growth;
#[cfg(test)]
mod harness;
pub mod metrics;
mod probe;
mod sample;
mod verdict;
mod window;

use controller::BitrateController;
pub use probe::ProbeReport;
pub use sample::{WindowActivity, WindowSample, WINDOW};
pub use verdict::Reason;

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
    /// `PUNKTFUNK_ABR_PROBE_KBPS`. `None` = twice the stream-shape cap.
    pub probe_target_kbps: Option<u32>,
}

/// What the embedder has to do for the controller. Everything else it does
/// with a window — jump-to-live, standing latency, the frame hand-off — is
/// its own business.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Shard loss this window, ppm: the host's adaptive-FEC input.
    Loss(u32),
    /// Session total packets received. The host escalates on a dead plane.
    Delivery(u64),
    /// Ask the host for a new encoder rate.
    SetBitrate(u32),
    /// Ask for a capacity burst beside the video.
    Probe { target_kbps: u32, duration_ms: u32 },
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
    /// A mode switch changes the geometry, not these.
    codec: u8,
    bit_depth: u8,
    chroma_format: u8,
    /// Raised between ticks (a probe ended, a burst was abandoned) and sent
    /// on the next one, microseconds later.
    pending: Vec<Action>,
    /// Acks since the last window closed.
    acks: Vec<u32>,
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
        Driver {
            abr,
            window: window::WindowAccumulator::new(cfg.audio_reserved_kbps, cfg.marks_repeats, now),
            // A pinned or explicit rate has nothing to measure for.
            probe: probe::CapacityProbe::new(
                cfg.probe && cfg.start_kbps > 0,
                cfg.probe_target_kbps,
                cfg.stream_cap_kbps,
                now,
            ),
            codec: cfg.codec,
            bit_depth: cfg.bit_depth,
            chroma_format: cfg.chroma_format,
            pending: Vec::new(),
            acks: Vec::new(),
        }
    }

    /// The session counters, once per embedder iteration.
    pub fn on_stats(&mut self, st: &crate::stats::Stats) {
        self.window.on_stats(st);
    }

    /// One completed access unit; `repeat` is the host's idle keepalive mark.
    pub fn on_au(&mut self, repeat: bool) {
        self.window.on_au(repeat);
    }

    /// Capture → received for one AU, ns.
    pub fn on_owd(&mut self, ns: i128) {
        self.window.on_owd(ns);
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
    /// the one that was running for it.
    pub fn on_ack(&mut self, kbps: u32) {
        self.acks.push(kbps);
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
        // Rebinds an already-learned ceiling downward for the new geometry.
        self.abr.set_stream_cap(stream_ceiling_kbps(
            width,
            height,
            refresh_hz,
            self.codec,
            self.bit_depth,
            self.chroma_format,
        ));
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
        if frames_completed == frames_at_start {
            self.pending.push(Action::Keyframe);
            tracing::warn!(
                "no frame survived the capacity probe — requested a keyframe to re-anchor"
            );
        }
    }

    /// The host's end-of-burst report.
    pub fn on_probe_result(&mut self, r: ProbeReport) {
        match self.probe.on_result(r) {
            probe::Measured::NotOurs => return,
            probe::Measured::Declined => {}
            probe::Measured::Ceiling(kbps) => self.set_ceiling(kbps),
        }
        // Skips video that landed under a suppressed report tick; `rebase`
        // already netted the filler out.
        self.window.rebase_bytes();
    }

    /// What the last judged window was, and so what named the last rate
    /// change. A cut nobody can attribute is a bug; this is what an overlay
    /// shows and a field report quotes.
    pub fn reason(&self) -> Reason {
        self.abr.last_reason()
    }

    /// The rate the session is running at — the host's latest ack, or the
    /// Welcome rate before one. What a window is judged against.
    pub fn target_kbps(&self) -> u32 {
        self.abr.current_kbps
    }

    /// A measured link capacity. Never lowers the climb ceiling: a
    /// congested-moment measurement must not shrink what was negotiated.
    pub fn set_ceiling(&mut self, kbps: u32) {
        self.abr.set_ceiling(kbps);
    }

    /// This window describes something other than the link.
    pub fn discard_window(&mut self) {
        self.window.discard();
    }

    /// Everything the session owes right now. Called every embedder
    /// iteration; the report window closes inside it, on its own cadence.
    pub fn tick(&mut self, now: Instant) -> Tick {
        let mut actions = std::mem::take(&mut self.pending);
        if self.probe.expired(now) {
            actions.push(Action::AbandonProbe);
        }
        if let Some((target_kbps, duration_ms)) =
            self.probe.poll(now, self.window.stats().frames_completed)
        {
            actions.push(Action::Probe {
                target_kbps,
                duration_ms,
            });
        }
        if !self.window.due(now, self.probe.active()) {
            return Tick {
                actions,
                window: None,
            };
        }
        let closed = self.window.close(now);
        // The host's answers to what this window's predecessors asked for,
        // learned before the controller judges this one.
        for kbps in std::mem::take(&mut self.acks) {
            self.abr.on_ack(kbps);
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
            actions.push(Action::Loss(w.loss_ppm));
            // Delivery rides the loss report so `loss_ppm = 0` is readable:
            // flawless, or delivering nothing.
            if let Some(packets_received) = closed.delivery {
                actions.push(Action::Delivery(packets_received));
            }
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
        }
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
        };
        let mut windows = Vec::new();
        for ms in done_ms..done_ms + 2_000 {
            deliver(&mut st, ms);
            d.on_stats(&st);
            d.on_probe_active(false, burst_ms, at(ms));
            d.on_probe_result(report);
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
