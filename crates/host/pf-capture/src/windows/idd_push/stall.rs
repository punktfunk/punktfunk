//! Capture-stall reporting for IDD-push: the log lines for a hole that
//! [`StallWatch`] found, with the OS display events that coincide.
//!
//! Probes and the DxgKrnl ETW session ride the report as evidence when
//! `PUNKTFUNK_IDD_DIAG` turned them on; they name no class.

use super::*;
use crate::stall_model::{attribute, Stall, StallEvidence, StallVerdict, StallWatch};

/// Driver GPU-priority lever (`PFVD_NO_RT_GPU` opt-out; default REALTIME).
/// Reads this process's env; WUDFHost resolves the same variable. An env
/// edited after either process started is stale until restart. Rides every
/// stall-triage line so an A/B is interpretable; lowering priority masks
/// this class at most.
pub(super) fn rt_gpu_driver_posture() -> &'static str {
    if std::env::var_os("PFVD_NO_RT_GPU").is_some() {
        "off (PFVD_NO_RT_GPU)"
    } else {
        "REALTIME (default)"
    }
}

pub(super) fn rt_gpu_host_posture() -> &'static str {
    match std::env::var("PUNKTFUNK_GPU_PRIORITY_CLASS")
        .ok()
        .as_deref()
    {
        Some("off") => "off",
        Some("normal") => "normal",
        Some("high") => "high",
        _ => "REALTIME (default)",
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
            // Presents from any process vs virtual-display kernel-queue adds,
            // both only under PUNKTFUNK_IDD_DIAG.
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
                    "capture stalls are REPEATING without a stable period — same triage as the \
                     metronomic arm: a connected-but-inactive display's standby servicing \
                     (see connected_inactive), then display-poller software (the SteelSeries \
                     GG / SignalRGB class). Lowering the GPU-priority defaults \
                     (setx /M PFVD_NO_RT_GPU 1 / PUNKTFUNK_GPU_PRIORITY_CLASS=high) has \
                     quieted some AMD boxes but masks this class at most — attenuation, not \
                     attribution. A compose-silence tally does NOT exonerate the display \
                     stack — a frozen presenter reads identically"
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
                    "capture stalls are METRONOMIC and coincide with Windows monitor \
                     hot-plug/re-enumeration events — a connected display (or its \
                     cable/switch/AVR) re-probes the link on a timer and Windows re-reacts \
                     each time. Cures, best-first: that display's OSD 'auto input \
                     scan/detect' OFF (and on TVs: instant-on/quick-start + CEC off), \
                     unplug its cable at the GPU, an HPD-holding adapter/dummy plug, or \
                     keep it active while streaming; the pnp_disable_monitors policy axis \
                     suppresses the Windows-side reaction (see connected_inactive for the \
                     suspects)"
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
                    "capture stalls are METRONOMIC with NO coinciding OS display event — \
                     the disturbance is BELOW Windows (damage-idle holes — the cursor \
                     stationary through the hole, i.e. input pauses — are already \
                     excluded from this beat; see cursor_moved_px_during_gap on the \
                     per-stall lines). Suspects: the GPU driver servicing a \
                     connected-but-asleep sink (standby HPD/DDC/link probing), \
                     display-poller software (the SteelSeries-GG/SignalRGB class — \
                     correlate 'slow display-descriptor poll' lines), or the DWM present \
                     clock (try a different refresh rate). Lowering the GPU-priority \
                     defaults (setx /M PFVD_NO_RT_GPU 1 / \
                     PUNKTFUNK_GPU_PRIORITY_CLASS=high) has quieted some AMD boxes but \
                     masks this class at most — a quiet A/B is attenuation, not \
                     attribution. If connected_inactive lists a \
                     display, its standby servicing is a suspect — cursor motion through \
                     the holes is what convicts the display stack. For an external \
                     display: keep it active while streaming, disable its OSD auto input \
                     scan (TVs: instant-on/quick-start + CEC off), unplug it at the GPU, \
                     or use an HPD-holding adapter/dummy. For a LAPTOP PANEL: keep it \
                     active with `topology: primary` (the dark-but-connected-head \
                     hypothesis has no confirmed post-0.28 case — verify with the cursor \
                     witness before chasing it)"
                );
            }
        }
    }
}
