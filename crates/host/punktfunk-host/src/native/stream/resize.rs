//! The Windows in-place resize: mode-set the live monitor, restore its presentation, open only
//! the encoder. Also the topology re-assert recovery, which is the same path with a liveness gate.

use super::pipeline::{display_mode_for, open_session_encoder, pacing_hz};
use super::state::StreamState;
use super::swap::Next;
use super::*;

impl StreamState {
    /// Mode-set the live monitor, restore its presentation, and open an encoder at the first
    /// frame of the new size, for the rebuild performer to swap in. `None` → full rebuild.
    /// With the driver's encoder, only its pool taking a frame proves the new size; a second
    /// without one restarts the presentation once. `recover_ring` re-attaches the ring at the
    /// current mode and demands a second, newer present as proof the OS resumed presenting.
    pub(super) fn resize(
        &mut self,
        new_mode: punktfunk_core::Mode,
        bitrate_kbps: u32,
        trace: &crate::bringup::Trace,
        recover_ring: bool,
    ) -> Option<Next> {
        let enc_of = self.enc_now();
        let cur_target = self.live.capturer.display().map(|d| d.target_id())?;
        let new_display_mode = display_mode_for(new_mode, self.display.vd.name());
        let vout = match crate::vdisplay::registry::acquire(
            &mut self.display.vd,
            new_display_mode,
            self.facts.life.quit_flag().clone(),
            None,
        ) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "in-place resize: acquire failed");
                return None;
            }
        };
        trace.mark("display_resized");
        let achieved_hz = vout
            .preferred_mode
            .map(|(_, _, hz)| hz)
            .filter(|&hz| hz > 0)
            .unwrap_or(new_display_mode.refresh_hz);
        let effective_hz = pacing_hz(new_mode.refresh_hz, achieved_hz);
        if vout.win_capture.as_ref().map(|t| t.target_id) != Some(cur_target) {
            tracing::info!(
                "resize: monitor re-arrived (no in-place support) — running the full pipeline rebuild"
            );
            return None;
        }
        let restored = match self.live.capturer.display() {
            Some(d) if recover_ring => d.restart_presentation(),
            Some(d) => d.resize(new_mode.width, new_mode.height),
            None => false,
        };
        if !restored {
            return None;
        }
        trace.mark("presentation_restored");
        let open_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        // The driver's pool is still built for the OLD geometry, so no composed frame passes until
        // this SET_ENCODE rebuilds it — the frame wait below cannot come first. A re-arrival gets its
        // swap chain after the arrival, so an open in that window fails for a display about to be
        // fine: retry to its own deadline rather than drop the resize to a full rebuild.
        let wire_seq_base = self.clock.next_au();
        let pre_opened = if self.plan.capture == crate::session_plan::CaptureBackend::IddPush {
            let opened = loop {
                match crate::capture::open_driver_encoder(
                    &self.plan,
                    &*self.live.capturer,
                    (new_mode.width, new_mode.height),
                    effective_hz,
                    enc_of.enc_kbps(bitrate_kbps) as u64 * 1000,
                    self.bit_depth,
                    self.facts.client_hdr,
                    wire_seq_base,
                ) {
                    Ok(e) => break Some(e),
                    Err(e) => {
                        if std::time::Instant::now() >= open_deadline
                            || self.facts.life.quit_flag().load(Ordering::Relaxed)
                        {
                            tracing::warn!(error = %format!("{e:#}"),
                                "resize: re-opening the driver encoder at the new mode failed - full rebuild");
                            break None;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
            };
            Some(opened?)
        } else {
            None
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        // The driver-encode capturer reads the display's progress off the encoder, and these loops
        // are the one place that polls it without the stream loop's own tick. SET_ENCODE retired
        // `enc`'s section, so the clocks are the pre-opened encoder's or they never move.
        let live: &dyn crate::encode::Encoder = pre_opened.as_deref().unwrap_or(&*self.live.enc);
        // A pixel-less delivery carries the asked size even while DWM still composes the old
        // one into the old swap chain, which the pool refuses frame by frame.
        let pool_took =
            || pre_opened.is_none() || live.telemetry().is_some_and(|t| t.source_seq > 0);
        let mut kick_at = pre_opened
            .is_some()
            .then(|| std::time::Instant::now() + std::time::Duration::from_secs(1));
        let new_frame = loop {
            if let Some(s) = self.live.capturer.supervised() {
                s.observe(live.telemetry());
            }
            match self.live.capturer.try_latest() {
                Ok(Some(f))
                    if (f.width, f.height) == (new_mode.width, new_mode.height) && pool_took() =>
                {
                    break f
                }
                Ok(_) => {
                    if kick_at.is_some_and(|t| std::time::Instant::now() >= t) {
                        kick_at = None;
                        tracing::info!(
                            "resize: the driver's pool took no frame at the new size — restarting \
                             the presentation"
                        );
                        if let Some(d) = self.live.capturer.display() {
                            d.restart_presentation();
                        }
                    }
                    if std::time::Instant::now() >= deadline {
                        tracing::warn!(
                            "resize: no new-size frame within 3s of the in-place mode set — running \
                             the full pipeline rebuild"
                        );
                        return None;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"),
                        "resize: capture failed after the in-place mode set — running the full rebuild");
                    return None;
                }
            }
        };
        // First frame is the stash (~50 ms). A second, newer present proves the OS resumed presenting.
        let new_frame = if recover_ring {
            // SOURCE-sequence evidence, not wall-clock PTS — see `source_advanced`.
            let first_seq = new_frame.provenance.source_seq;
            let first_pts = new_frame.pts_ns;
            let live_deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
            loop {
                if let Some(s) = self.live.capturer.supervised() {
                    s.observe(live.telemetry());
                }
                match self.live.capturer.try_latest() {
                    Ok(Some(f))
                        if source_advanced(first_seq, first_pts, &f.provenance, f.pts_ns) =>
                    {
                        break f
                    }
                    Ok(_) => {
                        if std::time::Instant::now() >= live_deadline {
                            tracing::warn!(
                                "eviction recovery: ring re-attached but only the stashed frame \
                                 arrived — the OS is not presenting; failing the in-place recovery"
                            );
                            return None;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"),
                            "eviction recovery: capture failed while waiting for a live frame");
                        return None;
                    }
                }
            }
        } else {
            new_frame
        };
        trace.mark("first_new_frame");
        let (enc, reframe) = match pre_opened {
            Some(e) => (
                e,
                punktfunk_core::video_fit::Reframe::full((new_frame.width, new_frame.height)),
            ),
            None => match open_session_encoder(
                &self.facts.encoder,
                &self.plan,
                &*self.live.capturer,
                &new_frame,
                (new_mode.width, new_mode.height),
                effective_hz,
                |_, _| enc_of.enc_kbps(bitrate_kbps) as u64 * 1000,
                self.bit_depth,
                self.facts.client_hdr,
                wire_seq_base,
            ) {
                Ok(opened) => opened,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"),
                        "resize: encoder open failed after the in-place mode set - full rebuild");
                    return None;
                }
            },
        };
        let interval = std::time::Duration::from_secs_f64(1.0 / effective_hz.max(1) as f64);
        trace.mark("encoder_open");
        Some(Next::Encoder {
            enc,
            reframe,
            resized: Some((new_frame, interval)),
        })
    }
}

/// Has the SOURCE advanced past the first (possibly stash-delivered) frame of a recovery?
/// Sequence evidence where the capturer tracks one (`first_seq != 0`): only a NEW source image
/// advances it — a cursor regeneration or hold re-stamps `pts_ns` over unchanged pixels and must
/// not count. The pts comparison survives solely as the fallback for an untracked capturer.
fn source_advanced(
    first_seq: u64,
    first_pts: u64,
    provenance: &pf_frame::Provenance,
    pts_ns: u64,
) -> bool {
    if first_seq != 0 {
        provenance.source_seq > first_seq
    } else {
        pts_ns != first_pts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The eviction-recovery liveness gate must demand SOURCE progress, not a changed wall-clock
    /// PTS: a cursor regeneration (and a repeat) stamps a fresh `pts_ns` over unchanged source
    /// pixels, which is exactly how a dead presentation path used to "prove" recovery.
    #[test]
    fn recovery_needs_a_new_source_frame_not_a_new_pts() {
        use pf_frame::Provenance;
        // Tracked capturer (IDD): only an ADVANCED source sequence counts…
        assert!(source_advanced(5, 100, &Provenance::source(6, 0), 999));
        // …a regen/hold/stalled-source frame with a fresh pts does not.
        assert!(!source_advanced(5, 100, &Provenance::cursor_regen(5), 999));
        assert!(!source_advanced(5, 100, &Provenance::hold(5), 999));
        assert!(!source_advanced(5, 100, &Provenance::source(5, 0), 999));
        // Untracked capturer (seq 0): the historical pts comparison stands.
        assert!(source_advanced(0, 100, &Provenance::UNTRACKED, 999));
        assert!(!source_advanced(0, 100, &Provenance::UNTRACKED, 100));
    }
}
