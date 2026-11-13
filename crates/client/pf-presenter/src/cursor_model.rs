//! The cursor decisions behind the run loop's `cursor_tick`: who draws the pointer, when
//! the host's relative hint flips the mouse model, and when the local cursor follows a
//! pointer the host moved. Pure: the loop gathers [`Inputs`] from SDL and the capture,
//! asks [`CursorModel::decide`], applies the flip, gathers again and asks
//! [`CursorModel::follow`]. The second gather reads the capture after the flip.
//!
//! Pinned by the table test below, one row per fix it carries.

use std::time::{Duration, Instant};

/// Outlasts the pointer hide Windows does for a click, short against a game's grab.
pub(crate) const HINT_SETTLE: Duration = Duration::from_millis(250);

/// Past a round trip: the host's echo of the user's own motion is never someone else's.
pub(crate) const FOLLOW_HOST_AFTER: Duration = Duration::from_millis(250);

/// How long motion events after a follow-warp are its echo, not the user.
pub(crate) const WARP_ECHO: Duration = Duration::from_millis(50);

/// Rounding between window and frame pixels; a smaller difference is the same spot.
pub(crate) const FOLLOW_SLACK_PX: i32 = 2;

/// The capture as a decision reads it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CaptureFacts {
    pub(crate) captured: bool,
    pub(crate) desktop: bool,
    pub(crate) buttons_held: bool,
    /// The session holds the pointer grant.
    pub(crate) pointer: bool,
    /// The host takes absolute input, so a flip to desktop can land.
    pub(crate) abs_ok: bool,
    /// Where the client last put the host's pointer, in frame pixels.
    pub(crate) last_abs: Option<(i32, i32)>,
}

/// The host's forwarded cursor state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HostCursor {
    pub(crate) relative_hint: bool,
    pub(crate) visible: bool,
    /// Frame pixels.
    pub(crate) pos: (i32, i32),
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Inputs {
    /// The cursor channel is negotiated: the host takes `client_draws`.
    pub(crate) negotiated: bool,
    pub(crate) host: Option<HostCursor>,
    pub(crate) capture: Option<CaptureFacts>,
    /// SDL's pointer focus is the stream window.
    pub(crate) over_us: bool,
    pub(crate) ring_open: bool,
    /// A frame is shown, so a host position maps onto the window.
    pub(crate) video: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Actions {
    /// Tell the host whether the client draws the pointer.
    pub(crate) client_draws: Option<bool>,
    /// The settled hint asks for the other mouse model (`true` = desktop). The capture
    /// applies it where the host takes absolute input; a flip to desktop warps the pointer
    /// to the host's.
    pub(crate) want_desktop: Option<bool>,
}

pub(crate) struct CursorModel {
    /// Auto-flip fires on changes only, so it never fights a user who chorded away.
    last_hint: Option<bool>,
    /// When `last_hint` last changed; the flip waits out [`HINT_SETTLE`] from here.
    hint_since: Instant,
    /// The user flipped the model by chord. The standing hint stops driving until the
    /// host's intent next changes.
    hint_override: bool,
    /// When the user last moved the mouse.
    last_user_motion: Instant,
    /// Motion events before this are the echo of a follow-warp.
    warp_echo_until: Instant,
    /// Last `client_draws` told to the host; `None` = nothing sent yet. Edge-detected so
    /// chord, auto-flip, and engage/release share one path.
    sent_client_draws: Option<bool>,
    /// The hint's flip to relative came under a held button: a game's press-to-look. It
    /// flips back the moment the pointer returns.
    press_grab: bool,
    /// Desktop motion went out as deltas under a press; the local cursor rejoins the
    /// host's pointer when it returns.
    rejoin: bool,
}

impl CursorModel {
    pub(crate) fn new(now: Instant) -> CursorModel {
        CursorModel {
            last_hint: None,
            hint_since: now,
            hint_override: false,
            last_user_motion: now,
            warp_echo_until: now,
            sent_client_draws: None,
            press_grab: false,
            rejoin: false,
        }
    }

    /// Desktop motion goes out as deltas while a host app hides the pointer under a held
    /// button and the settle has not flipped the model yet: positions would spin a camera
    /// that recentres the pointer each frame. Once started they last until the pointer
    /// returns, past the release.
    pub(crate) fn takes_deltas(&mut self, desktop: bool, buttons_held: bool) -> bool {
        let deltas = desktop
            && (buttons_held || self.rejoin)
            && !self.hint_override
            && self.last_hint == Some(true);
        self.rejoin |= deltas;
        deltas
    }

    /// The mouse moved. The echo of our own follow-warp is not the user moving.
    pub(crate) fn user_moved(&mut self, now: Instant) {
        if now >= self.warp_echo_until {
            self.last_user_motion = now;
        }
    }

    /// A manual flip outranks the standing hint until the host's intent next changes.
    pub(crate) fn manual_flip(&mut self) {
        self.hint_override = true;
    }

    /// Who draws the pointer, and the mouse model the host's hint asks for.
    pub(crate) fn decide(&mut self, i: &Inputs, now: Instant) -> Actions {
        let mut a = Actions::default();
        // We draw while released (a released cursor over a composited one is a frozen twin),
        // in desktop mode, or relative on the hint until its flip back lands: a compositing
        // host sends no state, so the hint would never clear and the flip would warp to a
        // stale spot. Without the pointer grant the host draws.
        let client_draws = i.capture.is_none_or(|c| {
            let hint_relative =
                !self.hint_override && self.last_hint.is_some_and(|h| h || c.abs_ok);
            (!c.captured || c.desktop || hint_relative) && c.pointer
        });
        if i.negotiated && self.sent_client_draws != Some(client_draws) {
            self.sent_client_draws = Some(client_draws);
            a.client_draws = Some(client_draws);
        }
        let Some(h) = i.host else {
            return a;
        };
        // Edge-triggered so a manual chord is not fought.
        if self.last_hint != Some(h.relative_hint) {
            self.last_hint = Some(h.relative_hint);
            self.hint_override = false;
            self.hint_since = now;
        }
        // A hide must hold (Windows hides the pointer for a click or a keystroke), and a grab
        // needs the pointer over this window. The flip back waits for no button down, but a
        // press-to-look lets go at once.
        let settled = now.saturating_duration_since(self.hint_since) >= HINT_SETTLE;
        let over = !h.relative_hint || i.over_us;
        if let Some(c) = i.capture.filter(|_| !self.hint_override) {
            let ready = if h.relative_hint {
                settled
            } else {
                self.press_grab || settled && !c.buttons_held
            };
            let flips = c.desktop == h.relative_hint;
            if flips && ready && c.captured && over && !i.ring_open {
                self.press_grab = h.relative_hint && c.buttons_held;
                self.rejoin = false;
                a.want_desktop = Some(!h.relative_hint);
            }
        }
        a
    }

    /// Whether the local cursor goes where the host put the pointer. Something else moved
    /// it (controller mouse, a trackpad gesture, an app warping it) and the user's own mouse
    /// has been still for longer than a round trip, or the pointer returns after
    /// [`Self::takes_deltas`] moved only the host's.
    pub(crate) fn follow(&mut self, i: &Inputs, now: Instant) -> bool {
        let (Some(h), Some(c), true) = (i.host, i.capture, i.video) else {
            return false;
        };
        let rejoin = !h.relative_hint && std::mem::take(&mut self.rejoin);
        let still = now.saturating_duration_since(self.last_user_motion) >= FOLLOW_HOST_AFTER;
        let drifted = c.last_abs.is_some_and(|(x, y)| {
            (x - h.pos.0).abs() > FOLLOW_SLACK_PX || (y - h.pos.1).abs() > FOLLOW_SLACK_PX
        });
        let follow =
            c.captured && c.desktop && h.visible && i.over_us && (rejoin || still && drifted);
        if follow {
            self.warp_echo_until = now + WARP_ECHO;
        }
        follow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_AT: (i32, i32) = (100, 100);

    fn captured(desktop: bool) -> CaptureFacts {
        CaptureFacts {
            captured: true,
            desktop,
            buttons_held: false,
            pointer: true,
            abs_ok: true,
            last_abs: Some(HOST_AT),
        }
    }

    fn inputs(capture: CaptureFacts, relative_hint: bool) -> Inputs {
        Inputs {
            negotiated: true,
            host: Some(HostCursor {
                relative_hint,
                visible: true,
                pos: HOST_AT,
            }),
            capture: Some(capture),
            over_us: true,
            ring_open: false,
            video: true,
        }
    }

    /// One step of a row: a tick at `ms`, the user moving the mouse, or the chord.
    enum Op {
        Tick {
            ms: u64,
            inputs: Inputs,
            want: Actions,
            follow: bool,
        },
        Moved(u64),
        Chord,
        /// A motion event with the button held or not, and whether it goes out as deltas.
        Motion(bool, bool),
    }

    fn tick(ms: u64, inputs: Inputs, want: Actions, follow: bool) -> Op {
        Op::Tick {
            ms,
            inputs,
            want,
            follow,
        }
    }

    fn act(client_draws: Option<bool>, want_desktop: Option<bool>) -> Actions {
        Actions {
            client_draws,
            want_desktop,
        }
    }

    /// Runs a row as the run loop does: decide, apply the flip, then follow.
    fn run(fix: &str, ops: &[Op]) {
        let t0 = Instant::now();
        let mut m = CursorModel::new(t0);
        for (n, op) in ops.iter().enumerate() {
            match op {
                Op::Moved(ms) => m.user_moved(t0 + Duration::from_millis(*ms)),
                Op::Chord => m.manual_flip(),
                Op::Motion(held, deltas) => {
                    assert_eq!(
                        m.takes_deltas(true, *held),
                        *deltas,
                        "{fix}: step {n} deltas"
                    );
                }
                Op::Tick {
                    ms,
                    inputs,
                    want,
                    follow,
                } => {
                    let now = t0 + Duration::from_millis(*ms);
                    let a = m.decide(inputs, now);
                    assert_eq!(a, *want, "{fix}: step {n}");
                    let mut after = *inputs;
                    if let (Some(c), Some(d)) = (after.capture.as_mut(), a.want_desktop) {
                        c.desktop = d;
                    }
                    assert_eq!(m.follow(&after, now), *follow, "{fix}: step {n} follow");
                }
            }
        }
    }

    #[test]
    fn every_cursor_fix_holds() {
        let drifted = CaptureFacts {
            last_abs: Some((HOST_AT.0 + 3, HOST_AT.1)),
            ..captured(true)
        };
        let not_over = Inputs {
            over_us: false,
            ..inputs(captured(true), true)
        };
        let held = CaptureFacts {
            buttons_held: true,
            ..captured(true)
        };
        let held_relative = CaptureFacts {
            buttons_held: true,
            ..captured(false)
        };
        let ring = Inputs {
            ring_open: true,
            ..inputs(captured(true), true)
        };
        let released = CaptureFacts {
            captured: false,
            ..captured(false)
        };
        let released_no_grant = CaptureFacts {
            pointer: false,
            ..released
        };
        let desktop_no_grant = CaptureFacts {
            pointer: false,
            ..captured(true)
        };
        let no_abs = CaptureFacts {
            abs_ok: false,
            ..captured(false)
        };
        let table: Vec<(&str, Vec<Op>)> = vec![
            (
                "8c565ba17 flip the mouse model only on a settled host hint",
                vec![
                    tick(
                        0,
                        inputs(captured(true), true),
                        act(Some(true), None),
                        false,
                    ),
                    tick(249, inputs(captured(true), true), act(None, None), false),
                    tick(250, not_over, act(None, None), false),
                    tick(250, ring, act(None, None), false),
                    tick(
                        250,
                        inputs(captured(true), true),
                        act(None, Some(false)),
                        false,
                    ),
                    // Releasing to desktop needs no pointer over the window; it waits for the
                    // buttons.
                    tick(300, inputs(captured(false), false), act(None, None), false),
                    tick(550, inputs(held_relative, false), act(None, None), false),
                    tick(
                        550,
                        Inputs {
                            over_us: false,
                            ..inputs(captured(false), false)
                        },
                        act(None, Some(true)),
                        false,
                    ),
                ],
            ),
            (
                "a pointer hidden under a held button is a press-to-look",
                vec![
                    tick(0, inputs(held, true), act(Some(true), None), false),
                    Op::Motion(true, true),
                    tick(249, inputs(held, true), act(None, None), false),
                    tick(250, inputs(held, true), act(None, Some(false)), false),
                    tick(300, inputs(held_relative, true), act(None, None), false),
                    // The pointer is back: no settle, and the press need not end.
                    tick(
                        400,
                        inputs(held_relative, false),
                        act(None, Some(true)),
                        false,
                    ),
                ],
            ),
            (
                "a flick shorter than the settle rejoins the host's pointer",
                vec![
                    tick(
                        0,
                        inputs(captured(true), true),
                        act(Some(true), None),
                        false,
                    ),
                    // A hide without a press stays on positions.
                    Op::Motion(false, false),
                    Op::Motion(true, true),
                    Op::Moved(100),
                    // Released, but the host has not shown the pointer yet.
                    Op::Motion(false, true),
                    tick(150, inputs(captured(true), true), act(None, None), false),
                    // Neither still nor drifted: the rejoin alone warps.
                    tick(160, inputs(captured(true), false), act(None, None), true),
                    Op::Motion(false, false),
                    tick(170, inputs(captured(true), false), act(None, None), false),
                ],
            ),
            (
                "keep drawing until the hint's flip back to desktop lands",
                vec![
                    tick(
                        0,
                        inputs(captured(true), true),
                        act(Some(true), None),
                        false,
                    ),
                    tick(
                        250,
                        inputs(captured(true), true),
                        act(None, Some(false)),
                        false,
                    ),
                    // The pointer is back. Composited, the host would stop its state, and the
                    // flip below would warp to where the pointer reappeared.
                    tick(300, inputs(captured(false), false), act(None, None), false),
                    tick(310, inputs(captured(false), false), act(None, None), false),
                    tick(
                        550,
                        inputs(captured(false), false),
                        act(None, Some(true)),
                        false,
                    ),
                ],
            ),
            (
                "a host without absolute input composites once the hint clears",
                vec![
                    tick(0, inputs(no_abs, true), act(Some(false), None), false),
                    tick(10, inputs(no_abs, true), act(Some(true), None), false),
                    tick(20, inputs(no_abs, false), act(None, None), false),
                    tick(30, inputs(no_abs, false), act(Some(false), None), false),
                ],
            ),
            (
                "780dc6fd5 follow host-driven pointer motion in desktop mode",
                vec![
                    tick(0, inputs(drifted, false), act(Some(true), None), false),
                    Op::Moved(100),
                    tick(349, inputs(drifted, false), act(None, None), false),
                    tick(350, inputs(drifted, false), act(None, None), true),
                    // Motion inside the warp's echo is not the user: still still.
                    Op::Moved(380),
                    tick(380, inputs(drifted, false), act(None, None), true),
                    // Within the slack it is the same spot.
                    tick(400, inputs(captured(true), false), act(None, None), false),
                ],
            ),
            (
                "27f12aab6 keep cursor state flowing in hint-driven capture",
                vec![
                    tick(
                        0,
                        inputs(captured(false), true),
                        act(Some(false), None),
                        false,
                    ),
                    // The hint seen last tick makes relative capture a client draw.
                    tick(
                        10,
                        inputs(captured(false), true),
                        act(Some(true), None),
                        false,
                    ),
                    Op::Chord,
                    tick(
                        20,
                        inputs(captured(false), true),
                        act(Some(false), None),
                        false,
                    ),
                ],
            ),
            (
                "6c17c4bbf let the host draw for a guest without pointer access",
                vec![
                    tick(0, inputs(released, false), act(Some(true), None), false),
                    tick(
                        10,
                        inputs(released_no_grant, false),
                        act(Some(false), None),
                        false,
                    ),
                    tick(20, inputs(desktop_no_grant, false), act(None, None), false),
                ],
            ),
        ];
        for (fix, ops) in &table {
            run(fix, ops);
        }
    }

    #[test]
    fn a_chord_holds_until_the_hint_next_changes() {
        let t0 = Instant::now();
        let mut m = CursorModel::new(t0);
        let at = |ms| t0 + Duration::from_millis(ms);
        m.decide(&inputs(captured(true), true), at(0));
        m.manual_flip();
        let a = m.decide(&inputs(captured(true), true), at(500));
        assert_eq!(a.want_desktop, None);
        m.decide(&inputs(captured(true), false), at(600));
        let a = m.decide(&inputs(captured(false), false), at(850));
        assert_eq!(a.want_desktop, Some(true));
    }
}
