//! Client model: FEC repair, keyframe asks, decode time, and the session
//! counters the real [`Driver`] assembles its window from.
//!
//! Only the wire is a model. The window, the verdict, the controller and the
//! capacity probe are the shipped code, driven the way
//! `client/pump/data.rs` drives them: counters in, actions out.

use super::host::{probe_chunk_bytes, Frame, FrameShape, ProbeDone, SHARD_WIRE_OVERHEAD};
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
/// Most shards past parity a NACK asks for (`design/loss-repair-nack-ack.md` D2).
const NACK_MAX_BEYOND: u32 = 2;
/// The acked chain stays engaged this long after the last loss (D5).
const ACK_HOLD_MS: u64 = 10_000;
/// Frames back an acked reference can reach: Vulkan Video's DPB. Past it the
/// encoder falls back to the plain chain.
const ACK_REACH_FRAMES: u64 = 8;

/// What the client does with a frame its parity could not close.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Repair {
    /// Today: the frame is lost and the client asks until a recovery frame lands.
    #[default]
    Rfi,
    /// A frame at most [`NACK_MAX_BEYOND`] short, on a round trip inside one
    /// frame period, gets the shards resent and completes a round trip later.
    /// Anything else falls to [`Repair::Rfi`].
    Nack,
    /// While the link has shown loss the encoder references only acknowledged
    /// frames, so a lost frame is skipped: still dropped, but nothing is asked.
    Ack,
}

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
    /// `PUNKTFUNK_ABR_PROBE_KBPS`. `None` = `probe_target_kbps(stream_cap)`;
    /// with `ramp`, the ramp's maximum instead.
    pub probe_target_kbps: Option<u32>,
    /// Host advertises `HOST_CAP2_RAMP`: measure the link during bring-up.
    pub ramp: bool,
    /// Host advertises `HOST_CAP2_DELIVERY`: report what arrived every window.
    pub reads_delivery: bool,
    /// Ceiling injected directly, for a scenario that replays a host which
    /// paused video for the burst. The window it lands in is discarded, as
    /// the probe tail is.
    pub ceiling_at: Option<(u64, u32)>,
    /// The host rebuilds its pipeline here: the window in flight describes the
    /// gap, and the client is left without a reference to decode against.
    pub rebuild_at_ms: Option<u64>,
    /// `false` = an explicit bitrate, so no controller.
    pub automatic: bool,
    /// PyroWave Automatic: the pin the Welcome resolved. `Some` runs the
    /// bring-up ramp as a fit check on it — a measured wall lowers it once,
    /// every other outcome leaves it, and no controller runs either way.
    pub pin_kbps: Option<u32>,
    pub repair: Repair,
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
            ramp: false,
            reads_delivery: true,
            ceiling_at: None,
            rebuild_at_ms: None,
            automatic: true,
            pin_kbps: None,
            repair: Repair::Rfi,
        }
    }
}

/// What the client sends the host in one tick. The driver's own
/// [`crate::abr::Action`]s become these; `unrecovered` is what the client
/// model knows and the real wire carries as a keyframe ask.
///
/// `Delivery` is the session's total packets received, and the only thing that
/// tells the host what is arriving. It goes out when the driver asks, which is
/// what makes the host's view of a shared path as thin here as on a wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    SetBitrate(u32),
    Keyframe,
    Loss { ppm: u32, unrecovered: bool },
    Delivery(u64),
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
    /// First-shard delay for this window. `None` = no frame opened in it.
    pub delay: Option<crate::abr::DelayTrend>,
    /// The learned link cap after this window, if one stands.
    pub link_cap: Option<u32>,
    /// The last delivered rate the link was marked at (`0` = none).
    pub link_mark_kbps: u32,
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
    /// Some of this frame has arrived: its first shard is already timed.
    seen: bool,
    /// Scenario-injected unrecoverable frame.
    forced: bool,
}

pub(super) struct Client {
    cfg: ClientCfg,
    rng: Rng,
    /// The scenario's zero, so a millisecond can become an `Instant`. Not the
    /// instant this session joined: a session that joins at 60 s still reads
    /// the run's clock, and dating its probe results from its own start put
    /// every report window a minute into the future.
    base: Instant,
    pub(super) abr: Driver,
    flight: VecDeque<InFlight>,
    lost_blocks: Vec<u32>,
    /// The session counters the driver differences its windows from.
    stats: Stats,
    /// Unrecoverable frames since the last window closed. The host's adaptive
    /// FEC is told; on the real wire the keyframe ask tells it.
    lost_frames: u64,
    /// Bytes NACK resends put on the wire past the budget.
    pub(super) resent_bytes: u64,
    /// When a frame last lost a shard: what keeps the acked chain engaged.
    loss_seen_ms: Option<u64>,
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
    /// Filler packets and AUs this burst has completed, so the session
    /// counters they feed are bumped once each.
    probe_packets: u64,
    probe_aus: u64,
    probe_chunk_bytes: u64,
    /// The host's end-of-burst report. The embedder's probe state keeps
    /// saying "done" until the next burst overwrites it, so the pump hands a
    /// report over on every iteration — and so does this. Its delivered
    /// figures keep growing while the queue drains, which is what the ramp
    /// times (`ProbeState::refresh_delivered`).
    probe_done: Option<ProbeDone>,
    pub(super) windows: Vec<WindowRec>,
    pub(super) owd_samples: Vec<u32>,
    /// Every bring-up ramp step: when it went out and what it asked for.
    pub(super) ramp_asks: Vec<(u64, u32)>,
    /// When the ramp stopped, and what it came to.
    pub(super) ramp_done: Option<(u64, crate::abr::probe::RampSummary)>,
    /// Every `SetBitrate` the driver emitted: when, and for what. The window
    /// record's `request_kbps` sees only asks that land on a close tick, so
    /// the ramp's own asks — the opening rate, the pin's verdict — live here.
    pub(super) set_asks: Vec<(u64, u32)>,
}

impl Client {
    pub(super) fn new(cfg: ClientCfg, seed: u64, base: Instant, joined: Instant) -> Self {
        let abr = Driver::new(
            DriverConfig {
                // A pinned session opens at its pin, and the pin is the
                // driver's own config — the controller stays off.
                start_kbps: if cfg.automatic && cfg.pin_kbps.is_none() {
                    cfg.start_kbps
                } else {
                    0
                },
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
                ramp: cfg.ramp,
                reads_delivery: cfg.reads_delivery,
                pin_kbps: cfg.pin_kbps,
            },
            joined,
        );
        Client {
            rng: Rng::new(seed),
            base,
            abr,
            flight: VecDeque::new(),
            lost_blocks: Vec::new(),
            stats: Stats::default(),
            lost_frames: 0,
            resent_bytes: 0,
            loss_seen_ms: None,
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
            probe_packets: 0,
            probe_aus: 0,
            probe_chunk_bytes: 1,
            probe_done: None,
            windows: Vec::new(),
            owd_samples: Vec::new(),
            ramp_asks: Vec::new(),
            ramp_done: None,
            set_asks: Vec::new(),
            cfg,
        }
    }

    /// What the session is running at. A fixed-rate session has no
    /// controller, so its rate is the one it negotiated; a pinned session's
    /// moves with the host's `Pinned` acks.
    pub(super) fn rate_kbps(&self) -> u32 {
        if self.cfg.automatic || self.cfg.pin_kbps.is_some() {
            self.abr.abr.current_kbps
        } else {
            self.cfg.start_kbps
        }
    }

    /// `false` = an explicit bitrate, which the governor never touches.
    pub(super) fn automatic(&self) -> bool {
        self.cfg.automatic
    }

    /// Frames this session could not decode, over the whole run.
    pub(super) fn frames_dropped(&self) -> u64 {
        self.stats.frames_dropped
    }

    /// Make the next frame unrecoverable, whatever the link does.
    pub(super) fn inject_lost_frame(&mut self, at_ms: u64) {
        self.force_loss_at_ms = Some(at_ms);
    }

    /// A host `BitrateChanged`. The driver queues it to the window close, as
    /// the pump's ack queue does.
    pub(super) fn push_ack(&mut self, kbps: u32, why: crate::quic::AckReason) {
        self.abr.on_ack(kbps, Some(why));
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
            seen: false,
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
    ///
    /// The first bytes of a frame are its first shard, which the reassembler
    /// times whatever becomes of the rest.
    pub(super) fn deliver(&mut self, frame: u32, bytes: u64, now_ms: u64) -> Option<u32> {
        let f = self.flight.iter_mut().find(|f| f.id == frame)?;
        let first = !std::mem::replace(&mut f.seen, true);
        let capture_ms = f.capture_ms;
        f.remaining = f.remaining.saturating_sub(bytes);
        let tail = (f.remaining == 0).then(|| f.shape.shards());
        if first {
            self.abr
                .on_shard_owd(i128::from(now_ms.saturating_sub(capture_ms)) * 1_000_000);
        }
        tail
    }

    /// Close one frame: repair what parity covers, then what [`Repair`] can,
    /// and count the rest. `rtt_ms` is the link's round trip right now.
    ///
    /// The NACK model resends only what closes the gap, and neither loses the
    /// resend nor holds the frames behind it: an upper bound on what a NACK buys.
    pub(super) fn complete(&mut self, frame: u32, draw: LossDraw, now_ms: u64, rtt_ms: u64) {
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
        let mut beyond = 0u32;
        let mut unrecoverable = f.forced;
        for (b, &lost_b) in self.lost_blocks.iter().enumerate() {
            if lost_b == 0 {
                continue;
            }
            let parity = f.shape.parity_of(b as u32);
            if lost_b <= parity {
                repaired += lost_b;
            } else {
                unrecoverable = true;
                beyond += lost_b - parity;
            }
        }
        let mut arrived = shards.saturating_sub(lost.min(shards));
        let period_ms = 1_000 / u64::from(self.cfg.refresh_hz.max(1));
        // Loss before this frame is what had the encoder on the acked chain.
        let engaged = self
            .loss_seen_ms
            .is_some_and(|t| now_ms.saturating_sub(t) <= ACK_HOLD_MS);
        if lost > 0 {
            self.loss_seen_ms = Some(now_ms);
        }
        let mut now_ms = now_ms;
        let mut skipped = false;
        if unrecoverable && !f.forced {
            match self.cfg.repair {
                Repair::Nack if beyond <= NACK_MAX_BEYOND && rtt_ms <= period_ms => {
                    unrecoverable = false;
                    repaired = lost;
                    arrived += beyond;
                    self.resent_bytes += u64::from(beyond) * shard_wire;
                    now_ms += rtt_ms;
                }
                Repair::Ack => {
                    skipped = engaged && rtt_ms <= ACK_REACH_FRAMES * period_ms;
                }
                _ => {}
            }
        }
        self.stats.packets_received += u64::from(arrived);
        self.stats.bytes_received += u64::from(arrived) * shard_wire;
        self.stats.fec_recovered_shards += u64::from(repaired);
        if unrecoverable {
            self.stats.frames_dropped += 1;
            self.lost_frames += 1;
            self.awaiting_idr |= !skipped;
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
        // A probe datagram is an accepted datagram: `session.rs` counts it in
        // `packets_received` with every other, and `packet/reassemble.rs`
        // adds the probe pair at the routing decision.
        let wire = self.cfg.shard_payload as u64 + SHARD_WIRE_OVERHEAD;
        let packets = self.probe_bytes / wire;
        self.stats.packets_received += packets - self.probe_packets;
        self.stats.probe_packets_received += packets - self.probe_packets;
        self.probe_packets = packets;
        // And a completed filler AU moves `frames_completed` like any other:
        // the reassembler splits probe from video, `session.rs`'s completion
        // count does not.
        let aus = self.probe_bytes / self.probe_chunk_bytes;
        self.stats.frames_completed += aus - self.probe_aus;
        self.probe_aus = aus;
    }

    /// What the pump would hand the driver right now: the host's report plus
    /// whatever has arrived since, which is still growing while the
    /// bottleneck queue drains.
    fn probe_report(&self, done: ProbeDone) -> ProbeReport {
        let packets = self.probe_packets;
        // Client receive interval: first to last filler arrival. Under two
        // packets there is no interval and the host's window stands.
        let client_interval_ms = if packets >= 2 && self.probe_last_ms > self.probe_first_ms {
            (self.probe_last_ms - self.probe_first_ms) as u32
        } else {
            0
        };
        ProbeReport {
            delivered_bytes: packets * PROBE_PACKET_BYTES,
            delivered_packets: packets,
            window_ms: if client_interval_ms > 0 {
                client_interval_ms
            } else {
                done.duration_ms
            },
            host_duration_ms: done.duration_ms,
            client_interval_ms,
            // The model's clock is one millisecond, so this carries no more
            // resolution than the field above it — the microsecond half of
            // the judge is a fact about wires, not about the model.
            client_interval_us: client_interval_ms.saturating_mul(1_000),
            host_bytes_sent: done.bytes_sent,
            wire_packets_sent: done.wire_packets_sent,
            send_dropped: 0,
        }
    }

    /// The host's `ProbeResult` landed: the burst's trailing edge, then the
    /// measurement, in the order one pump iteration sees them.
    pub(super) fn on_probe_result(&mut self, now_ms: u64, done: ProbeDone) {
        self.probing = false;
        let now = self.base + Duration::from_millis(now_ms);
        self.abr.on_probe_active(false, self.probe_duration_ms, now);
        self.abr.on_probe_result(self.probe_report(done), now);
        self.probe_done = Some(done);
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
        // probe state stands. Only the first is the measurement — except for
        // a ramp step, which is over when its bytes stop arriving.
        if let Some(done) = self.probe_done {
            self.abr.on_probe_result(self.probe_report(done), now);
        }
        let unrecovered = self.lost_frames > 0;
        let tick = self.abr.tick(now);
        let mut request = None;
        for action in tick.actions {
            match action {
                crate::abr::Action::Loss(ppm) => out.push(Action::Loss { ppm, unrecovered }),
                crate::abr::Action::SetBitrate(kbps) => {
                    request = Some(kbps);
                    self.set_asks.push((now_ms, kbps));
                    out.push(Action::SetBitrate(kbps));
                }
                crate::abr::Action::Probe {
                    target_kbps,
                    duration_ms,
                    ramp,
                } => {
                    if ramp {
                        self.ramp_asks.push((now_ms, target_kbps));
                    }
                    self.probing = true;
                    self.probe_duration_ms = duration_ms;
                    // A new burst overwrites the old state, report included.
                    self.probe_done = None;
                    self.probe_bytes = 0;
                    self.probe_first_ms = 0;
                    self.probe_last_ms = 0;
                    self.probe_packets = 0;
                    self.probe_aus = 0;
                    self.probe_chunk_bytes = probe_chunk_bytes(target_kbps);
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
                // As the pump does: a burst nobody answered is let go, or the
                // report tick stays suppressed for the rest of the session.
                crate::abr::Action::AbandonProbe => self.probing = false,
                crate::abr::Action::Delivery(packets) => out.push(Action::Delivery(packets)),
                // The simulated host has no pacer to hand a link rate to.
                crate::abr::Action::LinkRate(_) => {}
            }
        }
        if self.ramp_done.is_none() {
            if let Some(s) = self.abr.ramp_summary() {
                self.ramp_done = Some((now_ms, s));
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
            delay: w.sample.delay,
            link_cap: self.abr.abr.link_cap.kbps(),
            link_mark_kbps: self.abr.abr.link_mark_kbps,
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
        Client::new(ClientCfg::default(), 11, base, base)
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
            0,
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
            0,
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
            c.complete(1, draw, 5, 0);
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
            0,
        );
        assert_eq!(
            c.stats.frames_dropped, 1,
            "one shard past the pool loses the frame"
        );
    }

    /// NACK closes a frame two short on a round trip inside a frame period and
    /// leaves the rest to the ask; the acked chain skips a loss once engaged.
    #[test]
    fn a_nack_closes_a_short_frame_and_the_acked_chain_skips_a_lost_one() {
        let short = |beyond: u32| LossDraw {
            random: 0,
            burst_at: 10,
            burst_len: 4 + beyond,
        };
        let with = |repair: Repair| {
            Client::new(
                ClientCfg {
                    repair,
                    ..ClientCfg::default()
                },
                11,
                Instant::now(),
                Instant::now(),
            )
        };
        // 45 000 bytes at 10 %: 32 data, 4 parity. 60 Hz: a 16 ms period.
        let mut c = with(Repair::Nack);
        c.expect(&frame(1, 45_000, 10), 0);
        c.complete(1, short(2), 10, 16);
        assert_eq!(c.stats.frames_dropped, 0, "two past parity, resent");
        assert_eq!(c.resent_bytes, 2 * (1408 + SHARD_WIRE_OVERHEAD));
        assert!(!c.awaiting_idr);
        for (beyond, rtt) in [(3, 16), (1, 17)] {
            let mut c = with(Repair::Nack);
            c.expect(&frame(1, 45_000, 10), 0);
            c.complete(1, short(beyond), 10, rtt);
            assert_eq!(c.stats.frames_dropped, 1, "{beyond} past parity, rtt {rtt}");
            assert!(c.awaiting_idr, "falls to the ask");
            assert_eq!(c.resent_bytes, 0);
        }

        let mut c = with(Repair::Ack);
        c.expect(&frame(1, 45_000, 10), 0);
        c.complete(1, short(1), 10, 16);
        assert!(c.awaiting_idr, "no loss before it: the chain was plain");
        c.awaiting_idr = false;
        c.expect(&frame(2, 45_000, 10), 20);
        c.complete(2, short(1), 30, 16);
        assert_eq!(c.stats.frames_dropped, 2);
        assert!(!c.awaiting_idr, "engaged: skipped, nothing asked");
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
            Instant::now(),
        );
        let base = Instant::now();
        let mut c = client(base);
        let mut out = Vec::new();
        for t in 0..=750 {
            if let Some(f) = host.tick(t) {
                c.expect(&f, t);
                let burst = host.burst_of(&f);
                if c.deliver(f.id, burst, t).is_some() {
                    c.complete(f.id, LossDraw::default(), t, 0);
                }
            }
            let (id, bytes) = host.release(t);
            if bytes > 0 && c.deliver(id, bytes, t).is_some() {
                c.complete(id, LossDraw::default(), t, 0);
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
