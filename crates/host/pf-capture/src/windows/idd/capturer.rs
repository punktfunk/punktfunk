//! The frame-delivery endpoint: one tick of the capture loop, and the
//! [`Capturer`] surface the stream loop drives.
//!
//! [`IddPushCapturer::try_consume`] runs the tick ([`crate::windows::tick`]): the display's
//! pollers and the remote source's phases in the order it steps them, and reports the
//! driver's frame cadence as a pixel-less [`CapturedFrame`]: the driver owns the pixels, so a
//! delivery is only the news that its encode pool took a new composed frame, and `Ok(None)`
//! means the desktop composed nothing. The role impls add the loop's control points: encoder
//! telemetry in, recovery stages out as events, the in-place resize and presentation restart,
//! the cursor, and the driver's encoder.

use super::*;
use crate::windows::cursor_state::Event;
use crate::windows::remote::source::{first_frame, Geometry};
use crate::windows::stall_model::Recovery;
use crate::windows::tick::{Action, Cursor, Inputs};
use std::collections::VecDeque;

/// Make the OS present to `ccd` again at its current mode, and open the recover-or-drop window
/// in `recovering_since`. `false`: unrecoverable in place.
pub(super) fn restart_presentation(
    ccd: pf_win_display::win_display::CcdTargetKey,
    target_id: u32,
    (width, height): (u32, u32),
    recovering_since: &mut Option<Instant>,
) -> bool {
    // A target with no ACTIVE path cannot be recovered in place (immunity plan WP10 item 7):
    // a same-mode reset would attach to a known-inactive display. Fail fast — on a FRESH
    // snapshot only; a last-known-good one is not evidence either way.
    let snap = pf_win_display::display_events::snapshot_or_query();
    if snap.is_fresh() && !snap.target(ccd).is_some_and(|t| t.active) {
        tracing::warn!(
            target = %ccd,
            "IDD push: same-mode recovery refused — the target has no active display path \
             (topology removed it); a topology recovery must precede a presentation restart"
        );
        return false;
    }
    // The eviction's topology commit leaves DWM not presenting to this display, so the
    // driver's drain worker acquires nothing. CDS_RESET forces a real mode-set at the
    // CURRENT mode — the same lever bring-up's ADD path relies on.
    match pf_win_display::win_display::resolve_gdi_name(ccd) {
        Some(gdi) => {
            if !pf_win_display::win_display::force_mode_reset(&gdi) {
                tracing::warn!(
                    target_id,
                    "IDD push: presentation-restart mode reset failed"
                );
                return false;
            }
        }
        None => {
            tracing::warn!(
                target_id,
                "IDD push: no GDI name for the presentation-restart mode reset"
            );
            return false;
        }
    }
    tracing::info!(
        target_id,
        mode = format!("{width}x{height}"),
        "IDD push: same-mode presentation restart"
    );
    recovering_since.get_or_insert_with(Instant::now);
    true
}

impl IddPushCapturer {
    /// One tick: run what [`Tick`] steps against the display and the remote source, and answer
    /// its asks. A delivery carries no pixels (the driver encodes them) — its geometry, format
    /// and provenance are what the stream loop consumes, and `Ok(None)` means the desktop
    /// composed nothing since the last.
    fn try_consume(&mut self) -> Result<Option<CapturedFrame>> {
        let mut queue = VecDeque::from(self.tick.step(Inputs::Begin));
        let (mut now, mut frame) = (Instant::now(), None);
        while let Some(action) = queue.pop_front() {
            let next = match action {
                Action::TakeFault => {
                    self.remote.take_fault()?;
                    continue;
                }
                Action::PollSecureDesktop => {
                    self.poll_secure_desktop();
                    continue;
                }
                Action::SampleWitness => {
                    self.sample_cursor_witness();
                    continue;
                }
                Action::PollDepth => {
                    self.poll_display_hdr();
                    continue;
                }
                Action::Sample => Inputs::Sampled {
                    now: Instant::now(),
                    owns_episode: self.remote.owns_episode(),
                    cursor: Cursor {
                        declared: self.cursor_model.declared(),
                        gap_px: self.cursor.gap_px(),
                        moved_px: self.cursor.moved_px(),
                    },
                },
                Action::EndSession(why) => bail!("{why}"),
                Action::ProbeDriver => {
                    if self.remote.poll()? {
                        return Ok(None);
                    }
                    continue;
                }
                Action::Supervise {
                    cursor_gap_px,
                    recreating,
                } => {
                    self.recovery_tick(cursor_gap_px, recreating)?;
                    Inputs::Supervised {
                        heartbeat_age: self.remote.heartbeat_age(),
                    }
                }
                Action::Admit => {
                    let (width, height, format) = (self.width, self.height, self.out_format());
                    if !self.remote.admit(Geometry {
                        width,
                        height,
                        format,
                    }) {
                        return Ok(None);
                    }
                    now = Instant::now();
                    let encoder = self.remote.telemetry();
                    Inputs::Admitted {
                        now,
                        present_to_arrival: encoder.and_then(|t| t.present_to_arrival),
                        heartbeat_seen: encoder.is_some_and(|t| t.drain_heartbeat.is_some()),
                    }
                }
                Action::ReportStall {
                    gap,
                    max_heartbeat_age_ms,
                    cursor_moved_px,
                } => {
                    self.report_stall(gap, now, max_heartbeat_age_ms, cursor_moved_px);
                    continue;
                }
                Action::DegradedStretch(r) => {
                    log_degraded(&r);
                    continue;
                }
                Action::FlowLine => {
                    if let Some(flow) = self.etw.as_ref().and_then(|w| w.flow_line(now)) {
                        tracing::info!(
                            window_ms = super::dxgkrnl_etw::FLOW_EVERY.as_millis() as u64,
                            etw = %flow,
                            "IDD push: present flow"
                        );
                    }
                    continue;
                }
                Action::Deliver => {
                    frame = Some(self.remote.deliver(now));
                    continue;
                }
                Action::ResetWitness => {
                    self.cursor.fresh_frame();
                    continue;
                }
            };
            queue.extend(self.tick.step(next));
        }
        Ok(frame)
    }
}

/// A sustained ~2 fps stretch closed: per-hole lines gate on prior active flow, so this is the
/// stretch's one line past its first hole.
fn log_degraded(r: &Recovery) {
    let arrival = r.arrival_ms();
    tracing::info!(
        degraded_ms = r.degraded.as_millis() as u64,
        holes = r.holes,
        hole_time_ms = r.hole_time.as_millis() as u64,
        worst_hole_ms = r.worst.as_millis() as u64,
        // last/mean/max between the OS present and the access unit reaching us.
        present_to_arrival_ms = arrival.as_deref().unwrap_or("absent"),
        present_to_arrival_n = r.arrival_n,
        "IDD-push capture recovered from a degraded stretch — fresh frames arrived \
         only between stall-sized holes for its whole span; the per-stall lines \
         above cover at most its first hole"
    );
}

impl Capturer for IddPushCapturer {
    fn next_frame(&mut self, _budget: crate::FirstFrame) -> Result<CapturedFrame> {
        let target_id = self.target_id;
        first_frame(
            || self.try_consume(),
            || {
                anyhow::anyhow!(
                    "no IDD-push frame within 20s (target {target_id}) — the driver's encode \
                     pool took no composed frame: the swap-chain was never assigned, the \
                     display is powered off, or DWM composes nothing for it"
                )
            },
        )
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.try_consume()
    }

    fn events(&mut self) -> Vec<crate::SourceEvent> {
        self.remote.events()
    }

    fn colour(&self) -> crate::ColourFacts {
        // BT.2020 PQ while HDR. The driver does not forward IDDCX_HDR10_METADATA;
        // send the same generic HDR10 baseline as the native 0xCE path.
        let composed = self.depth.composed();
        crate::ColourFacts {
            hdr_meta: composed.hdr.then(pf_frame::hdr::generic_hdr10),
            sdr_fp16: composed.wcg && !composed.hdr,
        }
    }

    fn cursor(&mut self) -> Option<&mut dyn crate::CursorSource> {
        Some(self)
    }

    fn display(&mut self) -> Option<&mut dyn crate::DisplayControl> {
        Some(self)
    }

    fn supervised(&mut self) -> Option<&mut dyn crate::Supervised> {
        Some(self)
    }

    fn remote_encoder(&self) -> Option<&dyn crate::RemoteEncoder> {
        Some(self)
    }
}

impl crate::CursorSource for IddPushCapturer {
    fn overlay(&mut self) -> Option<pf_frame::CursorOverlay> {
        self.live_cursor()
    }

    fn set_forward(&mut self, client_draws: bool) {
        self.cursor_event(Event::ClientDraws(client_draws));
    }
}

impl crate::DisplayControl for IddPushCapturer {
    fn target_id(&self) -> u32 {
        self.target_id
    }

    fn resize(&mut self, width: u32, height: u32) -> bool {
        // The session already committed the new mode. Adopt it now — no two-strike debounce
        // (that stays for external HDR/game mode-sets); the loop's next frame carries the new
        // geometry, which re-opens the driver's encoder.
        if (width, height) == (self.width, self.height) {
            return true;
        }
        tracing::info!(
            target_id = self.target_id,
            from = format!("{}x{}", self.width, self.height),
            to = format!("{width}x{height}"),
            "IDD push: host-initiated resize — re-opening the driver's encoder at the new mode"
        );
        self.width = width;
        self.height = height;
        // A mode outside the driver's frozen advertised list re-arrives the monitor, and a fresh
        // monitor composes SDR whatever the session negotiated. Re-assert before the encoder
        // re-opens, or it opens for FP16 against a BGRA surface the pool can only refuse.
        self.pin_negotiated_depth();
        self.cursor_event(Event::Rearrived);
        true
    }

    fn restart_presentation(&mut self) -> bool {
        restart_presentation(
            self.ccd,
            self.target_id,
            (self.width, self.height),
            &mut self.tick.recovering_since,
        )
    }
}

impl crate::Supervised for IddPushCapturer {
    fn observe(&mut self, t: Option<pf_frame::health::EncoderTelemetry>) {
        self.remote.observe(t);
    }

    fn health(&self) -> crate::CaptureHealth {
        self.remote.health()
    }

    fn stage_done(
        &mut self,
        stage: pf_frame::recovery::Stage,
        outcome: pf_frame::recovery::StageOutcome,
    ) {
        let (remote, mut rungs) = self.source_and_rungs();
        remote.stage_done_with(stage, outcome, &mut rungs);
    }
}

impl crate::RemoteEncoder for IddPushCapturer {
    fn site(&self) -> crate::EncoderSite {
        self.remote.site()
    }

    fn open(&self, params: &crate::DriverEncodeParams) -> Result<Box<dyn pf_encode_core::Encoder>> {
        self.remote.open(params)
    }

    fn arrival_poke(&self) -> Option<std::sync::Arc<dyn Fn() + Send + Sync>> {
        self.remote.arrival_poke()
    }
}

impl Drop for IddPushCapturer {
    fn drop(&mut self) {
        // The open-time reset covers a host that died under the secure desktop; this is the
        // orderly end.
        if let (Some(on), Some(fwd)) = (self.cursor_model.close(), self.cursor_forward.as_ref()) {
            let _ = fwd(on);
        }
    }
}
