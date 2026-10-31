//! Bitrate and FEC policy for one native session: the rate a Hello resolves to, the PyroWave
//! pin, the budget↔encoder derivation, the encoder ceiling a session learns, and adaptive FEC.

// The wire budget, adaptive FEC and their band: one arithmetic, shared with
// the client's controller and the link simulator.
use punktfunk_core::abr::budget::{
    budget_kbps_for_encoder, encoder_kbps_for_budget, MIN_BITRATE_KBPS,
};
use punktfunk_core::quic::AckReason;
use punktfunk_core::Session;
use std::sync::atomic::{AtomicU8, Ordering};

use super::Punktfunk1Source;

/// Fallback when `Hello::bitrate_kbps == 0` (20 Mbps). A client that knows its link asks.
const DEFAULT_BITRATE_KBPS: u32 = 20_000;
/// Ceiling on a resolved rate: headroom over the 1 Gbps+ Leopard target
/// (5K@240 with margin), echoed in `Welcome::bitrate_kbps`. The encoder is
/// pixel-rate bound (~1 Gpix/s per NVENC, ~2 with a 2-way split), so the real
/// ceiling is the transport send path, not this number. The floor lives with
/// the derivation it floors ([`MIN_BITRATE_KBPS`]).
pub(super) const MAX_BITRATE_KBPS: u32 = 8_000_000;

/// A rate this session's encoder was seen to refuse, and the clock that tests
/// that refusal again.
///
/// One transient short apply used to cap the session for good. This is the
/// client's own learned-cap lifecycle on the host side: cleared when the
/// encoder opens at a different configuration, and re-tested once the wait has
/// run out — the wait doubles each time the refusal is still there, so an
/// encoder that means it costs one ask every few minutes.
pub(super) struct EncoderCeiling {
    cap: punktfunk_core::abr::LearnedCap,
    /// When the cap was last written. The wait is `reprobe_after` report
    /// windows of real time, the same 12 s → 96 s ladder the client re-probes
    /// its own caps on.
    written_at: std::time::Instant,
}

impl EncoderCeiling {
    pub(super) fn new() -> Self {
        EncoderCeiling {
            cap: punktfunk_core::abr::LearnedCap::new(),
            written_at: std::time::Instant::now(),
        }
    }

    /// The rate to hand the encoder for an ask of `want`, and what the client
    /// is told held it there.
    ///
    /// Past the wait the ask goes through: the encoder's answer is the only
    /// evidence that the ceiling still stands. The cap moves up an eighth
    /// first, exactly as the client's re-probe does, so a refusal that is still
    /// there re-latches under the lift and backs the clock off.
    pub(super) fn resolve(&mut self, want: u32) -> (u32, AckReason) {
        let Some(cap) = self.cap.kbps() else {
            return (want, AckReason::Granted);
        };
        if want <= cap {
            return (want, AckReason::Granted);
        }
        if self.written_at.elapsed() < self.wait() {
            tracing::info!(
                requested_kbps = want,
                ceiling_kbps = cap,
                "bitrate request clamped to the known encoder ceiling"
            );
            return (cap, AckReason::EncoderLimit);
        }
        self.write(cap.saturating_add(cap / 8));
        tracing::info!(
            requested_kbps = want,
            ceiling_kbps = cap,
            "re-testing the encoder ceiling — letting the request reach the encoder"
        );
        (want, AckReason::Granted)
    }

    /// What the encoder made of an ask of `want`. Short is the ceiling, again;
    /// taking a whole ask above the ceiling is the ceiling gone. A whole ask under
    /// it is no evidence either way.
    pub(super) fn note_applied(&mut self, want: u32, applied: u32) {
        if applied >= want {
            if self.cap.kbps().is_some_and(|cap| applied > cap) {
                tracing::info!(
                    applied_kbps = applied,
                    "the encoder took the whole rate — dropping the ceiling it refused before"
                );
                self.cap.drop_cap();
            }
            return;
        }
        // `latch` backs the clock off only for a cap that binds tighter; an
        // encoder that took more than the ceiling remembered has moved it up,
        // and that is not evidence of a standing refusal.
        if !self.cap.latch(applied, MIN_BITRATE_KBPS) {
            self.cap.park(applied);
        }
        self.written_at = std::time::Instant::now();
        tracing::info!(
            requested_kbps = want,
            ceiling_kbps = self.cap.kbps().unwrap_or(applied),
            retest_in_s = self.wait().as_secs(),
            "the encoder applied less than the rate asked — ceiling learned"
        );
    }

    /// The encoder opened at a different configuration. Whatever it refused was
    /// refused by an encoder that no longer exists.
    pub(super) fn clear(&mut self) {
        if self.cap.kbps().is_some() {
            tracing::info!("encoder rebuilt at a new configuration — its learned ceiling is gone");
            self.cap.drop_cap();
        }
    }

    fn write(&mut self, kbps: u32) {
        self.cap.park(kbps);
        self.written_at = std::time::Instant::now();
    }

    fn wait(&self) -> std::time::Duration {
        punktfunk_core::abr::WINDOW * self.cap.reprobe_after()
    }

    /// Spend the whole wait at once, so a test is not twelve seconds long.
    #[cfg(test)]
    fn spend_the_wait(&mut self) {
        self.written_at = self
            .written_at
            .checked_sub(self.wait())
            .expect("a monotonic clock older than one wait");
    }
}

/// `0` → host default; anything else clamped into `[MIN, MAX]`.
pub(super) fn resolve_bitrate_kbps(requested: u32) -> u32 {
    if requested == 0 {
        DEFAULT_BITRATE_KBPS
    } else {
        requested.clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS)
    }
}

/// The session's PyroWave bits per pixel: the client's quality
/// ([`EXT_TAG_PYROWAVE_QUALITY`](punktfunk_core::quic::EXT_TAG_PYROWAVE_QUALITY)) held inside
/// `[BPP_FLOOR, BPP_MAX]`, else the host's row `pyrowave_bpp`. Fixed for the session, so a
/// mode switch re-pins at the same quality.
pub(super) fn session_pyrowave_bpp(client_x100: u16) -> f64 {
    punktfunk_core::pyrowave::bpp_of_x100(client_x100)
        .unwrap_or_else(|| pf_host_config::config().pyrowave_bpp)
}

/// PyroWave pins the session's bits per pixel (`pyrowave_bpp`, [`session_pyrowave_bpp`]) for
/// the negotiated mode, not the 20 Mbps H.26x default: a Fixed session runs at the pin, an
/// Automatic one adapts under it. A non-zero client rate asks for Fixed and its number is
/// ignored: bits per pixel is the quality knob, and it holds across modes. Every pin goes
/// through `ceiling`, the session's `PUNKTFUNK_PYROWAVE_MAX_MBPS` in kbps. H.26x/AV1 explicit
/// rates stand.
pub(super) fn resolve_bitrate_kbps_for(
    codec: crate::encode::Codec,
    requested: u32,
    mode: &punktfunk_core::config::Mode,
    chroma: crate::encode::ChromaFormat,
    bit_depth: u8,
    bpp: f64,
    ceiling: Option<u32>,
) -> u32 {
    if codec == crate::encode::Codec::PyroWave {
        if requested != 0 {
            tracing::info!(
                requested_kbps = requested,
                "fixed PyroWave session — pinned at the session's bits per pixel, not the client rate"
            );
        }
        let pin = punktfunk_core::pyrowave::kbps_for(mode, chroma.is_444(), bit_depth, bpp)
            .clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
        // Open-loop pin can outrun the link. `PUNKTFUNK_PYROWAVE_MAX_MBPS` caps it;
        // unset ⇒ no cap.
        if let Some(ceiling) = ceiling {
            if pin > ceiling {
                tracing::warn!(
                    pin_kbps = pin,
                    ceiling_kbps = ceiling,
                    "PyroWave bitrate pin exceeds PUNKTFUNK_PYROWAVE_MAX_MBPS — capping to it"
                );
                return ceiling.max(MIN_BITRATE_KBPS);
            }
        }
        return pin;
    }
    resolve_bitrate_kbps(requested)
}

/// How a plane's video rides the wire, for [`encoder_bps_for_budget`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WireFraming {
    /// Native: sealed shards of `payload` bytes, priced at the budget's 64 B a shard and rounded
    /// down to the kbps ([`encoder_kbps_for_budget`]).
    Shards,
    /// Moonlight: a 32 B RTP and FEC header on each `payload` bytes, to the bit.
    #[cfg(feature = "gamestream")]
    Moonlight,
}

/// Encoder rate (bps) under a `budget_kbps` wire budget: `audio_kbps` comes off the top,
/// `framing` and `fec_percent` parity fit inside the rest. Floored at [`MIN_BITRATE_KBPS`].
pub(crate) fn encoder_bps_for_budget(
    budget_kbps: u32,
    audio_kbps: u32,
    fec_percent: u8,
    payload: usize,
    framing: WireFraming,
) -> u64 {
    match framing {
        WireFraming::Shards => {
            let payload = u16::try_from(payload).unwrap_or(u16::MAX);
            u64::from(encoder_kbps_for_budget(
                budget_kbps,
                audio_kbps,
                fec_percent,
                payload,
            )) * 1000
        }
        #[cfg(feature = "gamestream")]
        WireFraming::Moonlight => {
            let payload = payload.max(1) as u64;
            let video = u64::from(budget_kbps.saturating_sub(audio_kbps)) * 1000 * payload * 100
                / ((payload + 32) * (100 + u64::from(fec_percent)));
            video.max(u64::from(MIN_BITRATE_KBPS) * 1000)
        }
    }
}

/// Budget↔encoder at one moment: session constants plus a snapshot of adaptive FEC,
/// taken at each encoder touch. Stream loop re-derives when the live percent moves.
#[derive(Clone, Copy, Debug)]
pub(super) struct EncDerive {
    pub(super) audio_kbps: u32,
    pub(super) shard_payload: u16,
    pub(super) fec_percent: u8,
    /// PyroWave at this refresh rate: parity rides the frame's head only
    /// ([`pyrowave_parity_pct`]). `None`: every block carries `fec_percent`.
    pub(super) head_only_hz: Option<u32>,
    /// This encoder's share of the session's video, per mille: [`SHARE_ALL`] alone, less while
    /// further screens' encoders split the budget with it.
    pub(super) share_permille: u16,
}

/// [`EncDerive::share_permille`] of a session with one screen.
pub(super) const SHARE_ALL: u16 = 1000;

impl EncDerive {
    /// The share of the wire parity takes at `budget_kbps`, as the budget prices it.
    fn parity_pct(&self, budget_kbps: u32) -> u8 {
        match self.head_only_hz {
            None => self.fec_percent,
            Some(hz) => pyrowave_parity_pct(
                budget_kbps,
                self.audio_kbps,
                self.shard_payload,
                hz,
                self.fec_percent,
            ),
        }
    }

    /// [`encoder_bps_for_budget`] in kbps, over sealed shards: this encoder's share of it.
    pub(super) fn enc_kbps(&self, budget_kbps: u32) -> u32 {
        let bps = encoder_bps_for_budget(
            budget_kbps,
            self.audio_kbps,
            self.parity_pct(budget_kbps),
            self.shard_payload.into(),
            WireFraming::Shards,
        );
        let share = u64::from(self.share_permille.min(SHARE_ALL));
        ((bps / 1000) * share / u64::from(SHARE_ALL)).max(u64::from(MIN_BITRATE_KBPS)) as u32
    }

    /// The budget an encoder rate spends. Parity is priced at the budget the rate alone
    /// would need; it is a few percent, so that is the budget it lands in.
    pub(super) fn budget_kbps(&self, encoder_kbps: u32) -> u32 {
        let share = u64::from(self.share_permille.clamp(1, SHARE_ALL));
        let whole = (u64::from(encoder_kbps) * u64::from(SHARE_ALL) / share) as u32;
        let at = |pct| budget_kbps_for_encoder(whole, self.audio_kbps, pct, self.shard_payload);
        at(self.parity_pct(at(0)))
    }

    /// Read-back in the request's truncated terms. The roundtrip deflates, so a read-back
    /// that lost only truncation is the full ask. Only a genuine driver short-apply reports
    /// short.
    pub(super) fn applied_budget_kbps(
        &self,
        requested_budget_kbps: u32,
        applied_enc_kbps: u32,
    ) -> u32 {
        let b = self.budget_kbps(applied_enc_kbps);
        if b >= self.budget_kbps(self.enc_kbps(requested_budget_kbps)) {
            requested_budget_kbps
        } else {
            b
        }
    }
}

/// The share of a PyroWave frame its parity takes, as a percent of the frame: the head's parity
/// over the frame's wire shards. The head is sized at 15 % of the frame, the most level 4 measured
/// on game frames; its parity is `fec_percent` of that, never under
/// [`HEAD_MIN_RECOVERY_SHARDS`](punktfunk_core::packet::HEAD_MIN_RECOVERY_SHARDS).
fn pyrowave_parity_pct(
    budget_kbps: u32,
    audio_kbps: u32,
    shard_payload: u16,
    hz: u32,
    fec_percent: u8,
) -> u8 {
    if fec_percent == 0 {
        return 0;
    }
    let shards = punktfunk_core::abr::budget::wire_shards_per_frame(
        punktfunk_core::abr::budget::FrameBudget {
            budget_kbps,
            audio_kbps,
            shard_payload,
            fps: hz,
        },
    );
    let head = (shards * 15).div_ceil(100);
    let parity = (head * u32::from(fec_percent))
        .div_ceil(100)
        .max(punktfunk_core::packet::HEAD_MIN_RECOVERY_SHARDS as u32);
    (parity * 100).div_ceil(shards).min(100) as u8
}

/// Audio reservation from the resolved Welcome: PCM cost, the fixed AC-3 / E-AC-3 rate, else
/// the budget the audio thread runs ([`audio_budget`](super::handshake::audio_budget), the
/// operator's `audio.quality` included), with redundancy only when `HOST_CAP_AUDIO_RED` was
/// granted.
pub(super) fn audio_reserved_kbps(welcome: &punktfunk_core::quic::Welcome) -> u32 {
    use punktfunk_core::audio::bitstream;
    if bitstream::is_bitstream(welcome.audio_codec) {
        bitstream::bitrate_kbps(welcome.audio_codec, welcome.audio_channels)
    } else if welcome.audio_codec == punktfunk_core::quic::AUDIO_CODEC_PCM {
        punktfunk_core::audio::pcm::bitrate_kbps(
            welcome.audio_rate_hz,
            welcome.audio_bits,
            welcome.audio_channels,
        )
    } else {
        super::handshake::audio_budget(
            welcome.host_caps & punktfunk_core::quic::HOST_CAP_AUDIO_RED != 0,
            welcome.bitrate_kbps,
            welcome.audio_channels,
            punktfunk_core::audio::AudioLayout::from_wire(welcome.audio_layout).unwrap_or_default(),
        )
        .kbps
    }
}

/// A PyroWave session's `(floor, pin)` at `mode`, kbps: [`BPP_FLOOR`] and the session's `bpp`
/// ([`session_pyrowave_bpp`]) under its `ceiling`, the floor never over the pin. An Automatic
/// session adapts between them.
///
/// [`BPP_FLOOR`]: punktfunk_core::pyrowave::BPP_FLOOR
pub(super) fn pyrowave_bounds_kbps(
    mode: &punktfunk_core::config::Mode,
    chroma: crate::encode::ChromaFormat,
    bit_depth: u8,
    bpp: f64,
    ceiling: Option<u32>,
) -> (u32, u32) {
    let pin = resolve_bitrate_kbps_for(
        crate::encode::Codec::PyroWave,
        0,
        mode,
        chroma,
        bit_depth,
        bpp,
        ceiling,
    );
    let floor = punktfunk_core::pyrowave::kbps_for(
        mode,
        chroma.is_444(),
        bit_depth,
        punktfunk_core::pyrowave::BPP_FLOOR,
    )
    .clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS);
    (floor.min(pin), pin)
}

/// What a PyroWave session encodes at `mode`: the pin at the session's bits per pixel
/// ([`SessionPlan::pyrowave_bpp`](crate::session_plan::SessionPlan::pyrowave_bpp)) under the
/// session's `ceiling`, or an Automatic session's `running` rate held inside that mode's bounds.
pub(super) fn pyrowave_mode_kbps(
    running: Option<u32>,
    mode: &punktfunk_core::config::Mode,
    plan: &crate::session_plan::SessionPlan,
    ceiling: Option<u32>,
) -> u32 {
    let (floor, pin) = pyrowave_bounds_kbps(
        mode,
        plan.chroma,
        plan.bit_depth,
        plan.pyrowave_bpp,
        ceiling,
    );
    running.map_or(pin, |kbps| kbps.clamp(floor, pin))
}

/// Whether this source adapts FEC: only sources that can keep encoder and packetizer
/// FEC in one wire budget. Synthetic-abr derives frame bytes from FEC every frame;
/// the virtual path publishes a proposal only after its encoder accepts the matching
/// rate. Fixed synthetic and the standalone software source have no retarget path.
pub(super) fn adaptive_fec_for(source: Punktfunk1Source, static_override: bool) -> bool {
    !static_override
        && matches!(
            source,
            Punktfunk1Source::SyntheticAbr(_) | Punktfunk1Source::Virtual
        )
}

/// Consecutive report windows an RFI ask landed in — frames parity could not repair. The
/// client sends no report for a window it discards (probe tail, host pipeline gap),
/// so a report a window late says the asks before it belong to a window nobody may price.
#[derive(Default)]
pub(super) struct UnrecoveredRun {
    asked: bool,
    last_report: Option<std::time::Instant>,
    run: u32,
}

impl UnrecoveredRun {
    pub(super) fn rfi(&mut self) {
        self.asked = true;
    }

    /// Close the window this report ends; returns the run it leaves.
    pub(super) fn report(&mut self, now: std::time::Instant) -> u32 {
        let late = punktfunk_core::abr::WINDOW * 3 / 2;
        let discarded = self
            .last_report
            .is_some_and(|t| now.duration_since(t) > late);
        self.last_report = Some(now);
        self.run = if std::mem::take(&mut self.asked) && !discarded {
            self.run.saturating_add(1)
        } else {
            0
        };
        self.run
    }
}

/// Per-frame send path: apply the adaptive-FEC target if it changed (relaxed load + compare).
pub(super) fn apply_fec_target(session: &mut Session, fec_target: &AtomicU8) {
    let t = fec_target.load(Ordering::Relaxed);
    if session.fec_percent() != t {
        session.set_fec_percent(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::{Content, KeyframeAnswer, SynthAbrShape, DEFAULT_IDR_PCT};

    /// Adaptive FEC is offered only to a source that can keep encoder and packetizer
    /// on one wire budget: a proposal a source cannot apply would be accepted work
    /// that never reaches the wire.
    #[test]
    fn adaptive_fec_only_for_sources_with_a_coordinated_retarget() {
        let abr = Punktfunk1Source::SyntheticAbr(SynthAbrShape {
            content: Content::Steady { fill_pct: 100 },
            recovery: std::time::Duration::ZERO,
            answer: KeyframeAnswer::Idr,
            idr_pct: DEFAULT_IDR_PCT,
            bringup: std::time::Duration::ZERO,
            serve_ramp: false,
        });
        for (source, want) in [
            (Punktfunk1Source::Synthetic, false),
            (Punktfunk1Source::Software, false),
            (abr, true),
            (Punktfunk1Source::Virtual, true),
        ] {
            assert_eq!(
                adaptive_fec_for(source, false),
                want,
                "static override unset: {source:?}"
            );
            assert!(
                !adaptive_fec_for(source, true),
                "a pinned FEC adapts nothing: {source:?}"
            );
        }
    }

    /// A screen at a quarter of the video encodes a quarter of the whole's rate, floored at
    /// the minimum, and its full read-back is the session's budget.
    #[test]
    fn a_share_of_the_video_is_that_share_of_the_encoder_rate() {
        let whole = EncDerive {
            audio_kbps: 512,
            shard_payload: 1408,
            fec_percent: 10,
            head_only_hz: None,
            share_permille: SHARE_ALL,
        };
        let quarter = EncDerive {
            share_permille: 250,
            ..whole
        };
        let budget = 40_000;
        assert_eq!(quarter.enc_kbps(budget), whole.enc_kbps(budget) / 4);
        assert_eq!(quarter.enc_kbps(1), MIN_BITRATE_KBPS);
        let asked = quarter.enc_kbps(budget);
        assert_eq!(quarter.applied_budget_kbps(budget, asked), budget);
    }

    #[test]
    fn full_apply_readback_is_the_request_not_the_deflated_roundtrip() {
        // A roundtrip that lost only truncation must report the full request, not a phantom ceiling.
        let ed = EncDerive {
            audio_kbps: 576,
            shard_payload: 1408,
            fec_percent: 8,
            head_only_hz: None,
            share_permille: SHARE_ALL,
        };
        for budget in [2349u32, 4799, 6857, 9798, 14000, 20000, 940_032] {
            let asked_enc = ed.enc_kbps(budget);
            assert!(
                ed.budget_kbps(asked_enc) <= budget,
                "roundtrip must not inflate"
            );
            assert_eq!(ed.applied_budget_kbps(budget, asked_enc), budget);
        }
        // A genuine driver short-apply still reports short.
        let asked_enc = ed.enc_kbps(1_010_000);
        let short = ed.applied_budget_kbps(1_010_000, asked_enc * 3 / 4);
        assert!(short < ed.budget_kbps(ed.enc_kbps(1_010_000)));
    }

    /// One short apply is a ceiling, not a life sentence: it holds for its wait,
    /// then the next ask reaches the encoder. A full apply there drops it; a
    /// short one puts it back with twice the wait.
    #[test]
    fn a_learned_encoder_ceiling_is_re_tested_and_backs_off() {
        let mut c = EncoderCeiling::new();
        assert_eq!(c.resolve(400_000), (400_000, AckReason::Granted));
        c.note_applied(400_000, 300_000);
        let first_wait = c.wait();
        // Under the ceiling nothing is refused; above it, the ack says why.
        assert_eq!(c.resolve(200_000), (200_000, AckReason::Granted));
        assert_eq!(c.resolve(400_000), (300_000, AckReason::EncoderLimit));
        // Past the wait, one ask reaches the encoder.
        c.spend_the_wait();
        assert_eq!(c.resolve(400_000), (400_000, AckReason::Granted));
        // Still there: re-learned, and the next wait is twice as long.
        c.note_applied(400_000, 300_000);
        assert_eq!(c.wait(), first_wait * 2);
        assert_eq!(c.resolve(400_000), (300_000, AckReason::EncoderLimit));
        // Gone: the ceiling goes with it, and nothing is clamped again.
        c.spend_the_wait();
        assert_eq!(c.resolve(400_000), (400_000, AckReason::Granted));
        c.note_applied(400_000, 400_000);
        assert_eq!(c.resolve(8_000_000), (8_000_000, AckReason::Granted));
    }

    /// A rate under the ceiling, taken whole, says nothing about the ceiling: an ABR dip
    /// keeps it and its back-off clock.
    #[test]
    fn a_full_apply_under_the_ceiling_keeps_it() {
        let mut c = EncoderCeiling::new();
        c.note_applied(400_000, 300_000);
        let wait = c.wait();
        assert_eq!(c.resolve(250_000), (250_000, AckReason::Granted));
        c.note_applied(250_000, 250_000);
        assert_eq!(c.resolve(400_000), (300_000, AckReason::EncoderLimit));
        assert_eq!(c.wait(), wait);
    }

    /// The encoder that refused a rate is gone (a mode switch, a rebuild on a
    /// new source), and so is what it taught.
    #[test]
    fn a_rebuilt_encoder_starts_with_no_ceiling() {
        let mut c = EncoderCeiling::new();
        c.note_applied(400_000, 300_000);
        assert_eq!(c.resolve(400_000), (300_000, AckReason::EncoderLimit));
        c.clear();
        assert_eq!(c.resolve(400_000), (400_000, AckReason::Granted));
        // And the clock starts over rather than carrying the old backoff.
        c.note_applied(400_000, 300_000);
        assert_eq!(c.wait(), punktfunk_core::abr::WINDOW * 16);
    }

    #[test]
    fn pyrowave_bitrate_pins_to_bpp_default() {
        use punktfunk_core::config::Mode;
        let mode = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        use crate::encode::{ChromaFormat, Codec};
        // A client that sends no quality gets the host's row: ~1.6 bpp, not 20 Mbps.
        let host = session_pyrowave_bpp(0);
        let at = |codec, requested, chroma, depth| {
            resolve_bitrate_kbps_for(codec, requested, &mode, chroma, depth, host, None)
        };
        let kbps = at(Codec::PyroWave, 0, ChromaFormat::Yuv420, 8);
        assert_eq!(kbps, 1920 * 1080 * 60 * 16 / 10 / 1000);
        // 4:4:4 ≈ 2.6 bpp; 10-bit adds 15 %. `design/pyrowave-444-hdr.md`.
        assert_eq!(
            at(Codec::PyroWave, 0, ChromaFormat::Yuv444, 8),
            1920 * 1080 * 60 * 26 / 10 / 1000
        );
        assert_eq!(
            at(Codec::PyroWave, 0, ChromaFormat::Yuv444, 10),
            (1920u64 * 1080 * 60 * 26 / 10 * 115 / 100 / 1000) as u32
        );
        // A client rate is ignored; the session's bits per pixel sets the pin.
        assert_eq!(
            at(Codec::PyroWave, 130_000, ChromaFormat::Yuv420, 8),
            1920 * 1080 * 60 * 16 / 10 / 1000
        );
        // H.26x codecs keep the 20 Mbps default.
        assert_eq!(
            at(Codec::H265, 0, ChromaFormat::Yuv420, 8),
            DEFAULT_BITRATE_KBPS
        );
    }

    /// A client's quality sizes the pin, held inside `[0.5, 2.0]` bits per pixel and under the
    /// operator's ceiling. A client that sends none gets the host's row.
    #[test]
    fn a_client_quality_sizes_the_pyrowave_pin() {
        use crate::encode::{ChromaFormat, Codec};
        let fhd = punktfunk_core::config::Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        assert_eq!(
            session_pyrowave_bpp(0),
            pf_host_config::config().pyrowave_bpp
        );
        assert_eq!(session_pyrowave_bpp(120), 1.2);
        assert_eq!(session_pyrowave_bpp(10), 0.5);
        assert_eq!(session_pyrowave_bpp(900), 2.0);
        let none = None;
        let link = Some(200_000);
        let pin = |x100, ceiling| {
            resolve_bitrate_kbps_for(
                Codec::PyroWave,
                0,
                &fhd,
                ChromaFormat::Yuv420,
                8,
                session_pyrowave_bpp(x100),
                ceiling,
            )
        };
        let px = 1920 * 1080 * 60;
        assert_eq!(pin(120, none), px * 12 / 10 / 1000);
        assert_eq!(
            pin(900, none),
            px * 2 / 1000,
            "asked past 2.0, pinned at it"
        );
        assert_eq!(
            pin(900, link),
            200_000,
            "the operator's ceiling still binds"
        );
    }

    /// A mode switch re-pins at the session's quality, not the host's row.
    #[test]
    fn a_mode_switch_keeps_the_session_quality() {
        use crate::encode::{ChromaFormat, Codec};
        let mut plan = crate::session_plan::SessionPlan::resolve(
            &crate::encode::resolve(),
            8,
            false,
            ChromaFormat::Yuv420,
            Codec::PyroWave,
            false,
            false,
            crate::session_plan::DEFAULT_SLICES,
        );
        plan.pyrowave_bpp = session_pyrowave_bpp(100);
        let uhd = punktfunk_core::config::Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 60,
        };
        assert_eq!(
            pyrowave_mode_kbps(None, &uhd, &plan, None),
            3840 * 2160 * 60 / 1000
        );
    }

    /// The bits-per-pixel rule is core's (`punktfunk_core::pyrowave`); the host bounds it.
    #[test]
    fn a_tiny_pyrowave_pin_lands_on_the_rate_floor() {
        let tiny = punktfunk_core::config::Mode {
            width: 64,
            height: 64,
            refresh_hz: 1,
        };
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::PyroWave,
                0,
                &tiny,
                crate::encode::ChromaFormat::Yuv420,
                8,
                session_pyrowave_bpp(0),
                None,
            ),
            MIN_BITRATE_KBPS
        );
    }

    #[test]
    fn pyrowave_auto_pin_respects_operator_ceiling() {
        use crate::encode::{ChromaFormat, Codec};
        use punktfunk_core::config::Mode;
        // 5120×1440@240 4:4:4 10-bit pins above a 5 GbE link.
        let mode = Mode {
            width: 5120,
            height: 1440,
            refresh_hz: 240,
        };
        let host = session_pyrowave_bpp(0);
        let pin = |requested, mode: &Mode, chroma, depth, ceiling: Option<u32>| {
            resolve_bitrate_kbps_for(
                Codec::PyroWave,
                requested,
                mode,
                chroma,
                depth,
                host,
                ceiling,
            )
        };
        let none = None;
        let link = Some(4_500_000);
        let uncapped = pin(0, &mode, ChromaFormat::Yuv444, 10, none);
        assert!(
            uncapped > 5_000_000,
            "expected the open-loop pin, got {uncapped}"
        );
        // Ceiling caps the Automatic pin to the link rate.
        assert_eq!(pin(0, &mode, ChromaFormat::Yuv444, 10, link), 4_500_000);
        // A pin already under the ceiling is untouched.
        let small = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        assert_eq!(
            pin(0, &small, ChromaFormat::Yuv420, 8, link),
            1920 * 1080 * 60 * 16 / 10 / 1000
        );
        // Explicit client rate still goes through pin + ceiling.
        assert_eq!(
            pin(6_000_000, &mode, ChromaFormat::Yuv444, 10, link),
            4_500_000
        );
    }

    /// Automatic PyroWave adapts between 0.5 bpp and the session's pin for the mode it
    /// encodes. The operator's ceiling binds the pin and, under it, the floor too.
    #[test]
    fn pyrowave_bounds_follow_the_mode_under_the_operator_ceiling() {
        use crate::encode::{ChromaFormat, Codec};
        use punktfunk_core::config::Mode;
        let host = session_pyrowave_bpp(0);
        let bounds = |mode: &Mode, ceiling| {
            pyrowave_bounds_kbps(mode, ChromaFormat::Yuv420, 8, host, ceiling)
        };
        let none = None;
        let tight = Some(300_000);
        let uhd = Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 120,
        };
        let fhd = Mode {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
        };
        let px = 3840 * 2160 * 120;
        assert_eq!(bounds(&uhd, none), (px / 2 / 1000, px / 1000 * 16 / 10));
        assert_eq!(
            bounds(&fhd, none),
            (px / 8 / 2 / 1000, px / 8 * 16 / 10 / 1000),
            "an eighth of the pixels, an eighth of both"
        );
        assert_eq!(
            bounds(&uhd, tight),
            (300_000, 300_000),
            "a ceiling under the floor takes both"
        );
        // A rebuild holds an Automatic rate inside the new mode's bounds.
        let (floor, pin) = bounds(&fhd, none);
        let mut plan = crate::session_plan::SessionPlan::resolve(
            &crate::encode::resolve(),
            8,
            false,
            ChromaFormat::Yuv420,
            Codec::PyroWave,
            false,
            false,
            crate::session_plan::DEFAULT_SLICES,
        );
        plan.pyrowave_bpp = host;
        let at = |running| pyrowave_mode_kbps(running, &fhd, &plan, None);
        assert_eq!(at(None), pin, "an explicit session takes the pin");
        assert_eq!(at(Some(1_000)), floor);
        assert_eq!(at(Some(u32::MAX)), pin);
        assert_eq!(at(Some(floor + 1)), floor + 1);
    }

    /// Native prices sealed shards at 64 B with audio off the top, to the kbps; GameStream
    /// prices Moonlight's 32 B header with no audio in the budget, to the bit. Both floor at
    /// the minimum encoder rate.
    #[test]
    fn both_loops_derive_the_encoder_rate_under_their_wire_budget() {
        let native = EncDerive {
            audio_kbps: 512,
            shard_payload: 1408,
            fec_percent: 10,
            head_only_hz: None,
            share_permille: SHARE_ALL,
        };
        let bps = encoder_bps_for_budget(20_000, 512, 10, 1408, WireFraming::Shards);
        assert_eq!(
            bps,
            u64::from(encoder_kbps_for_budget(20_000, 512, 10, 1408)) * 1000
        );
        assert_eq!(u64::from(native.enc_kbps(20_000)) * 1000, bps);
        assert_eq!(
            encoder_bps_for_budget(0, 512, 10, 1408, WireFraming::Shards),
            500_000
        );
        #[cfg(feature = "gamestream")]
        {
            // 20 Mbps, 20 % FEC, packetSize 1392: 20e6 × 1376/1408 × 100/120.
            let moonlight =
                |kbps, fec| encoder_bps_for_budget(kbps, 0, fec, 1376, WireFraming::Moonlight);
            assert_eq!(moonlight(20_000, 20), 16_287_878);
            assert_eq!(moonlight(20_000, 0), 20_000_000 * 1376 / 1408);
            assert_eq!(moonlight(0, 20), 500_000);
        }
    }

    /// PyroWave's pin is its wire budget: the encoder gets it less seal, audio and the head's
    /// parity, a few percent where a percent on every block took 5–10. The read-back of a full
    /// apply is the ask.
    #[test]
    fn a_pyrowave_pin_prices_only_its_head_parity() {
        let at = |fec_percent, hz| EncDerive {
            audio_kbps: 512,
            shard_payload: 1408,
            fec_percent,
            head_only_hz: Some(hz),
            share_permille: SHARE_ALL,
        };
        // 4K60 at 1.6 bpp: 1126 wire shards a frame, a 169-shard head, 9 parity at 5 %.
        let pin = 796_262;
        let enc = at(5, 60).enc_kbps(pin);
        let seal_only = u64::from(pin - 512) * 1408 / 1472;
        assert!(enc as u64 * 100 >= seal_only * 99, "{enc} vs {seal_only}");
        assert!(at(10, 60).enc_kbps(pin) < enc, "more parity, less encoder");
        assert!(at(0, 60).enc_kbps(pin) as u64 >= seal_only);
        // 720p60 at 1.6 bpp is 124 shards: the four-shard floor prices as 4 %.
        assert_eq!(pyrowave_parity_pct(88_473, 512, 1408, 60, 5), 4);
        assert_eq!(pyrowave_parity_pct(pin, 512, 1408, 60, 5), 1);
        assert_eq!(pyrowave_parity_pct(pin, 512, 1408, 60, 50), 8);
        for (fec, hz) in [(5, 60), (10, 120), (25, 60)] {
            let ed = at(fec, hz);
            let asked = ed.enc_kbps(pin);
            assert!(ed.budget_kbps(asked) <= pin, "a roundtrip never inflates");
            assert_eq!(ed.applied_budget_kbps(pin, asked), pin);
        }
    }

    /// An RFI ask prices the report window it lands in. A report a window late closes a
    /// window the client discarded on purpose (probe tail, pipeline gap), so the asks before
    /// it must not leak into the clean window that follows.
    #[test]
    fn an_rfi_from_a_discarded_window_does_not_price_the_next_report() {
        let w = punktfunk_core::abr::WINDOW;
        let t0 = std::time::Instant::now();
        let mut run = UnrecoveredRun::default();
        assert_eq!(run.report(t0), 0);
        run.rfi();
        assert_eq!(run.report(t0 + w), 1);
        run.rfi();
        run.rfi();
        assert_eq!(run.report(t0 + w * 2), 2, "several asks are one window");
        assert_eq!(run.report(t0 + w * 3), 0, "a clean window ends the run");
        // An ask, then a discarded window: the report lands two windows after the last.
        run.rfi();
        assert_eq!(run.report(t0 + w * 5), 0);
        run.rfi();
        assert_eq!(
            run.report(t0 + w * 6),
            1,
            "the next on-time window counts again"
        );
    }
}
