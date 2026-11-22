//! Capture-stall reporting for IDD-push: the log lines for a hole that
//! [`StallWatch`] found, with the OS display events that coincide.
//!
//! Probes (`PUNKTFUNK_IDD_DIAG`) ride the report as evidence and name no class. The DxgKrnl
//! ETW session (`PUNKTFUNK_IDD_DIAG` or `PUNKTFUNK_IDD_FLOW`) rides it too and feeds
//! [`attribute`]: with it on, a still-pointer hole reads damage-idle only when every
//! presenter before it was DWM.

use super::*;
use crate::knobs::Knobs;
use crate::windows::stall_model::{attribute, Stall, StallEvidence, StallVerdict, StallWatch};

/// Driver GPU-priority lever ([`Knobs::no_rt_gpu`]; default REALTIME). Rides every
/// stall-triage line so an A/B is interpretable; lowering priority masks this class at most.
pub(super) fn rt_gpu_driver_posture() -> &'static str {
    if Knobs::process().no_rt_gpu {
        "off (PFVD_NO_RT_GPU)"
    } else {
        "REALTIME (default)"
    }
}

pub(super) fn rt_gpu_host_posture() -> &'static str {
    Knobs::process().gpu_priority_class
}

impl IddPushCapturer {
    /// Report a hole the tick found, with the probe and ETW windows over it. `now` is the
    /// frame that ended it. ETW prose takes a 300 ms lead-in (the cause lands just before); its
    /// counts take the gap only, as presents from healthy flow would falsely acquit.
    pub(super) fn report_stall(
        &mut self,
        gap: Duration,
        now: Instant,
        max_heartbeat_age_ms: Option<u64>,
        cursor_moved_px: Option<u32>,
    ) {
        let (etw, etw_counts) = self
            .etw
            .as_ref()
            .and_then(|w| {
                now.checked_sub(gap)
                    .map(|from| w.window_report(from, now, Duration::from_millis(300)))
            })
            .unzip();
        let evidence = StallEvidence {
            max_heartbeat_age_ms,
            // Same window as the report's OS-event correlation (gap + cause lead-in).
            probes: now
                .checked_sub(gap + Duration::from_millis(300))
                .zip(self.probes.as_deref())
                .map(|(from, p)| p.window(from, now)),
            etw,
            etw_counts,
            cursor_moved_px,
        };
        self.tick.stall_watch.report(&Stall { gap }, now, &evidence);
    }
}

impl StallWatch {
    /// Log a stall, correlate OS display events, and name the cause once the
    /// cadence is metronomic.
    ///
    /// `now` is the frame that ended the stall (same instant as [`Self::note_fresh`])
    /// and bounds the event-correlation window. `evidence` is the capturer's
    /// sample for that window; [`attribute`] rides every stall line.
    pub(super) fn report(&mut self, stall: &Stall, now: Instant, evidence: &StallEvidence) {
        // Gap plus 300 ms lead-in: the causing OS event lands just before
        // DWM stops delivering.
        let window = stall.gap + Duration::from_millis(300);
        let events = now
            .checked_sub(window)
            .map(|from| pf_win_display::display_events::events_between(from, now))
            .unwrap_or_default();
        self.seen = self.seen.saturating_add(1);
        if !events.is_empty() {
            self.with_os_events = self.with_os_events.saturating_add(1);
        }
        let verdict = attribute(stall.gap, evidence);
        self.verdicts[match verdict {
            StallVerdict::NoTelemetry => 0,
            StallVerdict::WorkerStalled => 1,
            StallVerdict::ComposeSilence => 2,
            StallVerdict::DamageIdle => 3,
        }] += 1;
        // Damage-idle is still a delivery hole (episode/recovery count it) but
        // not display-disturbance evidence: skip metronome, rate WARN, and
        // connected-inactive trial. The per-stall line still carries evidence.
        let damage_idle = verdict == StallVerdict::DamageIdle;
        let metronomic = self.cycle(now, damage_idle);
        // debug, not warn: a single hole is a legitimate content pause. The
        // reportable signal is the metronomic cycle below.
        tracing::debug!(
            gap_ms = stall.gap.as_millis() as u64,
            os_display_events = %pf_win_display::display_events::summarize(&events),
            verdict = %verdict,
            probes = evidence.probes.as_ref().map(tracing::field::display),
            etw = evidence.etw.as_deref().unwrap_or("unavailable"),
            // Presents from any process vs virtual-display kernel-queue adds, both
            // only while the ETW session runs (PUNKTFUNK_IDD_DIAG or PUNKTFUNK_IDD_FLOW).
            etw_presents = evidence.etw_counts.map(|c| c.presents),
            etw_queue_adds = evidence.etw_counts.map(|c| c.queue_adds),
            // 0 = nothing to compose. >0 = damage existed and DWM composed none of it.
            cursor_moved_px_during_gap = evidence.cursor_moved_px,
            flow_dwm_only = evidence.etw_counts.map(|c| c.flow_dwm_only),
            max_heartbeat_age_ms = evidence.max_heartbeat_age_ms,
            "IDD-push capture stall — the desktop was composing at speed, then the driver's \
             pool took no frame for the gap; the verdict names the clock that stopped"
        );
        // Aperiodic 150+ ms holes still need the triage payload. Skip when
        // this stall completed a metronomic cycle (richer arms below) or is
        // damage-idle.
        if metronomic.is_none() && !damage_idle {
            if let Some(stalls_in_window) = self.note_for_rate_warn(now) {
                let suspects = pf_win_display::display_events::connected_inactive_physicals();
                let suspects = if suspects.is_empty() {
                    "none".to_string()
                } else {
                    suspects.join(", ")
                };
                tracing::warn!(
                    stalls_in_window = stalls_in_window as u64,
                    os_correlated = format!("{}/{}", self.with_os_events, self.seen),
                    connected_inactive = %suspects,
                    rt_gpu_driver = rt_gpu_driver_posture(),
                    rt_gpu_host = rt_gpu_host_posture(),
                    verdicts = %self.verdict_tally(),
                    "capture stalls repeat without a stable period — suspect a connected-but-\
                     inactive display's standby servicing (connected_inactive), then \
                     display-poller software (SteelSeries GG, SignalRGB). Lowering the GPU \
                     priority (setx /M PFVD_NO_RT_GPU 1, PUNKTFUNK_GPU_PRIORITY_CLASS=high) \
                     has quieted some AMD boxes without finding the cause. A compose-silence \
                     tally doesn't clear the display stack: a frozen presenter reads the same"
                );
            }
        }
        if let Some(period) = metronomic {
            let suspects = pf_win_display::display_events::connected_inactive_physicals();
            let suspects = if suspects.is_empty() {
                "none".to_string()
            } else {
                suspects.join(", ")
            };
            let correlated = format!("{}/{}", self.with_os_events, self.seen);
            let verdict_tally = self.verdict_tally();
            // ≥ half the stalls with a coinciding OS event: the cascade is
            // OS-visible. Otherwise it never surfaces above the driver.
            if self.with_os_events * 2 >= self.seen {
                tracing::warn!(
                    period_s = format!("{:.2}", period.as_secs_f64()),
                    os_correlated = correlated,
                    connected_inactive = %suspects,
                    verdicts = %verdict_tally,
                    "capture stalls repeat on a fixed period and coincide with Windows monitor \
                     hot-plug events — a connected display (or its cable, switch or AVR) \
                     re-probes the link on a timer. Turn off its auto input scan (TVs: \
                     instant-on and CEC), unplug it at the GPU, use an HPD-holding adapter or \
                     dummy plug, or keep it active while streaming; the pnp_disable_monitors \
                     policy suppresses Windows' reaction. connected_inactive lists the suspects"
                );
            } else {
                // Both REALTIME GPU-priority levers (default on). The line
                // must say whether either is engaged so an A/B is interpretable.
                let rt_gpu_driver = rt_gpu_driver_posture();
                let rt_gpu_host = rt_gpu_host_posture();
                tracing::warn!(
                    period_s = format!("{:.2}", period.as_secs_f64()),
                    os_correlated = correlated,
                    connected_inactive = %suspects,
                    rt_gpu_driver,
                    rt_gpu_host,
                    verdicts = %verdict_tally,
                    "capture stalls repeat on a fixed period with no Windows display event — \
                     the cause is below Windows: the GPU driver servicing a connected-but-\
                     asleep display, display-poller software (SteelSeries GG, SignalRGB; \
                     look for 'slow display-descriptor poll' lines), or the DWM present clock \
                     (try another refresh rate). Cursor motion through the holes \
                     (cursor_moved_px_during_gap) convicts the display stack. If \
                     connected_inactive lists a display, keep it active while streaming (a \
                     laptop panel: `topology: primary`), turn off \
                     its auto input scan, or unplug it at the GPU. Lowering the GPU priority \
                     (setx /M PFVD_NO_RT_GPU 1, PUNKTFUNK_GPU_PRIORITY_CLASS=high) has quieted \
                     some AMD boxes without finding the cause"
                );
            }
        }
    }
}
