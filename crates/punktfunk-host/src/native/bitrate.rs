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
    /// taking the whole ask is the ceiling gone.
    pub(super) fn note_applied(&mut self, want: u32, applied: u32) {
        if applied >= want {
            if self.cap.kbps().is_some() {
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

/// PyroWave pins the host's bits per pixel (row `pyrowave_bpp`) for the negotiated mode, not
/// the 20 Mbps H.26x default. ABR stays off; mid-stream retargets are refused. A client rate
/// is ignored: bits per pixel is the quality knob, and it holds across modes. Every pin goes
/// through `PUNKTFUNK_PYROWAVE_MAX_MBPS`. H.26x/AV1 explicit rates stand.
pub(super) fn resolve_bitrate_kbps_for(
    codec: crate::encode::Codec,
    requested: u32,
    mode: &punktfunk_core::config::Mode,
    chroma: crate::encode::ChromaFormat,
    bit_depth: u8,
) -> u32 {
    resolve_bitrate_kbps_under(
        codec,
        requested,
        mode,
        chroma,
        bit_depth,
        pyrowave_auto_pin_ceiling_kbps,
    )
}

/// [`resolve_bitrate_kbps_for`] with the PyroWave ceiling (kbps) read through `ceiling`, so a
/// test hands one in without writing the process environment.
fn resolve_bitrate_kbps_under(
    codec: crate::encode::Codec,
    requested: u32,
    mode: &punktfunk_core::config::Mode,
    chroma: crate::encode::ChromaFormat,
    bit_depth: u8,
    ceiling: fn() -> Option<u32>,
) -> u32 {
    if codec == crate::encode::Codec::PyroWave {
        if requested != 0 {
            tracing::warn!(
                requested_kbps = requested,
                "a client bitrate does not apply to PyroWave — using the host's bits per pixel"
            );
        }
        let bpp = pf_host_config::config().pyrowave_bpp;
        let pin = pyrowave_pin_kbps(mode, chroma, bit_depth, bpp);
        // Open-loop pin can outrun the link. `PUNKTFUNK_PYROWAVE_MAX_MBPS` caps it;
        // unset ⇒ no cap.
        if let Some(ceiling) = ceiling() {
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

/// Budget↔encoder at one moment: session constants plus a snapshot of adaptive FEC,
/// taken at each encoder touch. Stream loop re-derives when the live percent moves.
#[derive(Clone, Copy, Debug)]
pub(super) struct EncDerive {
    pub(super) audio_kbps: u32,
    pub(super) shard_payload: u16,
    pub(super) fec_percent: u8,
    /// PyroWave: pin is an encoder rate; both directions are identity.
    pub(super) identity: bool,
}

impl EncDerive {
    pub(super) fn enc_kbps(&self, budget_kbps: u32) -> u32 {
        if self.identity {
            budget_kbps
        } else {
            encoder_kbps_for_budget(
                budget_kbps,
                self.audio_kbps,
                self.fec_percent,
                self.shard_payload,
            )
        }
    }

    pub(super) fn budget_kbps(&self, encoder_kbps: u32) -> u32 {
        if self.identity {
            encoder_kbps
        } else {
            budget_kbps_for_encoder(
                encoder_kbps,
                self.audio_kbps,
                self.fec_percent,
                self.shard_payload,
            )
        }
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

/// Audio reservation from the resolved Welcome: PCM cost, else the same
/// [`plan_audio_budget`](punktfunk_core::audio::plan_audio_budget) rung the audio thread
/// runs, with redundancy only when `HOST_CAP_AUDIO_RED` was granted.
pub(super) fn audio_reserved_kbps(welcome: &punktfunk_core::quic::Welcome) -> u32 {
    if welcome.audio_codec == punktfunk_core::quic::AUDIO_CODEC_PCM {
        punktfunk_core::audio::pcm::bitrate_kbps(
            welcome.audio_rate_hz,
            welcome.audio_bits,
            welcome.audio_channels,
        )
    } else {
        punktfunk_core::audio::plan_audio_budget(
            welcome.bitrate_kbps,
            welcome.audio_channels,
            punktfunk_core::audio::AudioLayout::from_wire(welcome.audio_layout).unwrap_or_default(),
            punktfunk_core::audio::AudioTier::default(),
            welcome.host_caps & punktfunk_core::quic::HOST_CAP_AUDIO_RED != 0,
        )
        .kbps
    }
}

/// `bpp` bits per pixel for a 4:2:0 SDR frame. 4:4:4 carries twice the samples but costs
/// ×1.625, since chroma compresses better than luma; 10-bit planes add 15 %.
fn pyrowave_pin_kbps(
    mode: &punktfunk_core::config::Mode,
    chroma: crate::encode::ChromaFormat,
    bit_depth: u8,
    bpp: f64,
) -> u32 {
    let mut bpp = bpp;
    if chroma.is_444() {
        bpp *= 1.625;
    }
    if bit_depth >= 10 {
        bpp *= 1.15;
    }
    let px_per_s =
        f64::from(mode.width) * f64::from(mode.height) * f64::from(mode.refresh_hz.max(1));
    // `as` saturates, so a huge mode lands on the clamp.
    ((px_per_s * bpp / 1000.0) as u32).clamp(MIN_BITRATE_KBPS, MAX_BITRATE_KBPS)
}

/// `PUNKTFUNK_PYROWAVE_MAX_MBPS` (Mb/s) → kbps. `None` when unset/zero/invalid (no cap).
/// Every PyroWave session, including an explicit client rate, goes through the pin.
fn pyrowave_auto_pin_ceiling_kbps() -> Option<u32> {
    pf_host_config::knob("PUNKTFUNK_PYROWAVE_MAX_MBPS")
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|&m| m > 0)
        .map(|m| m.saturating_mul(1000))
}

/// `PUNKTFUNK_FEC_PCT` pins recovery and disables adaptive FEC. `None` ⇒ adaptive. `0`
/// disables FEC. Clamped to ≤ 90.
pub(super) fn fec_static_override() -> Option<u8> {
    std::env::var("PUNKTFUNK_FEC_PCT")
        .ok()
        .and_then(|s| s.trim().parse::<u8>().ok())
        .map(|p| p.min(90))
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
        let late = punktfunk_core::client::ADAPT_REPORT_INTERVAL * 3 / 2;
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

    #[test]
    fn full_apply_readback_is_the_request_not_the_deflated_roundtrip() {
        // A roundtrip that lost only truncation must report the full request, not a phantom ceiling.
        let ed = EncDerive {
            audio_kbps: 576,
            shard_payload: 1408,
            fec_percent: 8,
            identity: false,
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
        use crate::encode::ChromaFormat;
        // Automatic PyroWave → ~1.6 bpp, not the 20 Mbps H.26x default.
        let kbps = resolve_bitrate_kbps_for(
            crate::encode::Codec::PyroWave,
            0,
            &mode,
            ChromaFormat::Yuv420,
            8,
        );
        assert_eq!(kbps, 1920 * 1080 * 60 * 16 / 10 / 1000);
        // 4:4:4 ≈ 2.6 bpp; 10-bit adds 15 %. `design/pyrowave-444-hdr.md`.
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::PyroWave,
                0,
                &mode,
                ChromaFormat::Yuv444,
                8
            ),
            1920 * 1080 * 60 * 26 / 10 / 1000
        );
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::PyroWave,
                0,
                &mode,
                ChromaFormat::Yuv444,
                10
            ),
            (1920u64 * 1080 * 60 * 26 / 10 * 115 / 100 / 1000) as u32
        );
        // A client rate is ignored; the host's bits per pixel sets the pin.
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::PyroWave,
                130_000,
                &mode,
                ChromaFormat::Yuv420,
                8
            ),
            1920 * 1080 * 60 * 16 / 10 / 1000
        );
        // H.26x codecs keep the 20 Mbps default.
        assert_eq!(
            resolve_bitrate_kbps_for(
                crate::encode::Codec::H265,
                0,
                &mode,
                ChromaFormat::Yuv420,
                8
            ),
            DEFAULT_BITRATE_KBPS
        );
    }

    #[test]
    fn pyrowave_pin_follows_the_host_bpp() {
        use crate::encode::ChromaFormat;
        use punktfunk_core::config::Mode;
        let mode = Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 120,
        };
        let px = 3840 * 2160 * 120;
        // 0.5 bpp is Steam's 500 Mbps ceiling at 4K120.
        assert_eq!(
            pyrowave_pin_kbps(&mode, ChromaFormat::Yuv420, 8, 0.5),
            px / 2 / 1000
        );
        // 4:4:4 and 10-bit scale from the operator's value, not from 1.6.
        assert_eq!(
            pyrowave_pin_kbps(&mode, ChromaFormat::Yuv444, 10, 1.0),
            (f64::from(px) * 1.625 * 1.15 / 1000.0) as u32
        );
        let tiny = Mode {
            width: 64,
            height: 64,
            refresh_hz: 1,
        };
        assert_eq!(
            pyrowave_pin_kbps(&tiny, ChromaFormat::Yuv420, 8, 0.25),
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
        let pin = |requested, mode: &Mode, chroma, depth, ceiling: fn() -> Option<u32>| {
            resolve_bitrate_kbps_under(Codec::PyroWave, requested, mode, chroma, depth, ceiling)
        };
        fn none() -> Option<u32> {
            None
        }
        fn link() -> Option<u32> {
            Some(4_500_000)
        }
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

    /// An RFI ask prices the report window it lands in. A report a window late closes a
    /// window the client discarded on purpose (probe tail, pipeline gap), so the asks before
    /// it must not leak into the clean window that follows.
    #[test]
    fn an_rfi_from_a_discarded_window_does_not_price_the_next_report() {
        let w = punktfunk_core::client::ADAPT_REPORT_INTERVAL;
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
