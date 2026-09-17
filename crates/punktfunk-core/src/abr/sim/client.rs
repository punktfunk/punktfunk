//! Client model: FEC repair, keyframe asks, decode time, and the session
//! counters the real [`Driver`] assembles its window from.
//!
//! Only the wire is a model. The window, the verdict, the controller and the
//! capacity probe are the shipped code, driven the way
//! `client/pump/data.rs` drives them: counters in, actions out.

use super::host::{Frame, FrameShape, SHARD_WIRE_OVERHEAD};
use super::link::LossDraw;
use super::Rng;
use crate::abr::{Driver, DriverConfig, ProbeReport};
use crate::client::FLUSH_COOLDOWN;
use crate::stats::Stats;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Jump-to-live's thresholds (`client/frame_channel.rs`, private there):
/// delay past `FLUSH_LATENCY` held for `FLUSH_AFTER`, or `QUEUE_HIGH` frames
/// of decode backlog held for `STANDING_TIME`.
const FLUSH_LATENCY_MS: u64 = 400;
const FLUSH_AFTER_MS: u64 = 250;
const QUEUE_HIGH: u32 = 6;
const STANDING_MS: u64 = 250;
/// The webOS client's recovery throttle: one ask per 100 ms until a keyframe
/// lands.
const KEYFRAME_ASK_MS: u64 = 100;
/// Header plus shard, the plaintext the reassembler counts per probe packet —
/// not the sealed datagram. The field's `delivered_kbps` is in these bytes.
const PROBE_PACKET_BYTES: u64 = 40 + 1408;
/// The probe's own id in the link queue. Filler never reaches the decoder and
/// never counts toward `actual_kbps` (`wire_bytes` nets it out).
pub(super) const PROBE_FRAME: u32 = u32::MAX;
/// Stall the client reports with a host pipeline rebuild. ABR only logs it;
/// what the rebuild costs is the discarded window and the lost reference.
const REBUILD_GAP_MS: u32 = 400;

/// Decode latency: a floor plus a rise past the rate the decoder is happy at.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DecodeCfg {
    pub base_us: u32,
    pub jitter_us: u32,
    pub knee_kbps: u32,
    pub us_per_mbps: u32,
}

#[derive(Clone, Debug)]
pub(super) struct ClientCfg {
    pub start_kbps: u32,
    pub refresh_hz: u32,
    pub stream_cap_kbps: u32,
    pub audio_kbps: u32,
    pub shard_payload: u16,
    /// Older host: repeats are not flagged, so no window is ever idle.
    pub marks_repeats: bool,
    pub decode: DecodeCfg,
    /// Run the startup capacity probe. `false` replays a host that declined
    /// it, or a scenario that injects the ceiling instead.
    pub probe: bool,
    /// `PUNKTFUNK_ABR_PROBE_KBPS`. `None` = `probe_target_kbps(stream_cap)`.
    pub probe_target_kbps: Option<u32>,
    /// Ceiling injected directly, for a scenario that replays a host which
    /// paused video for the burst. The window it lands in is discarded, as
    /// the probe tail is.
    pub ceiling_at: Option<(u64, u32)>,
    /// The host rebuilds its pipeline here: the window in flight describes the
    /// gap, and the client is left without a reference to decode against.
    pub rebuild_at_ms: Option<u64>,
    /// `false` = an explicit bitrate, so no controller.
    pub automatic: bool,
}

impl Default for ClientCfg {
    fn default() -> Self {
        ClientCfg {
            start_kbps: 20_000,
            refresh_hz: 60,
            stream_cap_kbps: u32::MAX,
            audio_kbps: 256,
            shard_payload: 1408,
            marks_repeats: true,
            decode: DecodeCfg::default(),
            probe: true,
            probe_target_kbps: None,
            ceiling_at: None,
            rebuild_at_ms: None,
            automatic: true,
        }
    }
}

/// What the client sends the host in one tick. The driver's own
/// [`crate::abr::Action`]s become these; `unrecovered` is what the client
/// model knows and the real wire carries as a keyframe ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    SetBitrate(u32),
    Keyframe,
    Loss { ppm: u32, unrecovered: bool },
    Probe { target_kbps: u32, duration_ms: u32 },
}

/// One closed report window, kept for the metrics.
#[derive(Clone, Copy, Debug)]
pub(super) struct WindowRec {
    pub t_ms: u64,
    /// Rate the host has acked — what the session is actually running at.
    pub rate_kbps: u32,
    pub actual_kbps: u32,
    pub dropped: u64,
    /// Keyframe asks this window — the recovery signal.
    pub recovery_kf: u32,
    pub request_kbps: Option<u32>,
    pub cut_from_kbps: Option<u32>,
    pub discarded: bool,
    /// The encode down-driver's stand-down, sampled after the verdict.
    pub encode_disarmed: bool,
}

impl WindowRec {
    /// This window as [`crate::abr::metrics`] reads it.
    pub(super) fn metric(&self) -> crate::abr::metrics::MetricWindow {
        crate::abr::metrics::MetricWindow {
            t_ms: self.t_ms,
            rate_kbps: self.rate_kbps,
            request_kbps: self.request_kbps,
            dropped: self.dropped,
            discarded: self.discarded,
        }
    }
}

struct InFlight {
    id: u32,
    capture_ms: u64,
    remaining: u64,
    refused: u64,
    shape: FrameShape,
    encode_us: u32,
    repeat: bool,
    idr: bool,
    /// Scenario-injected unrecoverable frame.
    forced: bool,
}

pub(super) struct Client {
    cfg: ClientCfg,
    rng: Rng,
    /// The scenario's zero, so a millisecond can become an `Instant`.
    base: Instant,
    pub(super) abr: Driver,
    flight: VecDeque<InFlight>,
    lost_blocks: Vec<u32>,
    /// The session counters the driver differences its windows from.
    stats: Stats,
    /// Unrecoverable frames since the last window closed. The host's adaptive
    /// FEC is told; on the real wire the keyframe ask tells it.
    lost_frames: u64,
    /// Jump-to-live detectors and their shared cooldown.
    owd_over_since: Option<u64>,
    queue_over_since: Option<u64>,
    last_flush_ms: Option<u64>,
    decode_free_at_ms: u64,
    /// Keyframe throttle: asking until an IDR lands.
    awaiting_idr: bool,
    kf_next_ms: u64,
    force_loss_at_ms: Option<u64>,
    /// A burst is in flight, how long it lasts, and what its filler
    /// delivered while it was.
    probing: bool,
    probe_duration_ms: u32,
    probe_bytes: u64,
    probe_first_ms: u64,
    probe_last_ms: u64,
    /// The finished burst's report. The embedder's probe state keeps saying
    /// "done" until the next burst overwrites it, so the pump hands the same
    /// report over on every iteration — and so does this.
    probe_report: Option<ProbeReport>,
    pub(super) windows: Vec<WindowRec>,
    pub(super) owd_samples: Vec<u32>,
}

impl Client {
    pub(super) fn new(cfg: ClientCfg, seed: u64, base: Instant) -> Self {
        let abr = Driver::new(
            DriverConfig {
                start_kbps: if cfg.automatic { cfg.start_kbps } else { 0 },
                ceiling_cap_kbps: None,
                stream_cap_kbps: cfg.stream_cap_kbps,
                refresh_hz: cfg.refresh_hz,
                codec: crate::quic::CODEC_HEVC,
                bit_depth: 8,
                chroma_format: crate::quic::CHROMA_IDC_420,
                audio_reserved_kbps: cfg.audio_kbps,
                marks_repeats: cfg.marks_repeats,
                probe: cfg.probe,
                probe_target_kbps: cfg.probe_target_kbps,
            },
            base,
        );
        Client {
            rng: Rng::new(seed),
            base,
            abr,
            flight: VecDeque::new(),
            lost_blocks: Vec::new(),
            stats: Stats::default(),
            lost_frames: 0,
            owd_over_since: None,
            queue_over_since: None,
            last_flush_ms: None,
            decode_free_at_ms: 0,
            awaiting_idr: false,
            kf_next_ms: 0,
            force_loss_at_ms: None,
            probing: false,
            probe_duration_ms: 0,
            probe_bytes: 0,
            probe_first_ms: 0,
            probe_last_ms: 0,
            probe_report: None,
            windows: Vec::new(),
            owd_samples: Vec::new(),
            cfg,
        }
    }

    /// What the session is running at. A fixed-rate session has no
    /// controller, so its rate is the one it negotiated.
    pub(super) fn rate_kbps(&self) -> u32 {
        if self.cfg.automatic {
            self.abr.abr.current_kbps
        } else {
            self.cfg.start_kbps
        }
    }

    /// Make the next frame unrecoverable, whatever the link does.
    pub(super) fn inject_lost_frame(&mut self, at_ms: u64) {
        self.force_loss_at_ms = Some(at_ms);
    }

    /// A host `BitrateChanged`. The driver queues it to the window close, as
    /// the pump's ack queue does.
    pub(super) fn push_ack(&mut self, kbps: u32) {
        self.abr.on_ack(kbps);
    }

    /// A frame left the host: the client now knows what to wait for.
    pub(super) fn expect(&mut self, f: &Frame, now_ms: u64) {
        let forced = matches!(self.force_loss_at_ms, Some(t) if now_ms >= t);
        if forced {
            self.force_loss_at_ms = None;
        }
        self.flight.push_back(InFlight {
            id: f.id,
            capture_ms: f.capture_ms,
            remaining: f.wire_bytes,
            refused: 0,
            shape: f.shape,
            encode_us: f.encode_us,
            repeat: f.repeat,
            idr: f.idr,
            forced,
        });
    }

    /// Bytes the link's depth refused — shards that never left the host.
    /// `Some(shards)` when they were the frame's tail.
    pub(super) fn refuse(&mut self, frame: u32, bytes: u64) -> Option<u32> {
        let f = self.flight.iter_mut().find(|f| f.id == frame)?;
        f.refused += bytes;
        f.remaining = f.remaining.saturating_sub(bytes);
        (f.remaining == 0).then(|| f.shape.shards())
    }

    /// Bytes arrived. `Some(shards)` when this was the frame's tail and the
    /// link owes it a loss draw.
    pub(super) fn deliver(&mut self, frame: u32, bytes: u64) -> Option<u32> {
        let f = self.flight.iter_mut().find(|f| f.id == frame)?;
        f.remaining = f.remaining.saturating_sub(bytes);
        (f.remaining == 0).then(|| f.shape.shards())
    }

    /// Close one frame: repair what parity covers, count the rest.
    pub(super) fn complete(&mut self, frame: u32, draw: LossDraw, now_ms: u64) {
        let Some(pos) = self.flight.iter().position(|f| f.id == frame) else {
            return;
        };
        let f = self.flight.remove(pos).expect("position just found");
        let shards = f.shape.shards();
        let shard_wire = self.cfg.shard_payload as u64 + SHARD_WIRE_OVERHEAD;
        // The depth drops the frame's tail, and the tail on the wire is
        // parity: data-first order means a shallow overflow costs recovery
        // before it costs picture.
        let refused_shards = (f.refused.div_ceil(shard_wire) as u32).min(shards);
        self.lost_blocks.clear();
        self.lost_blocks.resize(f.shape.blocks as usize, 0);
        let mut lost = 0u32;
        let mark = |shape: &FrameShape, blocks: &mut [u32], idx: u32| {
            blocks[shape.block_of(idx) as usize] += 1;
        };
        for i in 0..refused_shards {
            mark(&f.shape, &mut self.lost_blocks, shards - 1 - i);
            lost += 1;
        }
        // Uniform loss spreads over the frame; a burst is one contiguous run.
        for i in 0..draw.random.min(shards) {
            let idx = (i as u64 * shards as u64 / draw.random.max(1) as u64) as u32;
            mark(&f.shape, &mut self.lost_blocks, idx.min(shards - 1));
            lost += 1;
        }
        for i in 0..draw.burst_len {
            let idx = (draw.burst_at + i).min(shards - 1);
            mark(&f.shape, &mut self.lost_blocks, idx);
            lost += 1;
        }
        let mut repaired = 0u32;
        let mut unrecoverable = f.forced;
        for (b, &lost_b) in self.lost_blocks.iter().enumerate() {
            if lost_b == 0 {
                continue;
            }
            if lost_b <= f.shape.parity_of(b as u32) {
                repaired += lost_b;
            } else {
                unrecoverable = true;
            }
        }
        let arrived = shards.saturating_sub(lost.min(shards));
        self.stats.packets_received += u64::from(arrived);
        self.stats.bytes_received += u64::from(arrived) * shard_wire;
        self.stats.fec_recovered_shards += u64::from(repaired);
        if unrecoverable {
            self.stats.frames_dropped += 1;
            self.lost_frames += 1;
            self.awaiting_idr = true;
            return;
        }
        self.stats.frames_completed += 1;
        self.abr.on_au(f.repeat && self.cfg.marks_repeats);
        if f.idr {
            self.awaiting_idr = false;
        }
        let owd_us = (now_ms.saturating_sub(f.capture_ms) * 1_000) as i64;
        self.abr.on_owd(i128::from(owd_us) * 1_000);
        self.owd_samples.push((owd_us / 1_000) as u32);
        self.abr.on_encode_latency(u64::from(f.encode_us), 1);
        let decode_us = self.decode_us();
        self.abr.on_decode_latency(u64::from(decode_us), 1);
        self.decode_free_at_ms =
            self.decode_free_at_ms.max(now_ms) + u64::from(decode_us).div_ceil(1_000);
        self.note_latency(owd_us / 1_000, now_ms);
    }

    fn decode_us(&mut self) -> u32 {
        let d = self.cfg.decode;
        let over = self.abr.abr.current_kbps.saturating_sub(d.knee_kbps) / 1_000;
        let jitter = if d.jitter_us == 0 {
            0
        } else {
            self.rng.below(u64::from(d.jitter_us) + 1) as u32
        };
        d.base_us + jitter + over * d.us_per_mbps
    }

    /// Jump-to-live, both halves: one-way delay past [`FLUSH_LATENCY_MS`] for
    /// [`FLUSH_AFTER_MS`], or a decode backlog at [`QUEUE_HIGH`] for
    /// [`STANDING_MS`]. A flush is ABR's severe `flushed`.
    fn note_latency(&mut self, owd_ms: i64, now_ms: u64) {
        if owd_ms > FLUSH_LATENCY_MS as i64 {
            self.owd_over_since.get_or_insert(now_ms);
        } else {
            self.owd_over_since = None;
        }
        let backlog = (self.decode_free_at_ms.saturating_sub(now_ms)
            * u64::from(self.cfg.refresh_hz.max(1))
            / 1_000) as u32;
        if backlog >= QUEUE_HIGH {
            self.queue_over_since.get_or_insert(now_ms);
        } else if backlog <= 2 {
            self.queue_over_since = None;
        }
        let over = |since: Option<u64>, ms: u64| since.is_some_and(|t| now_ms - t >= ms);
        let behind = over(self.owd_over_since, FLUSH_AFTER_MS)
            || (backlog >= QUEUE_HIGH && over(self.queue_over_since, STANDING_MS));
        let cooled = self
            .last_flush_ms
            .is_none_or(|t| now_ms - t >= FLUSH_COOLDOWN.as_millis() as u64);
        if behind && cooled {
            self.owd_over_since = None;
            self.queue_over_since = None;
            self.last_flush_ms = Some(now_ms);
            self.decode_free_at_ms = now_ms;
            self.abr.on_flush();
            self.awaiting_idr = true;
        }
    }

    /// Probe filler arrived. It never reaches the decoder and `wire_bytes`
    /// nets it out of the window, so it teaches the controller nothing
    /// directly — only the damage it does to video beside it.
    pub(super) fn deliver_probe(&mut self, bytes: u64, now_ms: u64) {
        if self.probe_first_ms == 0 {
            self.probe_first_ms = now_ms;
        }
        self.probe_last_ms = now_ms;
        self.probe_bytes += bytes;
        self.stats.bytes_received += bytes;
        self.stats.probe_bytes_received += bytes;
    }

    /// The host's `ProbeResult` landed: the burst's trailing edge, then the
    /// measurement, in the order one pump iteration sees them.
    pub(super) fn on_probe_result(&mut self, now_ms: u64, host_duration_ms: u32) {
        self.probing = false;
        let packets = self.probe_bytes / (self.cfg.shard_payload as u64 + SHARD_WIRE_OVERHEAD);
        let delivered = packets * PROBE_PACKET_BYTES;
        // Client receive interval: first to last filler arrival. Under two
        // packets there is no interval and the host's window stands.
        let client_interval_ms = if packets >= 2 && self.probe_last_ms > self.probe_first_ms {
            (self.probe_last_ms - self.probe_first_ms) as u32
        } else {
            0
        };
        let report = ProbeReport {
            delivered_bytes: delivered,
            window_ms: if client_interval_ms > 0 {
                client_interval_ms
            } else {
                host_duration_ms
            },
            host_duration_ms,
            client_interval_ms,
        };
        let now = self.base + Duration::from_millis(now_ms);
        self.abr.on_probe_active(false, self.probe_duration_ms, now);
        self.abr.on_probe_result(report);
        self.probe_report = Some(report);
    }

    /// One millisecond of client: the host rebuild and the keyframe throttle,
    /// then the driver, which closes the report window when it comes due.
    pub(super) fn tick(&mut self, now_ms: u64, base: Instant, out: &mut Vec<Action>) {
        let now = base + Duration::from_millis(now_ms);
        if self.cfg.rebuild_at_ms.is_some_and(|at| now_ms >= at) {
            self.cfg.rebuild_at_ms = None;
            self.abr.on_pipeline_gap(REBUILD_GAP_MS);
            // The rebuilt encoder opens on a reference this client does not
            // have, so it asks until a recovery point lands.
            self.awaiting_idr = true;
        }
        if self.awaiting_idr && now_ms >= self.kf_next_ms {
            self.kf_next_ms = now_ms + KEYFRAME_ASK_MS;
            self.abr.on_keyframe_asks(1);
            out.push(Action::Keyframe);
        }
        if let Some((at, kbps)) = self.cfg.ceiling_at {
            if now_ms >= at {
                self.cfg.ceiling_at = None;
                self.abr.set_ceiling(kbps);
                // The burst's tail is still draining into this window.
                self.abr.discard_window();
            }
        }
        self.abr.on_stats(&self.stats);
        self.abr
            .on_probe_active(self.probing, self.probe_duration_ms, now);
        // The pump re-presents a finished burst's report for as long as the
        // probe state stands. Only the first is the measurement.
        if let Some(r) = self.probe_report {
            self.abr.on_probe_result(r);
        }
        let unrecovered = self.lost_frames > 0;
        let tick = self.abr.tick(now);
        let mut request = None;
        for action in tick.actions {
            match action {
                crate::abr::Action::Loss(ppm) => out.push(Action::Loss { ppm, unrecovered }),
                crate::abr::Action::SetBitrate(kbps) => {
                    request = Some(kbps);
                    out.push(Action::SetBitrate(kbps));
                }
                crate::abr::Action::Probe {
                    target_kbps,
                    duration_ms,
                } => {
                    self.probing = true;
                    self.probe_duration_ms = duration_ms;
                    // A new burst overwrites the old state, report included.
                    self.probe_report = None;
                    self.probe_bytes = 0;
                    self.probe_first_ms = 0;
                    self.probe_last_ms = 0;
                    out.push(Action::Probe {
                        target_kbps,
                        duration_ms,
                    });
                }
                // Counted where the control task counts every ask.
                crate::abr::Action::Keyframe => {
                    self.abr.on_keyframe_asks(1);
                    out.push(Action::Keyframe);
                }
                crate::abr::Action::Delivery(_) | crate::abr::Action::AbandonProbe => {}
            }
        }
        let Some(w) = tick.window else { return };
        self.lost_frames = 0;
        let was = self.rate_kbps();
        self.windows.push(WindowRec {
            t_ms: now_ms,
            rate_kbps: was,
            actual_kbps: w.sample.actual_kbps,
            dropped: w.sample.dropped,
            recovery_kf: w.sample.recovery_kf,
            request_kbps: request,
            cut_from_kbps: request.filter(|&k| k < was).map(|_| was),
            discarded: w.discarded,
            encode_disarmed: self.abr.abr.encode_down.disarmed(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::host::{Host, HostCfg};
    use super::*;

    fn frame(id: u32, bytes: u64, fec: u8) -> Frame {
        Frame {
            id,
            capture_ms: 0,
            wire_bytes: 0,
            shape: FrameShape::of(bytes, 1408, fec),
            encode_us: 1_000,
            repeat: false,
            idr: false,
        }
    }

    fn client(base: Instant) -> Client {
        Client::new(ClientCfg::default(), 11, base)
    }

    fn loss_ppm(c: &Client) -> u32 {
        crate::quic::window_loss_ppm(c.stats.fec_recovered_shards, 0, c.stats.packets_received)
    }

    /// Parity covers its block's losses or the frame dies; either way
    /// `loss_ppm` counts only what was repaired, which is why the field sees
    /// `loss_ppm=0` beside a lost frame.
    #[test]
    fn parity_repairs_its_block_and_an_unrecoverable_frame_reports_no_loss() {
        let mut c = client(Instant::now());
        // 45 000 bytes = 32 data shards, 4 parity at 10 %.
        let f = frame(1, 45_000, 10);
        assert_eq!(f.shape.data, 32);
        assert_eq!(f.shape.parity_of(0), 4);
        c.expect(&f, 0);
        c.complete(
            1,
            LossDraw {
                random: 0,
                burst_at: 10,
                burst_len: 4,
            },
            10,
        );
        assert_eq!(c.stats.frames_dropped, 0, "four shards, four parity");
        assert_eq!(c.stats.fec_recovered_shards, 4);
        assert!(loss_ppm(&c) > 0);

        let mut c = client(Instant::now());
        c.expect(&frame(2, 45_000, 10), 0);
        c.complete(
            2,
            LossDraw {
                random: 0,
                burst_at: 10,
                burst_len: 5,
            },
            10,
        );
        assert_eq!(
            c.stats.frames_dropped, 1,
            "one shard past the parity loses the frame"
        );
        assert_eq!(
            loss_ppm(&c),
            0,
            "an unrecoverable frame teaches loss_ppm nothing"
        );
    }

    /// One frame is one FEC block, so its whole parity pool covers loss
    /// wherever it lands — spread or in one burst.
    #[test]
    fn one_pool_covers_loss_wherever_it_lands() {
        for draw in [
            LossDraw {
                random: 22,
                ..LossDraw::default()
            },
            LossDraw {
                random: 0,
                burst_at: 40,
                burst_len: 22,
            },
        ] {
            let mut c = client(Instant::now());
            c.expect(&frame(1, 300_000, 10), 0);
            c.complete(1, draw, 5);
            assert_eq!(c.stats.frames_dropped, 0, "22 of 22 parity shards");
            assert_eq!(c.stats.fec_recovered_shards, 22);
        }
        let mut c = client(Instant::now());
        c.expect(&frame(2, 300_000, 10), 0);
        c.complete(
            2,
            LossDraw {
                random: 23,
                ..LossDraw::default()
            },
            5,
        );
        assert_eq!(
            c.stats.frames_dropped, 1,
            "one shard past the pool loses the frame"
        );
    }

    /// The window the ceiling injection lands in is discarded, exactly as the
    /// pump discards the probe tail: no report, no verdict.
    #[test]
    fn the_probe_window_is_discarded() {
        let base = Instant::now();
        let mut c = Client::new(
            ClientCfg {
                ceiling_at: Some((100, 170_000)),
                ..ClientCfg::default()
            },
            1,
            base,
        );
        let mut out = Vec::new();
        for t in 0..=750 {
            c.tick(t, base, &mut out);
        }
        assert!(
            !out.iter().any(|a| matches!(a, Action::Loss { .. })),
            "a discarded window sends no loss report"
        );
        assert!(c.windows[0].discarded);
    }

    /// The host's frames reach the client whole across the pacer, and one
    /// window of them is the wire rate the controller is handed: the budget
    /// plus what rounding each frame up to whole shards and the two-shard
    /// parity floor cost it.
    #[test]
    fn a_clean_window_reports_the_wire_rate_the_host_spent() {
        let mut host = Host::new(
            HostCfg {
                fps: 60,
                idr_pct: 100,
                ..HostCfg::default()
            },
            20_000,
            2,
        );
        let base = Instant::now();
        let mut c = client(base);
        let mut out = Vec::new();
        for t in 0..=750 {
            if let Some(f) = host.tick(t) {
                c.expect(&f, t);
                let burst = host.burst_of(&f);
                if c.deliver(f.id, burst).is_some() {
                    c.complete(f.id, LossDraw::default(), t);
                }
            }
            let (id, bytes) = host.release(t);
            if bytes > 0 && c.deliver(id, bytes).is_some() {
                c.complete(id, LossDraw::default(), t);
            }
            c.tick(t, base, &mut out);
        }
        let w = c.windows[0];
        assert!(
            (20_000..=22_000).contains(&w.actual_kbps),
            "a 20 000 kbps budget delivered {} kbps",
            w.actual_kbps
        );
    }
}
