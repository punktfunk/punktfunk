//! The frame-delivery endpoint: one tick of the capture loop, and the
//! [`Capturer`] surface the stream loop drives.
//!
//! [`IddPushCapturer::try_consume`] runs the pollers, walks the recovery ladder
//! and then reports the driver's frame cadence as a pixel-less
//! [`CapturedFrame`] — the driver owns the pixels, so a delivery is only the
//! news that its encode pool took a new composed frame, and `Ok(None)` means
//! the desktop composed nothing. The trait impl adds the loop's control points:
//! encoder telemetry in, recovery stages out, and the in-place presentation
//! restart the ladder's first rung runs.

use super::*;

impl IddPushCapturer {
    /// QPC ticks per second, read once. `0` when the call failed; every span then reads 0.
    fn qpc_freq() -> u64 {
        static FREQ: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        *FREQ.get_or_init(|| {
            let mut f = 0i64;
            // SAFETY: plain FFI; `f` is a valid local out-param. Frequency is fixed at boot.
            let _ = unsafe { QueryPerformanceFrequency(&mut f) };
            f.max(0) as u64
        })
    }

    /// A span of QPC ticks in µs (QPC is system-wide, so two driver stamps subtract).
    pub(super) fn qpc_ticks_us(ticks: u64) -> u64 {
        match Self::qpc_freq() {
            0 => 0,
            freq => ticks.saturating_mul(1_000_000) / freq,
        }
    }

    /// Age of a driver QPC stamp in µs. 0 if the stamp is ahead.
    pub(super) fn qpc_age_us(stamp: u64) -> u64 {
        let mut now = 0i64;
        // SAFETY: plain FFI; `now` is a valid local out-param.
        if unsafe { QueryPerformanceCounter(&mut now) }.is_err() {
            return 0;
        }
        Self::qpc_ticks_us((now as u64).saturating_sub(stamp))
    }

    /// One tick: pollers, recovery, then the driver's frame cadence. A delivery carries no
    /// pixels (the driver encodes them) — its geometry, format and provenance are what the
    /// stream loop consumes, and `Ok(None)` means the desktop composed nothing since the last.
    fn try_consume(&mut self) -> Result<Option<CapturedFrame>> {
        if let Some(e) = self.pending_fault.take() {
            return Err(e);
        }
        // Secure-desktop first: UAC/Winlogon may produce no frames until this edge.
        self.poll_secure_desktop();
        // Witness before any early return so every gap shape accumulates cursor motion.
        self.sample_cursor_witness();
        // A "Use HDR" flip or a resize re-opens the encoder at the matching format.
        self.poll_display_hdr();
        // Recover-or-drop: a presentation restart that never resumes ends the session. The
        // restart's own mode commit composes a frame; the ladder's deadlines govern an episode.
        if let Some(since) = self.recovering_since
            && since.elapsed() > Duration::from_secs(3)
            && !self.recovery.owns_episode()
        {
            bail!(
                "IDD-push: the display was restarted in place and no frame followed within 3s \
                 — dropping the session so the client reconnects"
            );
        }
        // A dead WUDFHost and an idle desktop both stop advancing the source counter. Probe
        // while stale so the driver cycle fires instead of the session streaming nothing.
        if self.last_fresh.elapsed() > Duration::from_secs(2)
            && self.last_liveness.elapsed() > Duration::from_secs(1)
        {
            self.last_liveness = Instant::now();
            if !self.broker.driver_alive() {
                tracing::warn!(
                    wudf_pid = self.broker.wudf_pid,
                    "IDD push: the pf-vdisplay WUDFHost is gone — firing the driver cycle"
                );
                self.pending_stage = Some(pf_frame::recovery::Stage::DriverCycle);
                return Ok(None);
            }
        }
        // Staged recovery — after the driver-death watch, so an episode means the WUDFHost is
        // ALIVE and the presentation path is what stopped.
        self.recovery_tick()?;
        // Stall-attribution evidence: the STALEST the driver's drain heartbeat ever reads
        // between fresh frames. A heartbeat that goes quiet for the hole convicts the worker
        // (starved/dead WUDFHost); one that stays fresh through it indicts the compose path.
        if let Some(age) = self.heartbeat_age() {
            self.max_hb_age_us = self.max_hb_age_us.max(age.as_micros() as u64);
        }
        // The driver's pool counter is the only source clock this side has. Before the
        // encoder opens there is no counter at all, and the loop needs one frame to open it.
        let opened = self.encoder.is_some();
        let driver_seq = self.encoder.map_or(0, |t| t.source_seq);
        let geometry = (self.width, self.height, self.out_format());
        // A re-opened encoder's fresh section reads 0 until the pool takes a frame: no news,
        // not a delivery.
        if opened
            && (driver_seq == 0 || driver_seq == self.driver_source_seq)
            && self.delivered == Some(geometry)
        {
            return Ok(None);
        }
        self.driver_source_seq = driver_seq;
        self.delivered = Some(geometry);
        let now = Instant::now();
        // The newest access unit's OS present stamp against the moment the host took it: the
        // ground-truth clock that tells "DWM stopped presenting" from "we were late".
        let arrival_ms = self
            .encoder
            .and_then(|t| t.present_to_arrival)
            .map(|d| d.as_millis() as u64);
        if self.recovering_since.take().is_some() {
            // Self-inflicted gap (the presentation restart). Reset so it is not a DWM stall.
            self.stall_watch.reset();
        } else if let Some(stall) = self.stall_watch.note_fresh(now, arrival_ms) {
            // ETW prose uses gap + 300 ms lead-in (the cause lands just before);
            // discriminator counts use the gap only — presents from healthy flow
            // would falsely acquit.
            let (etw, etw_counts) = self
                .etw
                .as_ref()
                .and_then(|w| {
                    now.checked_sub(stall.gap)
                        .map(|from| w.window_report(from, now, Duration::from_millis(300)))
                })
                .unzip();
            let evidence = StallEvidence {
                // `None` until the drain worker's first heartbeat: an age of 0 would acquit
                // a worker that never ran.
                max_heartbeat_age_ms: self
                    .encoder
                    .and_then(|t| t.drain_heartbeat)
                    .map(|_| self.max_hb_age_us / 1_000),
                // Same window as the report's OS-event correlation (gap + cause lead-in).
                probes: now
                    .checked_sub(stall.gap + Duration::from_millis(300))
                    .zip(self.probes.as_deref())
                    .map(|(from, p)| p.window(from, now)),
                etw,
                etw_counts,
                // Gap accumulator only; this call's pending (ending-frame move) is still unfolded.
                // Under a declared hardware cursor pointer travel composes nothing, so it is no
                // witness: the hole reads as damage-idle, as `health` reads it.
                cursor_moved_px: if self.cursor_shared.is_some() || self.composite_cursor {
                    Some(0)
                } else {
                    self.cursor.moved_px()
                },
            };
            self.stall_watch.report(&stall, now, &evidence);
        }
        // Sustained ~2 fps stretch: per-hole lines gate on prior ACTIVE flow.
        if let Some(r) = self.stall_watch.take_recovery() {
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
        // The opt-in present-flow line rides fresh frames, so a still desktop prints none.
        if let Some(flow) = self.etw.as_ref().and_then(|w| w.flow_line(now)) {
            tracing::info!(
                window_ms = super::dxgkrnl_etw::FLOW_EVERY.as_millis() as u64,
                etw = %flow,
                "IDD push: present flow"
            );
        }
        // A recovery episode closes only on the budgeted count of NEW source frames.
        if let Some((summary, outage)) = self.recovery.source_frame(now) {
            tracing::info!(
                target = %self.ccd,
                ?summary,
                outage_ms = outage.as_millis() as u64,
                "IDD push: recovery episode closed"
            );
            self.recovered_outage = Some(outage);
        }
        self.last_fresh = now;
        self.max_hb_age_us = 0;
        // Pending sample is the ending frame's move — discarded, never folded.
        self.cursor.fresh_frame();
        self.source_seq += 1;
        Ok(Some(CapturedFrame {
            // The driver stamps each access unit with the frame's own present QPC; this side
            // reports the sequence only.
            provenance: pf_frame::Provenance::source(self.source_seq, 0),
            width: self.width,
            height: self.height,
            pts_ns: now_ns(),
            format: geometry.2,
            // No pixels cross the boundary: the loop sees `Encoder::ready_aus` answer `Some`
            // and owes wire indexes instead of submitting this frame.
            payload: FramePayload::Cpu(Vec::new()),
            cursor: None,
        }))
    }
    /// Hand the re-arrived monitor its cursor channel again.
    ///
    /// The driver's cursor worker does not survive a re-arrival: the desired forward flag is
    /// inherited, but the worker is gone until a channel is delivered, and the composite render
    /// model has no other shape source. A mid-stream flip to capture-the-cursor then stores
    /// cleanly on both sides and blends nothing — the driver logs "no live worker" and waits for
    /// a delivery that never comes. Idempotent driver-side, so re-delivering costs one IOCTL.
    fn redeliver_cursor_channel(&mut self) {
        let (Some(cs), Some(send)) = (self.cursor_shared.as_ref(), self.cursor_sender.as_ref())
        else {
            return;
        };
        tracing::info!(
            target_id = self.target_id,
            composite = self.composite_cursor,
            "cursor channel: re-delivering after the monitor re-arrival"
        );
        if !deliver_cursor_channel(&self.broker, self.target_id, cs, send) {
            tracing::warn!(
                target_id = self.target_id,
                "cursor channel re-delivery failed after the re-arrival — a flip to the capture \
                 model will have no shape to blend"
            );
            return;
        }
        // Delivery starts the worker declared; restore the model this session runs.
        if let Some(fwd) = self.cursor_forward.as_ref()
            && let Err(e) = fwd(self.driver_forward())
        {
            tracing::warn!(
                composite = self.composite_cursor,
                error = %format!("{e:#}"),
                "cursor render model not re-applied after the re-arrival"
            );
        }
    }
}

impl Capturer for IddPushCapturer {
    fn cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        self.live_cursor()
    }

    fn set_cursor_forward(&mut self, on: bool) {
        // Capture model: the declared hardware cursor stays excluded (no working un-declare);
        // the driver blends it into the frames it encodes. `composite_forced` cannot turn off
        // — no client draws. Under the secure desktop the driver stays stood down; dismissal
        // applies the model chosen here.
        let composite = (!on && self.cursor_shared.is_some()) || self.composite_forced;
        if self.composite_cursor != composite {
            self.composite_cursor = composite;
            tracing::info!(
                composite,
                "cursor render model: the driver composites {}",
                if composite {
                    "ON (capture model — blending the pointer into what it encodes)"
                } else {
                    "OFF (client draws locally)"
                }
            );
            if let (Some(_), Some(fwd)) =
                (self.cursor_shared.as_ref(), self.cursor_forward.as_ref())
                && let Err(e) = fwd(self.driver_forward())
            {
                tracing::warn!(
                    composite,
                    error = %format!("{e:#}"),
                    "cursor render model: the driver did not take the flip"
                );
            }
        }
    }

    fn next_frame(&mut self) -> Result<CapturedFrame> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(f) = self.try_consume()? {
                return Ok(f);
            }
            if Instant::now() > deadline {
                bail!(
                    "no IDD-push frame within 20s (target {}) — the driver's encode pool took no \
                     composed frame: the swap-chain was never assigned, the display is powered \
                     off, or DWM composes nothing for it",
                    self.target_id
                );
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    fn try_latest(&mut self) -> Result<Option<CapturedFrame>> {
        self.try_consume()
    }

    fn hdr_meta(&self) -> Option<pf_frame::HdrMeta> {
        // BT.2020 PQ while HDR. The driver does not forward IDDCX_HDR10_METADATA;
        // send the same generic HDR10 baseline as the native 0xCE path.
        self.display_hdr.then(pf_frame::hdr::generic_hdr10)
    }

    fn composes_sdr_fp16(&self) -> bool {
        self.display_wcg && !self.display_hdr
    }

    fn capture_target_id(&self) -> Option<u32> {
        Some(self.target_id)
    }

    fn resize_output(&mut self, width: u32, height: u32) -> bool {
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
        self.redeliver_cursor_channel();
        true
    }

    fn take_recovered_outage(&mut self) -> Option<Duration> {
        self.recovered_outage.take()
    }

    fn health(&self) -> Option<crate::CaptureHealth> {
        Some(self.recovery.report(Instant::now(), self.encoder.as_ref()))
    }

    fn observe_encoder(&mut self, t: Option<pf_frame::health::EncoderTelemetry>) {
        self.encoder = t;
    }

    fn take_pending_stage(&mut self) -> Option<pf_frame::recovery::Stage> {
        self.pending_stage.take()
    }

    fn stage_done(
        &mut self,
        stage: pf_frame::recovery::Stage,
        outcome: pf_frame::recovery::StageOutcome,
    ) {
        use pf_frame::recovery::{Stage, StageOutcome};
        let step = self.finish_stage(stage, outcome);
        if let Err(e) = self.drive(step) {
            self.pending_fault = Some(e);
            return;
        }
        // A driver cycle, applied or not, ends this capturer: the WUDFHost its encoder points at
        // is gone either way, and the driver-death watch fires the cycle outside any episode, so
        // nothing else bounds a reload that keeps failing.
        if matches!(stage, Stage::DriverCycle) {
            let applied = matches!(outcome, StageOutcome::Applied);
            self.pending_fault = Some(anyhow::anyhow!(
                "IDD-push: the pf-vdisplay driver cycle {} — ending the capturer so the session \
                 rebuilds its virtual output",
                if applied {
                    "reloaded the adapter"
                } else {
                    "FAILED (adapter not reloaded)"
                }
            ));
        }
    }

    fn driver_endpoint(&self) -> Option<crate::DriverEndpoint> {
        Some(crate::DriverEndpoint {
            target_id: self.target_id,
            wudf_pid: self.broker.wudf_pid,
        })
    }

    fn restart_presentation_in_place(&mut self) -> bool {
        // A target with no ACTIVE path cannot be recovered in place (immunity plan WP10 item 7):
        // a same-mode reset would attach to a known-inactive display. Fail fast — on a FRESH
        // snapshot only; a last-known-good one is not evidence either way.
        let snap = pf_win_display::display_events::snapshot_or_query();
        if snap.is_fresh() && !snap.target(self.ccd).is_some_and(|t| t.active) {
            tracing::warn!(
                target = %self.ccd,
                "IDD push: same-mode recovery refused — the target has no active display path \
                 (topology removed it); a topology recovery must precede a presentation restart"
            );
            return false;
        }
        // The eviction's topology commit leaves DWM not presenting to this display, so the
        // driver's drain worker acquires nothing. CDS_RESET forces a real mode-set at the
        // CURRENT mode — the same lever bring-up's ADD path relies on.
        match pf_win_display::win_display::resolve_gdi_name(self.ccd) {
            Some(gdi) => {
                if !pf_win_display::win_display::force_mode_reset(&gdi) {
                    tracing::warn!(
                        target_id = self.target_id,
                        "IDD push: presentation-restart mode reset failed"
                    );
                    return false;
                }
            }
            None => {
                tracing::warn!(
                    target_id = self.target_id,
                    "IDD push: no GDI name for the presentation-restart mode reset"
                );
                return false;
            }
        }
        tracing::info!(
            target_id = self.target_id,
            mode = format!("{}x{}", self.width, self.height),
            "IDD push: same-mode presentation restart"
        );
        self.recovering_since.get_or_insert_with(Instant::now);
        true
    }
}

impl Drop for IddPushCapturer {
    fn drop(&mut self) {
        // Must not leave per-target desired-state off: the next session would
        // adopt undeclared and silently run the composite model. Open-time reset
        // covers host crash; this is orderly teardown.
        if self.secure_active && self.cursor_shared.is_some() {
            if let Some(fwd) = self.cursor_forward.as_ref() {
                let _ = fwd(true);
            }
        }
    }
}
