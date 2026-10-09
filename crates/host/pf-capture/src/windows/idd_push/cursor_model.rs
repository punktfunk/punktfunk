//! Which pointer the client gets, and who draws it.
//!
//! The GDI poller ([`cursor_poll`]) is the full-fidelity source; the driver's
//! hardware-cursor section ([`cursor`]) is the fallback, latched once taken
//! because the two serial namespaces must not interleave. This file also hands
//! that section to the driver at open, and stands the IddCx declare down for
//! the secure desktop, where a declared hardware cursor would block the OS
//! software-cursor path UAC and Winlogon render through.

use super::*;

impl IddPushCapturer {
    /// Overlay source for [`Capturer::cursor`] and the shape the client draws.
    ///
    /// A live poller wins even while it still reports `None`. Shm is only for a
    /// dead/missing poller, then latched: the two serial namespaces must not interleave.
    pub(super) fn live_cursor(&mut self) -> Option<pf_frame::CursorOverlay> {
        if !self.cursor_shm_latched {
            if let Some(p) = &self.cursor_poll {
                if p.alive() {
                    return p.read();
                }
            }
            // About to read shm — latch so a revived poller cannot recross serials.
            if self.cursor_shared.is_some() {
                self.cursor_shm_latched = true;
                tracing::warn!(
                    target_id = self.target_id,
                    "cursor: the GDI shape poller is not running — degrading to the driver's \
                     hardware-cursor shm section for the rest of the session (alpha-only shapes: \
                     monochrome/masked cursors will look wrong)"
                );
            }
        }
        self.cursor_shared.as_mut().and_then(|c| c.read())
    }

    /// The forward flag the driver should hold now: down while the secure desktop is up, down
    /// while the driver composites, up while the client draws.
    pub(super) fn driver_forward(&self) -> bool {
        !self.secure_active && !self.composite_cursor
    }

    /// UAC/Winlogon use the software-cursor path; a declared IddCx hardware cursor
    /// blocks it. Stand the declare down on the secure edge; on dismissal restore the
    /// model the session runs, which may be the driver compositing.
    /// Must run every tick, including while frames are stalled.
    pub(super) fn poll_secure_desktop(&mut self) {
        let Some(fwd) = self.cursor_forward.as_ref() else {
            return;
        };
        // Channel session, or forced-composite on a reused monitor that may still
        // run an earlier worker. A clean target has no poller — no guard.
        if self.cursor_shared.is_none() && !self.composite_forced {
            return;
        }
        let secure = pf_win_display::secure_desktop();
        if secure == self.secure_active {
            return;
        }
        self.secure_active = secure;
        if secure {
            tracing::info!(
                target_id = self.target_id,
                "secure desktop (UAC/Winlogon) active — standing the IddCx hardware-cursor \
                 declare down so the OS software-cursor path can render it"
            );
            if let Err(e) = fwd(false) {
                tracing::warn!(
                    "secure-desktop cursor-forward stand-down failed (secure content may stay \
                     invisible this session): {e:#}"
                );
            }
        } else {
            tracing::info!(
                target_id = self.target_id,
                "secure desktop dismissed — restoring the cursor render model"
            );
            // Only the session that runs the cursor channel. Forced-composite never
            // wanted the declare; leaving desired-state off stops per-assign re-declares.
            if self.cursor_shared.is_some() {
                if let Err(e) = fwd(self.driver_forward()) {
                    tracing::warn!(
                        "secure-desktop cursor-forward re-enable failed (client-drawn cursor \
                         may double with a composited one): {e:#}"
                    );
                }
            }
        }
    }
}

/// Duplicate `cs` into WUDFHost and `IOCTL_SET_CURSOR_CHANNEL`.
/// `true` = adopted. Idempotent driver-side (replaced worker is stopped).
pub(super) fn deliver_cursor_channel(
    broker: &ChannelBroker,
    target_id: u32,
    cs: &cursor::CursorShared,
    send_cursor: &crate::CursorChannelSender,
) -> bool {
    let value = match broker.dup_into_public(cs.section_handle()) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("cursor section duplication failed (composited cursor stays): {e:#}");
            return false;
        }
    };
    let req = pf_driver_proto::control::SetCursorChannelRequest {
        target_id,
        _pad: 0,
        header_handle: value,
    };
    match send_cursor(&req) {
        Ok(()) => {
            tracing::info!(
                target_id,
                "IDD push(host): cursor channel delivered — driver declares the hardware cursor"
            );
            true
        }
        Err(e) => {
            broker.close_remote_public(value);
            tracing::warn!("cursor channel delivery failed (composited cursor stays): {e:#}");
            false
        }
    }
}
