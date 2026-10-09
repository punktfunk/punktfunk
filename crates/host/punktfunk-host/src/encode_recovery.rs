//! Encoder and capture recovery policy both encode loops share: the silent-wedge watch with
//! its in-place reset budget, the submit-error and ladder-rung hooks around it, and the time a
//! capture loss may spend rebuilding.

use anyhow::Context as _;
use std::time::{Duration, Instant};

/// In-place encoder rebuilds and capture-loss rebuilds before a session ends.
pub(crate) const MAX_ENCODER_RESETS: u32 = 5;
pub(crate) const MAX_CAPTURE_REBUILDS: u32 = 5;

/// Non-blocking poll returning None forever while submits succeed. 2 s also sizes the backlog bound.
const ENCODE_STALL_WINDOW: Duration = Duration::from_secs(2);

/// Frames are owed and no AU came for the stall window, or more are owed than that window can
/// explain past the pipeline `depth`. The window stretches to eight intervals so a low frame
/// rate cannot false-trip.
fn encode_stalled(inflight: usize, since_au: Duration, depth: usize, interval: Duration) -> bool {
    let window = ENCODE_STALL_WINDOW.max(interval * 8);
    let backlog = depth + (window.as_secs_f64() / interval.as_secs_f64().max(1e-6)).ceil() as usize;
    inflight > 0 && (since_au >= window || inflight > backlog)
}

/// One encode loop's stall watch and in-place reset budget. Poll returning `None` forever
/// never errors, so the last AU's time is the only signal.
pub(crate) struct EncoderWatchdog {
    resets: u32,
    last_au_at: Instant,
}

impl EncoderWatchdog {
    pub(crate) fn new() -> EncoderWatchdog {
        EncoderWatchdog {
            resets: 0,
            last_au_at: Instant::now(),
        }
    }

    /// An AU arrived, or a fresh encoder opened: restart the watch and refill the budget.
    pub(crate) fn on_au(&mut self) {
        self.resets = 0;
        self.last_au_at = Instant::now();
    }

    /// Restart the watch without refilling the budget: nothing is owed yet.
    pub(crate) fn restart(&mut self) {
        self.last_au_at = Instant::now();
    }

    /// Why the loop should reset its encoder, or `None` while AUs keep coming.
    pub(crate) fn stalled(
        &self,
        inflight: usize,
        depth: usize,
        interval: Duration,
    ) -> Option<String> {
        let since = self.last_au_at.elapsed();
        encode_stalled(inflight, since, depth, interval).then(|| {
            format!(
                "no AU for {} ms with {inflight} frame(s) in flight",
                since.as_millis()
            )
        })
    }

    /// Spend one reset. `None` once [`MAX_ENCODER_RESETS`] are spent, else the backoff:
    /// 100 ms doubling to 1.6 s, never under one frame. Instant retries would burn all five
    /// inside one driver hiccup.
    pub(crate) fn spend(&mut self, interval: Duration) -> Option<Duration> {
        self.resets += 1;
        (self.resets <= MAX_ENCODER_RESETS)
            .then(|| interval.max(Duration::from_millis(100u64 << (self.resets - 1).min(4))))
    }

    /// [`Self::spend`], then `reset` the encoder in place. `None` when the budget is spent or
    /// the encoder has no in-place reset; on success the watch restarts.
    pub(crate) fn recover(
        &mut self,
        interval: Duration,
        reset: impl FnOnce() -> bool,
    ) -> Option<Duration> {
        let backoff = self.spend(interval).filter(|_| reset())?;
        self.restart();
        Some(backoff)
    }

    /// Resets spent since the last AU.
    pub(crate) fn resets(&self) -> u32 {
        self.resets
    }
}

/// Rebuild the encoder in place, drop what it owed (`on_reset`) and ask for an IDR.
/// `false` = no in-place reset.
pub(crate) fn reset_stalled_encoder(
    enc: &mut dyn crate::encode::Encoder,
    on_reset: impl FnOnce(),
) -> bool {
    if !enc.reset() {
        return false;
    }
    on_reset();
    enc.request_keyframe();
    true
}

/// A failed submit. A [`crate::encode::TerminalEncoderError`] ends the session at once: a
/// rebuild cannot fix a configuration. Anything else spends one in-place `reset` and returns
/// its backoff; `Err` once the budget is spent.
pub(crate) fn on_submit_error(
    watchdog: &mut EncoderWatchdog,
    e: anyhow::Error,
    interval: Duration,
    reset: impl FnOnce() -> bool,
) -> anyhow::Result<Duration> {
    if e.downcast_ref::<crate::encode::TerminalEncoderError>()
        .is_some()
    {
        tracing::error!(
            error = %format!("{e:#}"),
            "encoder failed with a deterministic configuration error — ending the video \
             session without rebuild attempts (see the error for the remedy)");
        return Err(e).context("encoder submit");
    }
    let Some(backoff) = watchdog.recover(interval, reset) else {
        tracing::error!(
            error = %format!("{e:#}"),
            resets = watchdog.resets(),
            "encoder did not recover after repeated in-place rebuilds — ending the video \
             session (see the error above for the cause)");
        return Err(e).context("encoder submit");
    };
    tracing::warn!(error = %format!("{e:#}"), reset = watchdog.resets(),
        max = MAX_ENCODER_RESETS,
        "encoder submit failed — encoder rebuilt in place, forcing an IDR");
    Ok(backoff)
}

/// Run the rung the capturer's ladder parked during this tick's `try_latest` and hand its
/// outcome straight back. Call it right after the grab, before the submit: the rung owns the
/// encoder this tick would feed. `on_reset` drops what a reset forfeited.
pub(crate) fn run_parked_stage(
    capturer: &mut dyn crate::capture::Capturer,
    enc: &mut dyn crate::encode::Encoder,
    watchdog: &mut EncoderWatchdog,
    on_reset: impl FnOnce(),
) {
    if let Some(stage) = capturer.take_pending_stage() {
        let outcome = run_loop_stage(stage, enc, on_reset);
        capturer.stage_done(stage, outcome);
        watchdog.restart();
    }
}

/// The ladder rungs whose actuator the loop owns because the encoder or the display manager
/// does. `EncoderReset` is [`reset_stalled_encoder`] plus a bounded wait for the first access
/// unit — the rung's whole cost, one IDR included. `DriverCycle` reaps the WUDFHost and reloads
/// the adapter (seconds, the display black for the cycle); the capturer then ends, and the
/// loop's capture-loss path rebuilds against the fresh host or ends the stream.
fn run_loop_stage(
    stage: pf_frame::recovery::Stage,
    enc: &mut dyn crate::encode::Encoder,
    on_reset: impl FnOnce(),
) -> pf_frame::recovery::StageOutcome {
    use pf_frame::recovery::{Stage, StageOutcome, ENCODER_RESET_FIRST_AU};
    match stage {
        Stage::EncoderReset => {
            let t0 = Instant::now();
            if !reset_stalled_encoder(enc, on_reset) {
                return StageOutcome::Failed;
            }
            let first_au = enc.ready_aus(t0 + ENCODER_RESET_FIRST_AU).map(|n| n > 0);
            tracing::warn!(
                cost_ms = t0.elapsed().as_millis() as u64,
                first_au,
                "recovery: encoder reset applied — one IDR plus the first-AU wait"
            );
            StageOutcome::Applied
        }
        #[cfg(target_os = "windows")]
        Stage::DriverCycle => {
            let t0 = Instant::now();
            match crate::vdisplay::driver::force_driver_cycle() {
                Ok(()) => {
                    tracing::warn!(
                        cost_ms = t0.elapsed().as_millis() as u64,
                        "recovery: driver cycle — adapter reloaded, display black for the cycle; \
                         the session rebuilds against the fresh WUDFHost"
                    );
                    StageOutcome::Applied
                }
                Err(e) => {
                    tracing::error!(error = %format!("{e:#}"), "recovery: driver cycle failed");
                    StageOutcome::Failed
                }
            }
        }
        _ => StageOutcome::Unsupported,
    }
}

/// Attach-only window after a capture loss. Session detection can still be stale, and a
/// rebuild acting on a stale "Gaming" answer restarts gamescope-session.target — on SteamOS
/// that steals the seat back from the session the user just switched to.
const PROBE_HOLDOFF: Duration = Duration::from_secs(4);
/// A Gaming↔Desktop switch can take 15 s+ to bring the new compositor up.
const REBUILD_BUDGET: Duration = Duration::from_secs(40);
/// A managed/attach gamescope (re)launch takes up to 45 s (the Steam Big Picture cold
/// start): room for two, so a flat 40 s does not expire inside the first attempt.
const GAMESCOPE_REBUILD_BUDGET: Duration = Duration::from_secs(100);

/// How long one capture loss may keep retrying its rebuild.
pub(crate) struct RebuildBudget {
    loss_at: Instant,
}

impl RebuildBudget {
    pub(crate) fn start() -> RebuildBudget {
        RebuildBudget {
            loss_at: Instant::now(),
        }
    }

    /// Hold across one build attempt: inside the holdoff it attaches to live outputs only,
    /// never stopping, relaunching or taking over a session.
    pub(crate) fn probe_scope(&self) -> Option<crate::vdisplay::RebuildProbeScope> {
        (self.loss_at.elapsed() < PROBE_HOLDOFF).then(crate::vdisplay::rebuild_probe_scope)
    }

    /// The budget for `compositor` has run out. Checked per attempt: re-detection can move it.
    pub(crate) fn expired(&self, compositor: Option<crate::vdisplay::Compositor>) -> bool {
        let budget = if compositor == Some(crate::vdisplay::Compositor::Gamescope) {
            GAMESCOPE_REBUILD_BUDGET
        } else {
            REBUILD_BUDGET
        };
        self.loss_at.elapsed() >= budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Either arm trips: silence for the window, or a backlog the window cannot explain.
    #[test]
    fn an_encode_stall_trips_on_silence_or_backlog() {
        let ms = Duration::from_millis;
        // 64 fps: a 2 s window is 128 intervals, so depth 2 bounds the backlog at 130.
        let i = Duration::from_micros(15_625);
        assert!(
            !encode_stalled(0, ms(60_000), 2, i),
            "nothing owed never stalls"
        );
        assert!(!encode_stalled(3, ms(1_999), 2, i));
        assert!(encode_stalled(3, ms(2_000), 2, i));
        assert!(!encode_stalled(130, ms(10), 2, i));
        assert!(
            encode_stalled(131, ms(10), 2, i),
            "AUs trickle while the backlog grows"
        );
        // 2 fps: the window is eight intervals (4 s), not 2 s.
        assert!(!encode_stalled(1, ms(3_000), 1, ms(500)));
        assert!(encode_stalled(1, ms(4_000), 1, ms(500)));
    }

    /// Five resets back off 100 ms → 1.6 s (floored at one frame), the sixth ends the
    /// session, a failed in-place reset spends one, and an AU refills the budget.
    #[test]
    fn the_reset_budget_backs_off_and_refills_on_an_au() {
        let ms = Duration::from_millis;
        let mut w = EncoderWatchdog::new();
        let backoffs: Vec<_> = (0..5).map(|_| w.spend(ms(8))).collect();
        assert_eq!(backoffs, [100, 200, 400, 800, 1_600].map(|b| Some(ms(b))));
        assert_eq!(w.spend(ms(8)), None);
        w.on_au();
        assert_eq!(w.spend(ms(500)), Some(ms(500)), "never under one frame");
        assert_eq!(w.recover(ms(8), || false), None);
        assert_eq!(w.resets(), 2);
        assert_eq!(w.recover(ms(8), || true), Some(ms(400)));
        assert!(
            w.stalled(1, 2, ms(16)).is_none(),
            "a reset restarts the watch"
        );
    }

    /// A configuration error ends the session without spending a reset; any other submit
    /// error resets in place and backs off.
    #[test]
    fn a_terminal_submit_error_spends_no_reset() {
        let ms = Duration::from_millis;
        let mut w = EncoderWatchdog::new();
        let terminal = anyhow::Error::new(crate::encode::TerminalEncoderError).context("open");
        let ended = on_submit_error(&mut w, terminal, ms(8), || {
            panic!("reset on a terminal error")
        });
        assert!(ended.is_err());
        assert_eq!(w.resets(), 0);
        let busy = on_submit_error(&mut w, anyhow::anyhow!("busy"), ms(8), || true);
        assert_eq!(busy.unwrap(), ms(100));
        assert_eq!(w.resets(), 1);
    }
}
