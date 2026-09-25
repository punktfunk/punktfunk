//! Host model: frames at the session fps, the wire-budget arithmetic the host
//! runs, adaptive FEC, and the send pacer.
//!
//! The budget arithmetic is the shipped [`crate::abr::budget`]; only
//! [`auto_burst_bytes`] is still a copy of the host's, naming its source.

use super::Rng;
pub(super) use crate::abr::budget::{
    adapt_fec, encoder_kbps_for_budget, fec_target, FrameBudget, LossHorizon, FEC_ADAPTIVE_START,
    SHARD_WIRE_OVERHEAD,
};
use crate::abr::governor::ShareWindow;
use crate::quic::AckReason;
use std::time::Instant;

/// `config.rs` `MIN_RECOVERY_SHARDS`, and the `max_data_per_block` the host
/// negotiates (`native/handshake.rs`). 4 096 means an ordinary frame is one
/// block, so its whole parity pool covers loss anywhere in it.
const MIN_RECOVERY_SHARDS: u32 = 2;
const MAX_DATA_PER_BLOCK: u32 = 4_096;
/// `stream.rs` `paced_submit`: the pacer runs at ~3× the live encoder rate.
const PACE_FACTOR: u64 = 3;
/// `send_pacing.rs` `MAX_PACE_SPREAD`.
const MAX_PACE_SPREAD_MS: u64 = 100;

/// Bytes that leave unpaced (`send_pacing.rs` `auto_burst_bytes`).
pub(super) fn auto_burst_bytes(pace_rate_bps: u64, wire_bytes: usize) -> usize {
    const BURST_MS: u64 = 10;
    const BURST_MIN: usize = 16 * 1024;
    const BURST_MAX: usize = 256 * 1024;
    if pace_rate_bps == 0 {
        return (wire_bytes / 4).max(128 * 1024);
    }
    usize::try_from(pace_rate_bps * BURST_MS / 8000)
        .unwrap_or(BURST_MAX)
        .clamp(BURST_MIN, BURST_MAX)
}

/// Filler bytes per probe AU (`stream.rs` `ProbeBurst::begin`): a 240th of a
/// second at the burst's rate, clamped. The client completes one AU per
/// chunk, and every completion moves `Stats::frames_completed`.
pub(super) fn probe_chunk_bytes(target_kbps: u32) -> u64 {
    (u64::from(target_kbps) * 125 / 240).clamp(1_200, 16 * 1_024)
}

/// Per-block FEC geometry of one frame. Every block but the last holds
/// [`MAX_DATA_PER_BLOCK`] data shards; parity follows all the data on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FrameShape {
    pub data: u32,
    pub blocks: u32,
    pub last_k: u32,
    pub m_full: u32,
    pub m_last: u32,
}

impl FrameShape {
    pub(super) fn of(bytes: u64, shard_payload: u16, fec_percent: u8) -> Self {
        let data = (bytes.div_ceil(shard_payload.max(1) as u64) as u32).max(1);
        let blocks = data.div_ceil(MAX_DATA_PER_BLOCK);
        let last_k = data - (blocks - 1) * MAX_DATA_PER_BLOCK;
        FrameShape {
            data,
            blocks,
            last_k,
            m_full: recovery_for(MAX_DATA_PER_BLOCK.min(data), fec_percent),
            m_last: recovery_for(last_k, fec_percent),
        }
    }

    pub(super) fn parity(&self) -> u32 {
        (self.blocks - 1) * self.m_full + self.m_last
    }

    pub(super) fn shards(&self) -> u32 {
        self.data + self.parity()
    }

    /// Block a wire index belongs to. Wire order is every block's data, then
    /// every block's parity (`packetize.rs`).
    pub(super) fn block_of(&self, idx: u32) -> u32 {
        if idx < self.data {
            return idx / MAX_DATA_PER_BLOCK;
        }
        let p = idx - self.data;
        let full = (self.blocks - 1) * self.m_full;
        if p < full {
            p / self.m_full.max(1)
        } else {
            self.blocks - 1
        }
    }

    /// Parity that block holds.
    pub(super) fn parity_of(&self, block: u32) -> u32 {
        if block + 1 == self.blocks {
            self.m_last
        } else {
            self.m_full
        }
    }
}

/// `config.rs` `FecConfig::recovery_for`.
fn recovery_for(data_shards: u32, fec_percent: u8) -> u32 {
    if fec_percent == 0 || data_shards == 0 {
        return 0;
    }
    (data_shards * fec_percent as u32)
        .div_ceil(100)
        .max(MIN_RECOVERY_SHARDS)
}

/// What the source hands the encoder over one stretch of the session.
#[derive(Clone, Copy, Debug)]
pub(super) struct ContentPhase {
    pub until_ms: u64,
    /// Share of the encoder's per-frame bit allowance the content fills.
    pub fill_pct: u32,
    /// Share of the session's frames the source actually produces.
    pub active_pct: u32,
    /// Idle: the host repeats the last picture as a keepalive instead.
    pub idle: bool,
    /// One frame `cut_pct` of normal size every `cut_every_ms`.
    pub cut_every_ms: u64,
    pub cut_pct: u32,
    /// Frame sizes vary ± this much. Without it every frame rounds to the
    /// same shard count and the wire rate moves in 2 900 kbps steps at
    /// 4K165 — an artefact of identical frames, not of the link.
    pub size_jitter_pct: u32,
}

impl Default for ContentPhase {
    fn default() -> Self {
        ContentPhase {
            until_ms: u64::MAX,
            fill_pct: 100,
            active_pct: 100,
            idle: false,
            cut_every_ms: 0,
            cut_pct: 100,
            size_jitter_pct: 25,
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct HostCfg {
    pub fps: u32,
    pub audio_kbps: u32,
    pub shard_payload: u16,
    /// Rate the encoder can actually apply; an ask above it acks short with
    /// [`AckReason::EncoderLimit`].
    pub encoder_ceiling_kbps: Option<u32>,
    /// While `now_ms` is inside this half-open range the host refuses every
    /// climb at its live rate, naming the cadence — its GPU is behind, and the
    /// rate is not the lever. Empty range = never.
    pub cadence_refusal_ms: (u64, u64),
    /// Ack and rebuild latency for one retarget.
    pub retarget_ms: u64,
    /// Encode time per frame, µs, plus its spread. The rate a saturated GPU
    /// can sustain is one frame per `encode_us`.
    pub encode_us: u32,
    pub encode_jitter_us: u32,
    /// Encode time after `loaded_from_ms` (GPU contention arriving), and the
    /// slow swing on top of it: contention ebbs over seconds, which is what
    /// the controller's rolling minimum reads as a rise.
    pub loaded_encode_us: u32,
    pub loaded_from_ms: u64,
    pub encode_swing_us: u32,
    pub encode_swing_ms: u64,
    /// A weak encoder: past `encode_knee_kbps` every extra Mbps of budget
    /// costs `encode_us_per_mbps` more encode time. Here the rate really is
    /// the lever, and a notch that keeps it shows up in the next window.
    pub encode_knee_kbps: u32,
    pub encode_us_per_mbps: u32,
    /// A keyframe is this many times an ordinary frame.
    pub idr_pct: u32,
    /// How long after a keyframe ask a decodable recovery point reaches the
    /// wire. `0` = the next frame is an IDR. Larger where the host answers
    /// with an intra-refresh wave instead: the client stays frozen through it.
    pub recovery_ms: u64,
    pub content: Vec<ContentPhase>,
    /// Older host: never marks idle repeats.
    pub marks_repeats: bool,
    /// Host serves probe requests during bring-up, without the 10 s spacing
    /// (`HOST_CAP2_RAMP`). An older host answers the first one and rejects
    /// every step behind it.
    pub ramp: bool,
    /// Display, capture and encoder bring-up: the gap between the punch and
    /// the first video frame, which is what the ramp measures the link in.
    pub bringup_ms: u64,
    /// `false` = a host that advertises the ramp and then answers no step.
    pub answers_probes: bool,
    /// `false` = a host that predates renegotiation: it applies nothing and
    /// answers nothing, and the controller retires itself.
    pub acks: bool,
    /// PyroWave: the session's rate is a pin. Asks are refused with it —
    /// except the bring-up ramp's verdict, once, inside the window, and only
    /// ever lower (`native/control.rs` `PyroWavePin`).
    pub pinned: bool,
}

impl Default for HostCfg {
    fn default() -> Self {
        HostCfg {
            fps: 60,
            audio_kbps: 256,
            shard_payload: 1408,
            encoder_ceiling_kbps: None,
            cadence_refusal_ms: (u64::MAX, u64::MAX),
            retarget_ms: 120,
            encode_us: 3_500,
            encode_jitter_us: 0,
            loaded_encode_us: 0,
            loaded_from_ms: u64::MAX,
            encode_swing_us: 0,
            encode_swing_ms: 1_500,
            encode_knee_kbps: u32::MAX,
            encode_us_per_mbps: 0,
            idr_pct: 400,
            recovery_ms: 0,
            content: vec![ContentPhase::default()],
            marks_repeats: true,
            ramp: false,
            bringup_ms: 0,
            answers_probes: true,
            acks: true,
            pinned: false,
        }
    }
}

/// A speed-test burst in flight (`stream.rs` `ProbeBurst`).
struct ProbeBurst {
    target_kbps: u32,
    start_ms: u64,
    end_ms: u64,
    bytes_sent: u64,
}

/// `native/control.rs` `MIN_PROBE_INTERVAL`: one burst per 10 s, lifted for
/// the bring-up ramp.
const MIN_PROBE_INTERVAL_MS: u64 = 10_000;

/// What the host tells the client about a finished burst
/// (`quic::ProbeResult`), in the two domains the wire carries: payload bytes
/// offered, and the wire packets they became.
#[derive(Clone, Copy, Debug)]
pub(super) struct ProbeDone {
    pub duration_ms: u32,
    pub bytes_sent: u64,
    pub wire_packets_sent: u32,
}

/// One frame on its way to the client.
#[derive(Clone, Copy, Debug)]
pub(super) struct Frame {
    pub id: u32,
    pub capture_ms: u64,
    pub wire_bytes: u64,
    pub shape: FrameShape,
    pub encode_us: u32,
    pub repeat: bool,
    pub idr: bool,
}

pub(super) struct Host {
    cfg: HostCfg,
    rng: Rng,
    /// Wire budget the encoder is running at, and the ask in flight: when it
    /// lands, at what rate, and what the ack will name.
    budget_kbps: u32,
    pending: Option<(u64, u32, AckReason)>,
    /// A pinned session's verdict ask already landed (`Pinned` sessions only).
    pin_fit_taken: bool,
    /// This session's share of a path it is not alone on (`0` = none), and the
    /// unsolicited ack carrying it. Its own slot: a share must not swallow the
    /// answer the client is waiting for.
    share_kbps: u32,
    governing: Option<(u64, u32)>,
    fec_percent: u8,
    fec_horizon: LossHorizon,
    unrecovered_run: u32,
    next_id: u32,
    next_frame_us: u64,
    /// Keyframe owed to the client, when it comes due, and the burst in
    /// flight.
    idr_owed: bool,
    idr_due_ms: Option<u64>,
    /// Recovery frames sent in answer to an ask, and the bytes each spent past an
    /// ordinary frame's allowance.
    pub(super) waves: u32,
    pub(super) wave_excess_bytes: u64,
    probe: Option<ProbeBurst>,
    last_probe_ms: Option<u64>,
    swing_us: u32,
    swing_until_ms: u64,
    pace_left: u64,
    pace_frame: u32,
    pace_per_ms: u64,
    pace_until_ms: u64,
    /// Tail of the frame the next one preempted: the send thread is serial,
    /// so it finishes what it holds before picking the new frame up.
    flush: Option<(u32, u64)>,
    next_cut_ms: u64,
    /// Wire bytes this session has handed the link, filler included — the
    /// host's own send counter (`Stats::bytes_sent`), which knows nothing about
    /// whose index space a packet is in.
    offered_bytes: u64,
    /// What the client's delivery reports say about the path, and the window
    /// they are measured over. `None` until the first one arrives: a zero there
    /// would read as a path refusing everything this session offered it.
    share_window: ShareWindow,
    offered_kbps: u32,
    delivered_kbps: Option<u32>,
    streaming: bool,
}

impl Host {
    pub(super) fn new(cfg: HostCfg, start_kbps: u32, seed: u64, joined: Instant) -> Self {
        Host {
            cfg,
            rng: Rng::new(seed),
            budget_kbps: start_kbps,
            pending: None,
            pin_fit_taken: false,
            share_kbps: 0,
            governing: None,
            fec_percent: FEC_ADAPTIVE_START,
            fec_horizon: LossHorizon::default(),
            unrecovered_run: 0,
            next_id: 1,
            next_frame_us: 0,
            idr_owed: true,
            idr_due_ms: None,
            waves: 0,
            wave_excess_bytes: 0,
            probe: None,
            last_probe_ms: None,
            swing_us: 0,
            swing_until_ms: 0,
            pace_left: 0,
            pace_frame: 0,
            pace_per_ms: 0,
            pace_until_ms: 0,
            flush: None,
            next_cut_ms: 0,
            offered_bytes: 0,
            share_window: ShareWindow::new(joined, 0),
            offered_kbps: 0,
            delivered_kbps: None,
            streaming: false,
        }
    }

    /// A `SetBitrate` landed. The ack the client gets back is what the encoder
    /// can apply, not what was asked, and it names what held it short
    /// (`native/control.rs`: the share, the ceiling clamp, then the cadence
    /// hold).
    ///
    /// A pinned session answers every ask `Pinned` with the pin as it stands.
    /// The one exception is the ramp's verdict inside the bring-up window —
    /// `now_ms < bringup_ms` is `ramp_open` — which lowers the pin to what it
    /// asked. Never a raise, never twice.
    pub(super) fn on_set_bitrate(&mut self, now_ms: u64, kbps: u32) {
        if !self.cfg.acks {
            return;
        }
        if self.cfg.pinned {
            let open = now_ms < self.cfg.bringup_ms;
            if open && !self.pin_fit_taken && kbps > 0 && kbps < self.budget_kbps {
                self.pin_fit_taken = true;
                self.budget_kbps = kbps;
            }
            self.pending = Some((
                now_ms + self.cfg.retarget_ms,
                self.budget_kbps,
                AckReason::Pinned,
            ));
            return;
        }
        let ceiling = self.cfg.encoder_ceiling_kbps.unwrap_or(u32::MAX);
        let mut applied = kbps.min(ceiling);
        let mut why = if applied < kbps {
            AckReason::EncoderLimit
        } else {
            AckReason::Granted
        };
        if self.share_kbps > 0 && applied > self.share_kbps {
            applied = self.share_kbps;
            why = AckReason::Governor;
        }
        let (from, until) = self.cfg.cadence_refusal_ms;
        if (from..until).contains(&now_ms) && applied > self.budget_kbps {
            applied = self.budget_kbps;
            why = AckReason::Cadence;
        }
        self.pending = Some((now_ms + self.cfg.retarget_ms, applied, why));
    }

    /// Coalesced, as the control task coalesces it: asks while one is already
    /// due do not move the due time.
    pub(super) fn on_keyframe_request(&mut self, now_ms: u64) {
        self.idr_due_ms.get_or_insert(now_ms + self.cfg.recovery_ms);
    }

    /// Arm a speed-test burst (`stream.rs` `ProbeBurst::begin`), unless the
    /// spacing refuses it. Bring-up is exempt on a host that serves the ramp:
    /// there is no pipeline yet, so a step costs the session nothing.
    pub(super) fn on_probe_request(&mut self, now_ms: u64, target_kbps: u32, duration_ms: u32) {
        if !self.cfg.answers_probes {
            return;
        }
        let ramping = self.cfg.ramp && now_ms < self.cfg.bringup_ms;
        let spaced = self
            .last_probe_ms
            .is_none_or(|t| now_ms - t >= MIN_PROBE_INTERVAL_MS);
        if !ramping && !spaced {
            return;
        }
        self.last_probe_ms = Some(now_ms);
        self.probe = Some(ProbeBurst {
            target_kbps,
            start_ms: now_ms,
            end_ms: now_ms + u64::from(duration_ms),
            bytes_sent: 0,
        });
    }

    /// Filler the burst may put on the wire this millisecond: elapsed × rate
    /// minus what has gone (`stream.rs` `allowed_bytes`). The host's 64 KiB
    /// `PROBE_PUMP_BYTES` slice is one send-loop pass, not a rate limit — the
    /// loop passes many times a millisecond — so it does not bind here.
    pub(super) fn probe_release(&mut self, now_ms: u64) -> u64 {
        let Some(p) = self.probe.as_mut() else {
            return 0;
        };
        let elapsed = now_ms.min(p.end_ms) - p.start_ms;
        let allowed = elapsed * u64::from(p.target_kbps) * 125 / 1_000;
        let take = allowed.saturating_sub(p.bytes_sent);
        p.bytes_sent += take;
        self.offered_bytes += take;
        take
    }

    /// The burst's own report once it expires. `bytes_sent` is the payload
    /// the filler carried; the wire packets are those bytes plus their
    /// headers, which is the domain the client counts arrivals in.
    pub(super) fn probe_done(&mut self, now_ms: u64) -> Option<ProbeDone> {
        let p = self.probe.as_ref()?;
        if now_ms < p.end_ms {
            return None;
        }
        let wire = self.cfg.shard_payload as u64 + SHARD_WIRE_OVERHEAD;
        let done = ProbeDone {
            duration_ms: (p.end_ms - p.start_ms) as u32,
            bytes_sent: p.bytes_sent * self.cfg.shard_payload as u64 / wire,
            wire_packets_sent: (p.bytes_sent / wire) as u32,
        };
        self.probe = None;
        Some(done)
    }

    /// Host adaptive FEC closes on the client's loss report, sized for the
    /// frame this session's budget buys — the same call the host's control
    /// task makes, with the facts a host has without asking.
    pub(super) fn on_loss_report(&mut self, loss_ppm: u32, unrecovered: bool) {
        self.unrecovered_run = if unrecovered {
            self.unrecovered_run.saturating_add(1)
        } else {
            0
        };
        self.fec_percent = fec_target(
            loss_ppm,
            self.fec_percent,
            self.unrecovered_run,
            FrameBudget {
                budget_kbps: self.budget_kbps,
                audio_kbps: self.cfg.audio_kbps,
                shard_payload: self.cfg.shard_payload,
                fps: self.cfg.fps,
            },
            &mut self.fec_horizon,
        );
    }

    /// The encoder target, which is the `current_kbps` the host governor reads.
    pub(super) fn budget_kbps(&self) -> u32 {
        self.budget_kbps
    }

    /// A client [`crate::quic::DeliveryReport`] arrived. Its boundary closes the
    /// share window, so what this session offered and what reached the client
    /// cover one stretch of link (`native/control.rs`).
    pub(super) fn on_delivery_report(&mut self, now: Instant, packets_received: u64) {
        let wire = self.cfg.shard_payload as u64 + SHARD_WIRE_OVERHEAD;
        let (offered, delivered, streaming) =
            self.share_window
                .close(now, self.offered_bytes, packets_received, wire);
        self.offered_kbps = offered;
        self.delivered_kbps = Some(delivered);
        self.streaming = streaming;
    }

    /// The session was already streaming when the reported window opened, so
    /// the pair of rates above is a reading of the path.
    pub(super) fn streaming(&self) -> bool {
        self.streaming
    }

    /// The wire rate this session put out over the last reported window.
    pub(super) fn offered_kbps(&self) -> u32 {
        self.offered_kbps
    }

    /// What the client's last report said was arriving, kbps. `None` until one
    /// has arrived — the host knows nothing about this session's air until then.
    pub(super) fn delivered_kbps(&self) -> Option<u32> {
        self.delivered_kbps
    }

    /// The source has nothing new: the host is repeating the last picture, so
    /// this session is not asking for its share.
    pub(super) fn idle(&self, now_ms: u64) -> bool {
        self.phase(now_ms).idle
    }

    /// This session's share of a shared path, on its way to the client.
    pub(super) fn govern(&mut self, now_ms: u64, share_kbps: u32) {
        if !self.cfg.acks {
            return;
        }
        self.share_kbps = share_kbps;
        self.governing = Some((now_ms + self.cfg.retarget_ms, share_kbps));
    }

    /// The share the unsolicited ack carries, once the retarget lands. A share
    /// under the live rate retargets the encoder; one above it only travels,
    /// because the share is a ceiling the client still has to earn.
    pub(super) fn apply_governor(&mut self, now_ms: u64) -> Option<u32> {
        let (at, share) = self.governing?;
        if now_ms < at {
            return None;
        }
        self.governing = None;
        if share > 0 {
            self.budget_kbps = self.budget_kbps.min(share);
        }
        Some(share)
    }

    /// The rate the encoder is now running at, `Some` on the tick it changes,
    /// with the reason the ack carries.
    pub(super) fn apply_pending(&mut self, now_ms: u64) -> Option<(u32, AckReason)> {
        match self.pending {
            Some((at, kbps, why)) if now_ms >= at => {
                self.pending = None;
                self.budget_kbps = kbps;
                Some((kbps, why))
            }
            _ => None,
        }
    }

    fn phase(&self, now_ms: u64) -> ContentPhase {
        *self
            .cfg
            .content
            .iter()
            .find(|p| now_ms < p.until_ms)
            .unwrap_or(&self.cfg.content[self.cfg.content.len() - 1])
    }

    fn encode_us(&mut self, now_ms: u64) -> u32 {
        let loaded = now_ms >= self.cfg.loaded_from_ms;
        let base = if loaded {
            self.cfg.loaded_encode_us
        } else {
            self.cfg.encode_us
        };
        if loaded && self.cfg.encode_swing_us > 0 && now_ms >= self.swing_until_ms {
            self.swing_us = self.rng.below(u64::from(self.cfg.encode_swing_us) + 1) as u32;
            self.swing_until_ms = now_ms + self.cfg.encode_swing_ms;
        }
        let swing = if loaded { self.swing_us } else { 0 };
        // What the rate itself costs this encoder, past where it keeps up.
        let over_mbps = self.budget_kbps.saturating_sub(self.cfg.encode_knee_kbps) / 1_000;
        let rate_us = over_mbps.saturating_mul(self.cfg.encode_us_per_mbps);
        if self.cfg.encode_jitter_us == 0 {
            return base + swing + rate_us;
        }
        base + swing + rate_us + self.rng.below(u64::from(self.cfg.encode_jitter_us) + 1) as u32
    }

    /// Produce this millisecond's frame, if the frame clock fired. A loaded
    /// encoder caps the rate at one frame per `encode_us`, which is what turns
    /// GPU contention into a short window. Nothing leaves before the pipeline
    /// exists. A recovery frame that answers an ask counts as a wave.
    pub(super) fn tick(&mut self, now_ms: u64) -> Option<Frame> {
        if now_ms < self.cfg.bringup_ms {
            self.next_frame_us = self.cfg.bringup_ms * 1_000;
            return None;
        }
        if now_ms * 1_000 < self.next_frame_us {
            return None;
        }
        let phase = self.phase(now_ms);
        let encode_us = self.encode_us(now_ms);
        let source_fps = (self.cfg.fps * phase.active_pct / 100).max(1);
        let encoder_fps = (1_000_000 / encode_us.max(1)).max(1);
        let fps_eff = source_fps.min(encoder_fps);
        // Accumulate the period so the long-run rate is the frame rate and not
        // the millisecond it was rounded to; resync only when a stall put the
        // clock a whole frame behind.
        let period_us = 1_000_000 / fps_eff as u64;
        self.next_frame_us += period_us;
        if self.next_frame_us + period_us < now_ms * 1_000 {
            self.next_frame_us = now_ms * 1_000 + period_us;
        }

        let enc_kbps = encoder_kbps_for_budget(
            self.budget_kbps,
            self.cfg.audio_kbps,
            self.fec_percent,
            self.cfg.shard_payload,
        );
        // Per-frame allowance is the session fps, not the rate the source
        // manages: a frame-driven source spends a slice of the budget.
        let frame_bytes = enc_kbps as u64 * 1_000 / 8 / self.cfg.fps.max(1) as u64;
        let asked = matches!(self.idr_due_ms, Some(t) if now_ms >= t);
        let idr = std::mem::take(&mut self.idr_owed) || asked;
        if idr {
            self.idr_due_ms = None;
        }
        self.waves += u32::from(asked);
        let cut = phase.cut_every_ms > 0 && now_ms >= self.next_cut_ms;
        if cut {
            self.next_cut_ms = now_ms + phase.cut_every_ms;
        }
        let bytes = if phase.idle {
            // Keepalive repeat: one shard's worth, flagged so the controller
            // reads the window as stillness.
            self.cfg.shard_payload as u64
        } else {
            let mut b = frame_bytes * phase.fill_pct as u64 / 100;
            if phase.size_jitter_pct > 0 {
                let j = u64::from(phase.size_jitter_pct);
                b = b * (100 + self.rng.below(2 * j + 1) - j) / 100;
            }
            if cut {
                b = b * phase.cut_pct as u64 / 100;
            }
            if idr {
                let plain = b;
                b = b * self.cfg.idr_pct as u64 / 100;
                if asked {
                    self.wave_excess_bytes += b.saturating_sub(plain);
                }
            }
            b.max(1)
        };
        let shape = FrameShape::of(bytes, self.cfg.shard_payload, self.fec_percent);
        let wire_bytes =
            shape.shards() as u64 * (self.cfg.shard_payload as u64 + SHARD_WIRE_OVERHEAD);
        let id = self.next_id;
        self.next_id += 1;
        self.offered_bytes += wire_bytes;
        // Pacer: the burst leaves now, the overflow over its wire time at 3×
        // the budget, bounded by MAX_PACE_SPREAD.
        let pace_rate_bps = self.budget_kbps as u64 * 1_000 * PACE_FACTOR;
        let burst = auto_burst_bytes(pace_rate_bps, wire_bytes as usize) as u64;
        let overflow = wire_bytes.saturating_sub(burst);
        let spread_ms = if overflow > 0 && pace_rate_bps > 0 {
            (overflow * 8 * 1_000 / pace_rate_bps).clamp(1, MAX_PACE_SPREAD_MS)
        } else {
            0
        };
        if self.pace_left > 0 {
            self.flush = Some((self.pace_frame, self.pace_left));
        }
        self.pace_frame = id;
        self.pace_left = overflow;
        self.pace_per_ms = if spread_ms > 0 {
            overflow.div_ceil(spread_ms)
        } else {
            0
        };
        self.pace_until_ms = now_ms + spread_ms;
        Some(Frame {
            id,
            capture_ms: now_ms,
            wire_bytes,
            shape,
            encode_us,
            repeat: phase.idle,
            idr,
        })
    }

    /// The preempted frame's tail, offered before the new frame's burst.
    pub(super) fn take_flush(&mut self) -> Option<(u32, u64)> {
        self.flush.take()
    }

    /// Bytes the pacer releases this millisecond, after the burst.
    pub(super) fn release(&mut self, now_ms: u64) -> (u32, u64) {
        if self.pace_left == 0 {
            return (self.pace_frame, 0);
        }
        // Past the spread the remainder goes at once: the send thread finishes
        // the frame before it picks up the next one.
        let take = if now_ms >= self.pace_until_ms {
            self.pace_left
        } else {
            self.pace_per_ms.min(self.pace_left)
        };
        self.pace_left -= take;
        (self.pace_frame, take)
    }

    /// Bytes of a new frame that leave unpaced.
    pub(super) fn burst_of(&self, f: &Frame) -> u64 {
        f.wire_bytes - self.pace_left
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget mirrors agree with the host's own test vectors: a roundtrip
    /// through the wire derivation lands back on the budget.
    #[test]
    fn the_budget_mirror_matches_the_host_derivation() {
        for (budget, audio, fec) in [
            (20_000u32, 256u32, 5u8),
            (171_294, 512, 10),
            (2_000, 96, 50),
        ] {
            let enc = encoder_kbps_for_budget(budget, audio, fec, 1408);
            let wire = enc as u64 * (1408 + SHARD_WIRE_OVERHEAD) * (100 + fec as u64)
                / (1408 * 100)
                + audio as u64;
            assert!(
                wire <= budget as u64 + 2 && wire + budget as u64 / 50 >= budget as u64,
                "{budget} kbps → {enc} kbps encoder → {wire} kbps wire"
            );
        }
        // MIN_BITRATE_KBPS floors a budget the audio reservation swallows.
        assert_eq!(encoder_kbps_for_budget(300, 256, 5, 1408), 500);
    }

    /// `adapt_fec` on the host's own band: clean decays to the floor, loss
    /// ramps past it, and 50 % is the ceiling.
    #[test]
    fn adaptive_fec_maps_loss_to_the_hosts_recovery_band() {
        assert_eq!(adapt_fec(0), 5);
        assert_eq!(adapt_fec(10_000), 5, "1 % loss is still inside the floor");
        assert_eq!(adapt_fec(50_000), 8, "5 % → ceil(7) + 1");
        assert_eq!(adapt_fec(400_000), 50);
        // A run of unrecovered frames adds the step for four windows, then
        // lets it go; decay is one point per window.
        let frame = FrameBudget {
            budget_kbps: 12_500,
            audio_kbps: 256,
            shard_payload: 1408,
            fps: 30,
        };
        let mut h = LossHorizon::default();
        assert_eq!(fec_target(0, 5, 1, frame, &mut h), 8);
        assert_eq!(fec_target(0, 8, 5, frame, &mut h), 7, "past the run, decay");
        assert_eq!(fec_target(0, 8, 0, frame, &mut h), 7);
    }

    /// Parity is at least two shards per block, and the wire index of every
    /// shard maps back to the block that can repair it.
    #[test]
    fn a_frames_blocks_carry_their_own_parity() {
        let shape = FrameShape::of(100_000, 1408, 10);
        assert_eq!(shape.data, 72);
        assert_eq!(shape.blocks, 1, "an ordinary frame is one block");
        assert_eq!(shape.parity(), 8);
        assert_eq!(shape.shards(), 80);
        assert_eq!(shape.block_of(0), 0);
        assert_eq!(shape.block_of(79), 0);
        // The 2-shard floor: 5 % of 9 shards is one, and one is not enough.
        assert_eq!(FrameShape::of(12_000, 1408, 5).parity(), 2);
        // Past 4 096 data shards a second block starts, with its own parity.
        let big = FrameShape::of(4_097 * 1408, 1408, 10);
        assert_eq!(big.blocks, 2);
        assert_eq!(big.last_k, 1);
        assert_eq!(big.m_full, 410);
        assert_eq!(big.m_last, 2);
        assert_eq!(big.block_of(4_096), 1);
        assert_eq!(big.parity_of(1), 2);
    }

    /// The burst rule: a 5 Mbps stream bursts about 19 KiB, a fat link clamps
    /// at 256 KiB (`send_pacing.rs`).
    #[test]
    fn the_pacing_burst_follows_the_hosts_rule() {
        assert_eq!(auto_burst_bytes(15_000_000, 200_000), 18_750);
        assert_eq!(auto_burst_bytes(90_000_000, 200_000), 112_500);
        assert_eq!(auto_burst_bytes(400_000_000, 900_000), 256 * 1024);
        assert_eq!(auto_burst_bytes(1_000_000, 200_000), 16 * 1024);
        assert_eq!(auto_burst_bytes(0, 900_000), 225_000);
    }

    /// A frame-driven source spends its share of the budget, and a saturated
    /// encoder caps the frame rate at one frame per encode.
    #[test]
    fn the_source_rate_bounds_what_the_budget_buys() {
        let cfg = HostCfg {
            fps: 165,
            content: vec![ContentPhase {
                active_pct: 50,
                ..ContentPhase::default()
            }],
            ..HostCfg::default()
        };
        let mut host = Host::new(cfg, 100_000, 7, Instant::now());
        let frames = (0..1_000).filter_map(|t| host.tick(t)).count();
        assert_eq!(frames, 82, "165 fps × 50 % over one second");

        let mut loaded = Host::new(
            HostCfg {
                fps: 165,
                encode_us: 18_000,
                ..HostCfg::default()
            },
            100_000,
            7,
            Instant::now(),
        );
        let frames = (0..1_000).filter_map(|t| loaded.tick(t)).count();
        assert_eq!(frames, 55, "one frame per 18 ms of encode");
    }
}
