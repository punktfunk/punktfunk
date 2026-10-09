//! This capturer's side of the recovery ladder: what the classifier is fed, and
//! which rungs run on the capture thread.
//!
//! [`recovery::Supervisor`] owns the verdicts; this file gathers their inputs —
//! the driver's drain heartbeat, the pool's source counter, and a cursor
//! witness that says whether anything was dirty through a gap — and walks the
//! steps it hands back. A rung the stream loop owns (encoder reset, driver
//! cycle) parks in `pending_stage` instead, and the loop re-enters through
//! `stage_done`. An exhausted ladder ends the plane with a typed fault.

use super::*;

impl IddPushCapturer {
    /// Feed [`CursorWitness`] the one thing it cannot read for itself. The rule — the one-call
    /// lag and the rate limit — lives there and is tested there.
    pub(super) fn sample_cursor_witness(&mut self) {
        self.cursor.sample(Instant::now(), || {
            let mut pos = POINT::default();
            // SAFETY: plain FFI; `pos` is a valid out-param for this synchronous call.
            unsafe { GetCursorPos(&mut pos) }
                .is_ok()
                .then_some((pos.x, pos.y))
        });
    }

    /// Staged recovery (immunity plan WP12/WP13). A wedged-but-ALIVE display answers
    /// `Ok(None)` forever, so a known-active desktop could stream nothing indefinitely. The
    /// classifier names the gap from the driver's own clocks — drain heartbeat, pool source
    /// sequence, access units — plus cursor travel and an unanswered canary; the coordinator
    /// walks the ladder from that class, one rung at a time under a deadline, and proves each
    /// rung with NEW source frames (access units for an encoder stall). No evidence = plain
    /// idle = no recovery: a static desktop composes nothing, and that is healthy. An
    /// exhausted ladder ends the plane with the typed `CaptureFault::SourceStalled`.
    pub(super) fn recovery_tick(&mut self) -> Result<()> {
        let now = Instant::now();
        let drain = self.encoder.map_or(0, |t| t.drain_progress());
        if drain != self.drain_seq {
            self.drain_seq = drain;
            self.last_drain = now;
        }
        let inputs = recovery::Inputs {
            now,
            last_source: self.last_drain,
            source_seq: self.drain_seq,
            heartbeat_age: self.heartbeat_age(),
            // With an IddCx hardware cursor declared — the client draws it or the driver
            // composites it — DWM composes nothing for pointer travel and the input canary can
            // never be answered: neither is evidence of a changed desktop.
            cursor_gap_px: if self.cursor_shared.is_some() || self.composite_cursor {
                0
            } else {
                self.cursor.gap_px()
            },
            recreating: self.recovering_since.is_some(),
            secure_desktop: self.secure_active,
            topology_held: pf_win_display::topology_churn::held(),
            encoder: self.encoder,
        };
        let step = self.recovery.tick(inputs);
        self.drive(step)
    }

    /// How long ago the driver's drain worker last finished a pass; `None` before its first
    /// heartbeat (no encoder open yet).
    pub(super) fn heartbeat_age(&self) -> Option<Duration> {
        let t = self.encoder?;
        Some(
            Instant::now()
                .checked_duration_since(t.drain_heartbeat?)
                .unwrap_or_default(),
        )
    }

    /// Walk the supervisor's steps until it rests or the plane ends. A rung the stream loop
    /// owns is parked in `pending_stage`; the loop's `stage_done` re-enters here with its
    /// outcome.
    pub(super) fn drive(&mut self, mut step: recovery::Step) -> Result<()> {
        loop {
            match step {
                recovery::Step::Nothing => return Ok(()),
                recovery::Step::Canary => {
                    let rect = pf_win_display::display_events::snapshot().source_rect(self.ccd);
                    if rect.is_some_and(pf_win_display::compose_probe::present) {
                        tracing::info!(
                            target = %self.ccd,
                            "IDD push: source suspect on weak evidence — presenting the compose canary"
                        );
                    } else {
                        // A canary nobody saw proves nothing: it must not age into strong evidence.
                        self.recovery.canary_not_shown();
                    }
                    return Ok(());
                }
                recovery::Step::Run(stage) => {
                    let Some(outcome) = self.run_stage(stage) else {
                        self.pending_stage = Some(stage);
                        return Ok(());
                    };
                    step = self.finish_stage(stage, outcome);
                }
                recovery::Step::Recovered { summary, outage } => {
                    tracing::info!(
                        target = %self.ccd,
                        ?summary,
                        outage_ms = outage.as_millis() as u64,
                        "IDD push: recovery episode closed"
                    );
                    self.recovered_outage = Some(outage);
                    return Ok(());
                }
                recovery::Step::Failed { gap, summary } => {
                    let fault = crate::CaptureFault::SourceStalled {
                        secs: gap.as_secs() as u32,
                    };
                    tracing::error!(
                        target = %self.ccd,
                        %fault,
                        ?summary,
                        "IDD push: recovery ladder exhausted"
                    );
                    return Err(anyhow::Error::new(fault).context(
                        "IDD-push: a known-active display delivered no source frame through the \
                         recovery ladder — ending the video plane with a typed error",
                    ));
                }
            }
        }
    }

    /// Record a rung's outcome with the supervisor and take its next step.
    pub(super) fn finish_stage(
        &mut self,
        stage: pf_frame::recovery::Stage,
        outcome: pf_frame::recovery::StageOutcome,
    ) -> recovery::Step {
        tracing::warn!(
            target = %self.ccd,
            ?stage,
            ?outcome,
            "IDD push: recovery stage"
        );
        self.recovery.stage_done(Instant::now(), stage, outcome)
    }

    /// Run one ladder rung here, or `None` for a rung the stream loop owns: the encoder reset
    /// and the driver cycle, where the loop holds the encoder and the display manager. Rungs
    /// with no actuator on this host report `Unsupported` and the ladder moves on unpenalised.
    fn run_stage(
        &mut self,
        stage: pf_frame::recovery::Stage,
    ) -> Option<pf_frame::recovery::StageOutcome> {
        use pf_frame::recovery::{Stage, StageOutcome};
        Some(match stage {
            Stage::PresentationReset => {
                if self.restart_presentation_in_place() {
                    StageOutcome::Applied
                } else {
                    StageOutcome::Failed
                }
            }
            Stage::EncoderReset | Stage::DriverCycle => return None,
            Stage::SwapChainReset => StageOutcome::Unsupported,
        })
    }
}
