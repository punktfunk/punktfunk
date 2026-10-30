//! [`Driver`] as an embedder runs it: the native pump is one, the browser client another.
//! The embedder owns the sockets; this owns the order the driver hears things in.
//!
//! 1. [`Feed::new`] once the Welcome is in: the session's [`Facts`] and both ends' ports.
//! 2. Each completed picture AU, never probe filler: [`Driver::on_au`] on [`Feed::driver`],
//!    then its capture → received delay as [`Driver::on_owd`] once a clock offset is known.
//! 3. Each receive-loop iteration: [`Feed::tick`] with what was read since the last one. A
//!    counter is the session's running total; a list holds each item once.
//! 4. Apply the returned actions in order. A [`Action::SetBitrate`] or [`Action::Probe`]
//!    that is not sent goes back at once as [`Driver::on_request_dropped`] or
//!    [`Driver::on_probe_dropped`]; [`Action::LinkRate`] reads [`Driver::link`].
//! 5. If the tick closed a window, [`Feed::record`] it after the actions, with the last
//!    [`Action::SetBitrate`] rate among them.
//!
//! A jump-to-live goes in as [`Driver::on_flush`]. Past [`Feed::new`] nothing here reads the
//! environment, a socket or a runtime.

#[cfg(doc)]
use super::Action;
use super::{ClosedWindow, Driver, DriverConfig, ProbeReport, Tick, WindowRecord};
use crate::quic::{AckReason, HostLink, HostTiming, LinkFacts};
use std::time::Instant;

/// What the session negotiated. None of it changes until the session ends.
pub struct Facts {
    /// The rate the player asked for, kbps. `0` = Automatic, the only case the controller runs.
    pub bitrate_kbps: u32,
    /// The rate the host configured: its Welcome echo.
    pub resolved_bitrate_kbps: u32,
    pub codec: u8,
    pub bit_depth: u8,
    pub chroma_format: u8,
    /// Negotiated refresh, not a pending request.
    pub refresh_hz: u32,
    /// The mode and codec ceiling at the negotiated geometry; PyroWave's pin.
    pub stream_cap_kbps: u32,
    /// PyroWave: [`crate::pyrowave::BPP_FLOOR`] at the negotiated geometry.
    pub pyrowave_floor_kbps: Option<u32>,
    /// Audio's wire reservation, spent whether video flows or not.
    pub audio_reserved_kbps: u32,
    /// The host marks idle-keepalive repeats ([`crate::quic::HOST_CAP2_REPEAT_MARK`]).
    pub marks_repeats: bool,
    /// The host serves probes during bring-up ([`crate::quic::HOST_CAP2_RAMP`]).
    pub serves_ramp: bool,
    /// A diagnostic session: no video ever comes.
    pub probe_only: bool,
}

/// What the embedder read since the last tick.
#[derive(Default)]
pub struct Inputs {
    /// Host clock minus this one, ns. `0` = not measured yet, and no delay is read.
    pub clock_offset_ns: i64,
    /// The newest host pipeline gap, ms.
    pub gap_ms: Option<u32>,
    /// [`crate::session::Session::stats`].
    pub stats: crate::stats::Stats,
    /// [`crate::session::Session::take_loss_positions`].
    pub loss_positions: crate::packet::LossPositions,
    /// The host's port from its newest `StreamConfig`, this device's from the dial.
    pub host_link: HostLink,
    pub client_link: LinkFacts,
    /// The receiver's own drops since the session opened, when sampled this iteration.
    pub sock_drops: Option<u64>,
    /// [`crate::session::Session::take_shard_delays`]: `(pts_ns, raw delay ns)`.
    pub shard_delays: Vec<(u64, i64)>,
    /// A burst is in flight, how long it was asked to run, and the host's report once done.
    pub probe_active: bool,
    pub probe_duration_ms: u32,
    pub probe_report: Option<ProbeReport>,
    /// The newest accepted mode.
    pub mode: Option<crate::config::Mode>,
    /// Every bitrate ack, in arrival order.
    pub acks: Vec<(u32, Option<AckReason>)>,
    /// Client decode and host encode stage totals: `(sum µs, samples)`.
    pub decode_lat: (u64, u32),
    pub encode_lat: (u64, u32),
    /// The host's per-AU timings, in arrival order.
    pub host_timings: Vec<HostTiming>,
    /// Keyframe asks sent: decode recovery's, and those no lost frame stood behind.
    pub keyframe_asks: u32,
    pub starved_keyframe_asks: u32,
}

/// One session's [`Driver`] and the state it is fed through.
pub struct Feed {
    /// Per-frame events go straight in; see the module doc.
    pub driver: Driver,
    /// Window records count from here, and the delay frame reads it.
    session_start: Instant,
    /// The host's facts the driver last had: its `StreamConfig` moves them.
    host_link: HostLink,
}

impl Feed {
    /// The session's driver, with the three environment overrides read once. Automatic is a
    /// session with no player rate and a host that echoed one: PyroWave's adapts between its
    /// floor and the host's pin. An explicit-rate PyroWave session runs at the pin, which the
    /// bring-up ramp may lower once; no controller runs for it.
    pub fn new(facts: Facts, host: HostLink, client: LinkFacts, now: Instant) -> Self {
        let automatic = facts.bitrate_kbps == 0;
        let pyrowave = facts.codec == crate::quic::CODEC_PYROWAVE;
        let pin_kbps = (pyrowave && !automatic)
            .then_some(facts.resolved_bitrate_kbps)
            .filter(|&pin| pin > 0);
        let mut driver = Driver::new(
            DriverConfig {
                start_kbps: if automatic {
                    facts.resolved_bitrate_kbps
                } else {
                    0
                },
                ceiling_cap_kbps: env_u32("PUNKTFUNK_ABR_MAX_MBPS")
                    .map(|m| m.saturating_mul(1_000)),
                stream_cap_kbps: facts.stream_cap_kbps,
                refresh_hz: facts.refresh_hz,
                codec: facts.codec,
                bit_depth: facts.bit_depth,
                chroma_format: facts.chroma_format,
                audio_reserved_kbps: facts.audio_reserved_kbps,
                marks_repeats: facts.marks_repeats,
                probe: std::env::var("PUNKTFUNK_ABR_PROBE").map_or(true, |v| v != "0"),
                probe_target_kbps: env_u32("PUNKTFUNK_ABR_PROBE_KBPS"),
                ramp: facts.serves_ramp,
                probe_only: facts.probe_only,
                pin_kbps,
                floor_kbps: facts.pyrowave_floor_kbps.filter(|_| automatic),
            },
            now,
        );
        driver.set_ports(host.facts(), client);
        Feed {
            driver,
            session_start: now,
            host_link: HostLink::default(),
        }
    }

    /// Everything the driver hears before this iteration's tick, in the order it hears it,
    /// then the tick. The delay frame goes first, so a step moves what was read before it;
    /// host timings come after the shard delays they settle.
    pub fn tick(&mut self, inputs: Inputs) -> Tick {
        let d = &mut self.driver;
        // Where the monotonic clock stops while the device sleeps, waking reads as a forward
        // step of the sleep's length.
        let mono_ns = self.session_start.elapsed().as_nanos() as i128;
        d.on_clock(
            crate::quic::wall_clock_ns() as i128 - mono_ns + i128::from(inputs.clock_offset_ns),
        );
        // Before the tick, so the window the rebuild corrupted is the one dropped.
        if let Some(gap_ms) = inputs.gap_ms {
            d.on_pipeline_gap(gap_ms);
        }
        d.on_stats(&inputs.stats);
        d.on_loss_positions(inputs.loss_positions);
        if inputs.host_link != self.host_link {
            self.host_link = inputs.host_link;
            d.set_ports(inputs.host_link.facts(), inputs.client_link);
        }
        if let Some(total) = inputs.sock_drops {
            d.on_sock_drops(total);
        }
        // Same offset and sign test as a completed AU's. Without an offset there is no delay
        // to read, and the samples are dropped.
        for (pts_ns, raw_ns) in inputs.shard_delays {
            let owd_ns = i128::from(raw_ns) + i128::from(inputs.clock_offset_ns);
            if inputs.clock_offset_ns != 0 && owd_ns > 0 {
                d.on_shard_owd(pts_ns, owd_ns);
            }
        }
        d.on_probe_active(
            inputs.probe_active,
            inputs.probe_duration_ms,
            Instant::now(),
        );
        if let Some(r) = inputs.probe_report {
            d.on_probe_result(r, Instant::now());
        }
        if let Some(m) = inputs.mode {
            d.on_mode_switch(m.width, m.height, m.refresh_hz);
        }
        for (acked, why) in inputs.acks {
            d.on_ack(acked, why);
        }
        d.on_decode_latency(inputs.decode_lat.0, inputs.decode_lat.1);
        d.on_encode_latency(inputs.encode_lat.0, inputs.encode_lat.1);
        for t in &inputs.host_timings {
            d.on_host_timing(t);
        }
        d.on_keyframe_asks(inputs.keyframe_asks);
        d.on_starved_keyframe_asks(inputs.starved_keyframe_asks);
        d.tick(Instant::now())
    }

    /// The window a tick closed, as a trajectory writes it down. Read once the tick's actions
    /// are applied: a send that never left moves the driver.
    pub fn record(&self, window: &ClosedWindow, request_kbps: Option<u32>) -> WindowRecord {
        WindowRecord {
            t_ms: window
                .sample
                .now
                .duration_since(self.session_start)
                .as_millis() as u64,
            // The rate the window ran at: the ask has not been acked yet.
            rate_kbps: self.driver.target_kbps(),
            request_kbps,
            sample: window.sample,
            discarded: window.discarded,
            reason: self.driver.reason(),
        }
    }
}

/// A positive `u32` from the environment. Unset, zero or garbage is `None`.
fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|&v| v > 0)
}
