//! The Windows in-place resize: mode-set the live monitor, restore its presentation, open only
//! the encoder. Also the topology re-assert recovery, which is the same path with a liveness gate.
//! Past the mode set it is [`StreamState::resize_on`], which a test drives on any host.

use super::pipeline::open_session_encoder_with;
use super::state::StreamState;
use super::swap::Next;
use super::*;
use crate::native::geometry::Geometry;

impl StreamState {
    /// Mode-set the live monitor to `intent` and build its geometry from the re-acquired
    /// display's read-back, then [`Self::resize_on`] it. `None` → full rebuild.
    #[cfg(target_os = "windows")]
    pub(super) fn resize(
        &mut self,
        intent: crate::native::geometry::Intent,
        price: impl Fn(&Geometry) -> u32,
        trace: &crate::bringup::Trace,
        recover_ring: bool,
    ) -> Option<Next> {
        use super::pipeline::{display_mode_for, display_rate};
        let cur_target = self.live.capturer.display().map(|d| d.target_id())?;
        let display = display_mode_for(intent.mode(), self.display.vd.name());
        let vout = match crate::vdisplay::registry::acquire(
            &mut self.display.vd,
            display,
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
        if vout.win_capture.as_ref().map(|t| t.target_id) != Some(cur_target) {
            tracing::info!(
                "resize: monitor re-arrived (no in-place support) — running the full pipeline rebuild"
            );
            return None;
        }
        let built = Geometry::built(intent, display_rate(&vout, display));
        self.resize_on(built, price, trace, recover_ring, crate::encode::open_video)
    }

    /// Restore the live monitor's presentation at `built`'s mode and open an encoder through
    /// `open_video` at the first frame of the new size, framed as its intent says, at `price` the
    /// geometry it encodes, for the rebuild performer to swap in. `None` → full rebuild.
    /// With the driver's encoder, only its pool taking a frame proves the new size; a second
    /// without one restarts the presentation once. `recover_ring` re-attaches the ring at the
    /// current mode and demands a second, newer present as proof the OS resumed presenting.
    pub(super) fn resize_on(
        &mut self,
        built: Geometry,
        price: impl Fn(&Geometry) -> u32,
        trace: &crate::bringup::Trace,
        recover_ring: bool,
        mut open_video: impl FnMut(
            &crate::encode::Resolved,
            crate::encode::OpenParams,
        ) -> Result<Box<dyn crate::encode::Encoder>>,
    ) -> Option<Next> {
        let (intent, enc_of) = (built.intent(), self.enc_now());
        let new_mode = intent.mode();
        let restored = match self.live.capturer.display() {
            Some(d) if recover_ring => d.restart_presentation(),
            Some(d) => d.resize(new_mode.width, new_mode.height),
            None => false,
        };
        if !restored {
            return None;
        }
        trace.mark("presentation_restored");
        let wire_seq_base = self.clock.next_au();
        let open = |st: &Self, frame: &crate::capture::CapturedFrame, open_video: &mut _| {
            open_session_encoder_with(
                &st.facts.terms.encoder,
                &st.plan,
                &*st.live.capturer,
                frame,
                intent.framing(),
                built.hz(),
                |f| enc_of.enc_kbps(price(&built.reframed(intent, *f))) as u64 * 1000,
                st.bit_depth,
                st.facts.terms.client_hdr(),
                wire_seq_base,
                open_video,
            )
        };
        let open_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        // The driver's pool is still built for the OLD geometry, so no composed frame passes until
        // this SET_ENCODE rebuilds it — the frame wait below cannot come first. A re-arrival gets its
        // swap chain after the arrival, so an open in that window fails for a display about to be
        // fine: retry to its own deadline rather than drop the resize to a full rebuild.
        let pre_opened = if self.plan.capture == crate::session_plan::CaptureBackend::IddPush {
            // The driver delivers the new size without pixels: the open reads only its size.
            let at = crate::capture::CapturedFrame {
                width: new_mode.width,
                height: new_mode.height,
                pts_ns: 0,
                format: self.live.frame.format,
                payload: pf_frame::FramePayload::Pixelless,
                cursor: None,
                provenance: pf_frame::Provenance::UNTRACKED,
            };
            let opened = loop {
                match open(self, &at, &mut open_video) {
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
        let live: &dyn crate::encode::Encoder = pre_opened
            .as_ref()
            .map_or(&*self.live.enc, |(enc, _)| &**enc);
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
            Some(opened) => opened,
            None => match open(self, &new_frame, &mut open_video) {
                Ok(opened) => opened,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"),
                        "resize: encoder open failed after the in-place mode set - full rebuild");
                    return None;
                }
            },
        };
        trace.mark("encoder_open");
        Some(Next::Encoder {
            enc,
            geometry: built.reframed(intent, reframe),
            resized: Some(new_frame),
        })
    }
}

/// Has the SOURCE advanced past the first (possibly stash-delivered) frame of a recovery?
/// Sequence evidence where the capturer tracks one (`first_seq != 0`): only a NEW source image
/// advances it — a cursor regeneration or hold re-stamps `pts_ns` over unchanged pixels and must
/// not count. The pts comparison is only the fallback for an untracked capturer.
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
    /// pixels, which would let a dead presentation path "prove" recovery.
    #[test]
    fn recovery_needs_a_new_source_frame_not_a_new_pts() {
        use pf_frame::Provenance;
        // Tracked capturer (IDD): only an ADVANCED source sequence counts…
        assert!(source_advanced(5, 100, &Provenance::source(6, 0), 999));
        // …a regen/hold/stalled-source frame with a fresh pts does not.
        assert!(!source_advanced(5, 100, &Provenance::cursor_regen(5), 999));
        assert!(!source_advanced(5, 100, &Provenance::hold(5), 999));
        assert!(!source_advanced(5, 100, &Provenance::source(5, 0), 999));
        // Untracked capturer (seq 0): the pts comparison stands.
        assert!(source_advanced(0, 100, &Provenance::UNTRACKED, 999));
        assert!(!source_advanced(0, 100, &Provenance::UNTRACKED, 100));
    }
}
