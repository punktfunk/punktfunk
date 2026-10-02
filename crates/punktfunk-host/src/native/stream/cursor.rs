//! The pointer on the stream: forwarded to a client that draws it, or handed to the encoder
//! blend as a host composite; on Linux, the seat-pointer park that keeps it on the streamed
//! output at all.

use super::state::StreamState;
use super::*;

/// `(gamescope_composite, metadata_composite)` for the live compositor. Shared by bring-up and
/// mid-stream retarget so they cannot drift. `gamescope` is the live compositor, not the original.
pub(super) fn composite_plan(
    plan: &crate::session_plan::SessionPlan,
    has_cursor_channel: bool,
    gamescope: bool,
) -> (bool, bool) {
    (
        plan.gamescope_cursor && !has_cursor_channel,
        !has_cursor_channel && plan.cursor_blend && !gamescope,
    )
}

/// Settle the cursor plan against the portal's negotiated mode. Shared by bring-up and capture-loss
/// rebuild so the two cannot drift.
///
/// Returns whether "no cursor overlay" still means the pointer is off the streamed output, and
/// clears `metadata_composite` when the negotiated mode makes it a fiction. wlr portals
/// (`Hidden|Embedded`) paint the pointer into the frames and never send `SPA_META_Cursor`, so a
/// metadata composite can never be fed and "no overlay" is not a park signal. `None` (KWin,
/// Mutter, gamescope, Windows) leaves both answers alone.
///
/// Does not undo [`SessionPlan::cursor_blend`]: blend is resolved before `create`.
#[cfg(target_os = "linux")]
pub(super) fn settle_portal_cursor(
    vd: &dyn crate::vdisplay::VirtualDisplay,
    metadata_composite: &mut bool,
) -> bool {
    let Some(negotiated) = vd.last_portal_cursor_mode() else {
        return true;
    };
    if negotiated.delivers_metadata() {
        return true;
    }
    if *metadata_composite {
        *metadata_composite = false;
        tracing::info!(
            negotiated = negotiated.name(),
            "the portal negotiated a cursor mode that carries no cursor metadata — dropping the \
             host composite; the pointer in this stream is the compositor's own, burnt into the \
             frames"
        );
    }
    false
}

/// Park the seat pointer at the streamed surface's centre, through the same injection path
/// client input takes.
///
/// A fresh virtual output leaves the seat wherever it last was. A relative-only client never
/// moves it onto the streamed output, so input lands on the wrong monitor and Mutter suppresses
/// `SPA_META_Cursor` while the pointer is off the recorded view. Retry only for relative-only
/// clients: a desktop-model client's own `MouseMoveAbs` is the retry, and a synthetic one fights it.
#[cfg(target_os = "linux")]
fn park_pointer(
    input_tx: &std::sync::mpsc::SyncSender<super::super::input::ClientInput>,
    w: u32,
    h: u32,
) {
    let ev = punktfunk_core::input::InputEvent {
        kind: punktfunk_core::input::InputKind::MouseMoveAbs,
        _pad: [0; 3],
        code: 0,
        x: (w / 2) as i32,
        y: (h / 2) as i32,
        flags: (w << 16) | (h & 0xffff),
    };
    // Best-effort: never block the stream loop behind a full input backlog.
    if input_tx
        .try_send(super::super::input::ClientInput::Event(ev))
        .is_ok()
    {
        tracing::info!(
            w,
            h,
            "parked the seat pointer at the streamed surface's centre"
        );
    }
}

/// Which host-composite outcomes have been logged; each is said once per session.
#[derive(Default)]
pub(super) struct CompositeLog {
    pub(super) saw_overlay: bool,
    pub(super) saw_none: bool,
}

/// A relative-only session retries: the first park can hit a cold EIS whose devices have not
/// resumed. One park for a client that steers absolutely — more is a yank to centre.
#[cfg(target_os = "linux")]
const PARK_ATTEMPTS_MAX: u32 = 10;

impl StreamState {
    /// Re-derive the cursor plan for compositor `c` on `route` before the next pipeline is built
    /// from it: blend, gamescope reader, both composite flags. Returns whether the display asks
    /// for a metadata cursor ([`crate::vdisplay::VirtualDisplay::set_hw_cursor`]). Shared by the
    /// capture-loss retarget and the session switch so neither keeps the old compositor's plan.
    pub(super) fn retarget_cursor_plan(
        &mut self,
        c: crate::vdisplay::Compositor,
        route: Option<&crate::vdisplay::GamescopeRoute>,
    ) -> bool {
        let gamescope = c == crate::vdisplay::Compositor::Gamescope;
        self.plan.cursor_blend = crate::session_plan::cursor_blend_for(
            self.plan.cursor_forward,
            c,
            self.plan.codec,
            self.plan.bit_depth,
            self.plan.hdr,
            route,
        );
        self.plan.gamescope_cursor = crate::session_plan::gamescope_cursor_for(gamescope, route);
        self.plan.sdr10_native = crate::session_plan::sdr10_native_for(&self.plan, c, route);
        (self.gamescope_composite, self.metadata_composite) =
            composite_plan(&self.plan, self.cursor_fwd.is_some(), gamescope);
        self.plan.cursor_forward || self.metadata_composite
    }

    /// Route this tick's cursor: forward it when the client draws, else put the live overlay on
    /// the frame for the encoder blend. Either way the capturer hears the host places it, so it
    /// bakes no second copy. An invisible cursor never reaches the encoder.
    pub(super) fn tick_cursor(&mut self) {
        if let Some(fwd) = self.cursor_fwd.as_mut() {
            let client_draws = self.cursor_client_draws.load(Ordering::Relaxed);
            self.capturer.set_cursor_forward(client_draws);
            if client_draws != self.cursor_client_drew {
                self.cursor_client_drew = client_draws;
                tracing::info!(
                    client_draws,
                    "cursor render mode flipped ({})",
                    if client_draws {
                        "client draws — exclude + forward"
                    } else {
                        "host composites"
                    }
                );
                #[cfg(target_os = "linux")]
                if !client_draws {
                    self.park_attempts = 0;
                    self.next_park_at = std::time::Instant::now();
                }
            }
            if client_draws {
                let live = self.capturer.cursor();
                let reframe = *self.frame_map.lock().unwrap_or_else(|e| e.into_inner());
                fwd.tick(
                    live.as_ref().or(self.frame.cursor.as_ref()),
                    &reframe,
                    &self.conn,
                    &self.cursor_shape_tx,
                );
                self.frame.cursor = None;
            } else {
                #[cfg(not(target_os = "windows"))]
                self.composite_live_cursor(
                    "host-composite active but the capture has no live cursor overlay (no \
                     SPA_META_Cursor bitmap) — nothing for the encoder blend to draw; the \
                     pointer, if any, is the compositor's own",
                );
            }
        } else if self.gamescope_composite || self.metadata_composite {
            #[cfg(not(target_os = "windows"))]
            {
                self.capturer.set_cursor_forward(false);
                self.composite_live_cursor(
                    "host-composite active but the capture has no live cursor overlay yet (no \
                     SPA_META_Cursor bitmap) — the stream is cursorless until one arrives",
                );
            }
        }
        if self.frame.cursor.as_ref().is_some_and(|c| !c.visible) {
            self.frame.cursor = None;
        }
    }

    /// Hand the capture's live overlay to the encoder blend. Each of the two outcomes is logged once.
    #[cfg(not(target_os = "windows"))]
    fn composite_live_cursor(&mut self, none_msg: &'static str) {
        match self.capturer.cursor() {
            Some(live) => {
                if !self.composite_log.saw_overlay {
                    self.composite_log.saw_overlay = true;
                    tracing::info!(
                        x = live.x,
                        y = live.y,
                        w = live.w,
                        h = live.h,
                        visible = live.visible,
                        "host-composite: first live cursor overlay handed to the encoder blend"
                    );
                }
                self.frame.cursor = Some(live);
            }
            None => {
                if !self.composite_log.saw_none {
                    self.composite_log.saw_none = true;
                    tracing::info!("{none_msg}");
                }
            }
        }
    }

    /// The park schedule (see [`park_pointer`]): per (re)built display, re-armed by the
    /// capture-model flip. Unconditional parks first, then only while the composite is starved
    /// and "no overlay" means the pointer is off the output.
    #[cfg(target_os = "linux")]
    pub(super) fn park_seat_pointer(&mut self) {
        if self.compositor == pf_vdisplay::Compositor::Gamescope
            || self.park_attempts >= PARK_ATTEMPTS_MAX
            || std::time::Instant::now() < self.next_park_at
        {
            return;
        }
        let client_steers =
            self.cursor_fwd.is_some() && self.cursor_client_draws.load(Ordering::Relaxed);
        let unconditional = if client_steers { 1 } else { 2 };
        let composite_starved = ((self.cursor_fwd.is_some() && !client_steers)
            || self.metadata_composite)
            && self.capturer.cursor().is_none()
            && self.no_overlay_means_off_output;
        if self.park_attempts < unconditional || composite_starved {
            park_pointer(&self.input_tx, self.frame.width, self.frame.height);
            self.park_attempts += 1;
            self.next_park_at = std::time::Instant::now() + std::time::Duration::from_secs(1);
        } else {
            self.park_attempts = PARK_ATTEMPTS_MAX;
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn an_embedded_portal_voids_both_the_composite_and_the_starvation_signal() {
        struct Fake(Option<pf_vdisplay::PortalCursorMode>);
        impl crate::vdisplay::VirtualDisplay for Fake {
            fn name(&self) -> &'static str {
                "fake"
            }
            fn create(
                &mut self,
                _mode: pf_vdisplay::Mode,
            ) -> anyhow::Result<crate::vdisplay::VirtualOutput> {
                anyhow::bail!("this test never creates a display")
            }
            fn last_portal_cursor_mode(&self) -> Option<pf_vdisplay::PortalCursorMode> {
                self.0
            }
        }

        let mut composite = true;
        assert!(!settle_portal_cursor(
            &Fake(Some(pf_vdisplay::PortalCursorMode::Embedded)),
            &mut composite
        ));
        assert!(!composite, "the composite can never be fed — drop it");

        let mut composite = true;
        assert!(!settle_portal_cursor(
            &Fake(Some(pf_vdisplay::PortalCursorMode::Hidden)),
            &mut composite
        ));
        assert!(!composite);

        let mut composite = true;
        assert!(settle_portal_cursor(
            &Fake(Some(pf_vdisplay::PortalCursorMode::Metadata)),
            &mut composite
        ));
        assert!(composite);
    }
}
