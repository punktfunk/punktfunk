//! Map a [`CompositorPref`] to a live `crate::vdisplay::Compositor`, for both planes.
//!
//! [`pick_compositor`] is pure. [`resolve_compositor`] runs the blocking
//! session probes — call it off the async reactor (`spawn_blocking`). An
//! explicit name wins only when that backend is available; `Auto` and a miss
//! fall back to the detected graphical session.
//!
//! [`GamescopeHold`] counts the sessions, on either plane, that may stream a
//! gamescope the host took over. The last one dropped hands Game Mode back.
//!
//! Pin with `PUNKTFUNK_COMPOSITOR`. A pin names a backend, not a running
//! session, and outranks dedicated-launch and auto-follow. Leave it unset
//! except for CI and single-session appliances.
//!
//! Evidence: `design/gamemode-and-dedicated-sessions.md`,
//! `design/gamescope-multiuser.md`.

use anyhow::Result;
use punktfunk_core::config::CompositorPref;
use std::sync::atomic::{AtomicUsize, Ordering};

/// `None` only when nothing is available *and* nothing was detected — the
/// caller turns that into a handshake error.
fn pick_compositor(
    pref: CompositorPref,
    available: &[crate::vdisplay::Compositor],
    detected: Option<crate::vdisplay::Compositor>,
) -> Option<crate::vdisplay::Compositor> {
    use crate::vdisplay::Compositor;
    match Compositor::from_pref(pref) {
        Some(want) if available.contains(&want) => Some(want),
        // Clients before the Hyprland byte sent `Wlroots` for Hyprland too, so it
        // stays the family: the live member, else the first available of the two.
        Some(Compositor::Wlroots) => match detected {
            Some(d @ (Compositor::Wlroots | Compositor::Hyprland)) => Some(d),
            _ => [Compositor::Wlroots, Compositor::Hyprland]
                .into_iter()
                .find(|c| available.contains(c))
                .or(detected),
        },
        _ => detected,
    }
}

/// A pin still appears in [`crate::vdisplay::available`] and skips the
/// `XDG_CURRENT_DESKTOP` scrub, so [`pick_compositor`]'s `None` arm cannot
/// fire. [`Compositor::Gamescope`] is exempt: it stands a session up.
#[cfg(not(target_os = "windows"))]
fn pinned_at_a_dead_session(
    overridden: bool,
    chosen: crate::vdisplay::Compositor,
    live: crate::vdisplay::ActiveKind,
) -> bool {
    overridden && chosen.needs_live_session() && live == crate::vdisplay::ActiveKind::None
}

/// With `recover`, fires the operator recovery hook (debounced) when configured,
/// so a retry a few seconds later can land in a recovered desktop. `pinned` is
/// the `PUNKTFUNK_COMPOSITOR` value when the pin is what got us here, so the
/// message names the knob to change.
#[cfg(not(target_os = "windows"))]
fn no_live_session(pinned: Option<&str>, recover: bool) -> anyhow::Error {
    if recover && crate::vdisplay::try_recover_session() {
        return anyhow::anyhow!(
            "no live graphical session for this uid — host session recovery launched \
             (PUNKTFUNK_RECOVER_SESSION_CMD); retry in a few seconds"
        );
    }
    match pinned {
        Some(pin) => anyhow::anyhow!(
            "PUNKTFUNK_COMPOSITOR={pin} pins this host to a backend that can only attach to an \
             already-running compositor, and no graphical session is live for this uid — start a \
             session, pin `gamescope` (it stands its own up), or set PUNKTFUNK_RECOVER_SESSION_CMD"
        ),
        None => anyhow::anyhow!(
            "no usable compositor (no live graphical session for this uid; set \
             PUNKTFUNK_COMPOSITOR or start a desktop/gaming session)"
        ),
    }
}

/// Isolated planes: own pinned injector, env-routed audio, per-session mic
/// (`design/gamescope-multiuser.md`).
///
/// Only a gamescope bare spawn qualifies — managed/attach are single-occupant,
/// shared-desktop backends want shared planes. `PUNKTFUNK_GAMESCOPE_ISOLATE`
/// turns it off. The resolve paths and `serve_session`'s plane setup must
/// never disagree about this predicate.
#[cfg(not(target_os = "windows"))]
pub(crate) fn session_is_isolated(
    compositor: crate::vdisplay::Compositor,
    route: Option<&crate::vdisplay::GamescopeRoute>,
) -> bool {
    compositor == crate::vdisplay::Compositor::Gamescope
        && matches!(route, Some(crate::vdisplay::GamescopeRoute::Spawn))
        && pf_host_config::config().gamescope_isolate
}

/// Blocking session probes around [`pick_compositor`]. Call off the async
/// reactor (`spawn_blocking`). `isolation_capable` is false for a plane with
/// no per-session injector (GameStream): it always publishes the shared input
/// backend, even for a spawn [`session_is_isolated`] would isolate. `revive` is
/// false on a capture-loss rebuild: it follows a live session and never stands
/// one up, so no managed takeover and no recovery hook mid-switch.
pub(crate) fn resolve_compositor(
    pref: CompositorPref,
    dedicated_launch: bool,
    isolation_capable: bool,
    revive: bool,
) -> Result<(
    crate::vdisplay::Compositor,
    Option<crate::vdisplay::GamescopeRoute>,
)> {
    use crate::vdisplay::Compositor;
    // Windows has one virtual-display backend, so skip the Linux
    // session-detection state machine.
    #[cfg(target_os = "windows")]
    {
        let _ = (pref, dedicated_launch, isolation_capable, revive);
        Ok((Compositor::Windows, None))
    }
    #[cfg(not(target_os = "windows"))]
    {
        // (Re)connect: drop a pending TV-session restore so the box stays in
        // the streamed session. Keep-alive REUSE skips `create_managed_session`'s
        // own cancel.
        crate::vdisplay::cancel_pending_tv_restore();
        // Operator pin: assumed to come with a hand-set env — do not retarget.
        let overridden = pf_host_config::config().compositor.is_some();
        let isolated = |c: Compositor, route: Option<&crate::vdisplay::GamescopeRoute>| {
            isolation_capable && session_is_isolated(c, route)
        };
        // Liveness on both paths. Auto retargets env at the live session; a pin
        // names a backend, not a running session, and skips the
        // `XDG_CURRENT_DESKTOP` scrub, so [`pick_compositor`] cannot return
        // `None` for a dead compositor — `needs_live_session` below is the gate.
        let active = crate::vdisplay::detect_active_session();
        let detected = if overridden {
            crate::vdisplay::detect().ok()
        } else {
            // Detect the live session (Gaming vs Desktop) and retarget process
            // env at it so capture + input open against the active session.
            // If the compositor instance changed since last connect, bump the
            // epoch so this connect never reuses a node id from the dead one.
            crate::vdisplay::observe_session_instance(&active);
            crate::vdisplay::apply_session_env(&active);
            tracing::info!(
                active = ?active.kind,
                wayland = active.env.wayland_display.as_deref().unwrap_or("-"),
                "detected active graphical session"
            );
            crate::vdisplay::compositor_for_kind(active.kind)
        };
        // Dedicated launch (`design/gamemode-and-dedicated-sessions.md`): force
        // a headless gamescope spawn at the client's mode. Env was already
        // retargeted above; pin the backend + input to the spawn. An operator
        // compositor pin still outranks — and must say so, not veto silently.
        if dedicated_launch {
            if overridden {
                // Pin still wins, but never silently: the console still shows
                // `game_session=dedicated` while launches land in the pinned
                // session. The warn below is the only evidence of the veto.
                tracing::warn!(
                    pin = pf_host_config::config()
                        .compositor
                        .as_deref()
                        .unwrap_or("-"),
                    "game_session=dedicated asked for this launch's OWN headless gamescope, but \
                     PUNKTFUNK_COMPOSITOR pins this host to a backend — the operator pin wins and \
                     the game launches into the pinned session instead. Unset PUNKTFUNK_COMPOSITOR \
                     to get dedicated game sessions."
                );
            } else {
                let route = crate::vdisplay::resolve_gamescope_route(Compositor::Gamescope, true);
                // Isolated input goes to its own pinned injector. Do not
                // retarget the shared last-write-wins slot — that steals
                // input from a concurrent shared-desktop viewer.
                if !isolated(Compositor::Gamescope, route.as_ref()) {
                    crate::inject::set_backend_id(crate::vdisplay::input_backend_id(
                        Compositor::Gamescope,
                    ));
                }
                tracing::info!(
                    ?route,
                    "dedicated game session — routing to a headless gamescope spawn at the client \
                     mode"
                );
                return Ok((Compositor::Gamescope, route));
            }
        }
        let available = crate::vdisplay::available();
        let chosen = match pick_compositor(pref, &available, detected) {
            Some(c) => c,
            // No live session, but managed gamescope infra exists: its path
            // stands the session up from nothing. Skip under an operator pin —
            // `PUNKTFUNK_COMPOSITOR` keeps its exact meaning.
            None if revive && !overridden && crate::vdisplay::managed_session_available() => {
                tracing::info!(
                    "no live graphical session — managed gamescope infra present; routing to \
                     the managed takeover to revive the session"
                );
                Compositor::Gamescope
            }
            None => return Err(no_live_session(None, revive)),
        };
        if pinned_at_a_dead_session(overridden, chosen, active.kind) {
            return Err(no_live_session(
                pf_host_config::config().compositor.as_deref(),
                revive,
            ));
        }
        // Resolve the gamescope route on both paths, before input publish: a
        // pin skips the retarget below but still needs a route, and an
        // isolated spawn must not touch the shared injector.
        let route = crate::vdisplay::resolve_gamescope_route(chosen, false);
        // Publish input as a value, not `PUNKTFUNK_INPUT_BACKEND`. Skip on a
        // pin (operator knob stays in charge) and on isolated (own injector).
        if !overridden && !isolated(chosen, route.as_ref()) {
            crate::inject::set_backend_id(crate::vdisplay::input_backend_id(chosen));
        }
        let avail_ids: Vec<&str> = available.iter().map(|c| c.id()).collect();
        match Compositor::from_pref(pref) {
            Some(want) if want == chosen => {
                tracing::info!(
                    compositor = chosen.id(),
                    "honoring client compositor request"
                )
            }
            Some(want) => tracing::warn!(
                requested = want.id(),
                chosen = chosen.id(),
                available = ?avail_ids,
                "client-requested compositor unavailable — falling back to auto-detect"
            ),
            None => tracing::info!(
                compositor = chosen.id(),
                "auto-detected compositor (client: auto)"
            ),
        }
        Ok((chosen, route))
    }
}

/// Live sessions, on either plane, that may stream a gamescope the host took over.
static LIVE_GAMESCOPE: AtomicUsize = AtomicUsize::new(0);

/// Whether any session holds a [`GamescopeHold`]: a held takeover is then streaming, not kept.
pub(crate) fn gamescope_sessions_live() -> bool {
    LIVE_GAMESCOPE.load(Ordering::SeqCst) > 0
}

/// One count in [`LIVE_GAMESCOPE`], taken before the session resolves its compositor. Resolving
/// cancels a pending Game Mode hand-back; the last hold dropped, on any path, schedules it again.
pub(crate) struct GamescopeHold;

impl GamescopeHold {
    pub(crate) fn new() -> Self {
        LIVE_GAMESCOPE.fetch_add(1, Ordering::SeqCst);
        GamescopeHold
    }
}

impl Drop for GamescopeHold {
    fn drop(&mut self) {
        // A `join` session still shows the owner's game after the owner leaves.
        if LIVE_GAMESCOPE.fetch_sub(1, Ordering::SeqCst) == 1 {
            crate::vdisplay::restore_managed_session();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::pick_compositor;
    use punktfunk_core::config::CompositorPref;

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_pin_at_a_dead_session_recovers_instead_of_retrying() {
        use super::pinned_at_a_dead_session as dead;
        use crate::vdisplay::{ActiveKind, Compositor::*};
        assert!(dead(true, Mutter, ActiveKind::None));
        assert!(dead(true, Kwin, ActiveKind::None));
        assert!(dead(true, Wlroots, ActiveKind::None));
        assert!(dead(true, Hyprland, ActiveKind::None));
        assert!(!dead(true, Mutter, ActiveKind::DesktopGnome));
        // Gamescope stands its own session up: pinning it on a headless box
        // is supported, not a dead session.
        assert!(!dead(true, Gamescope, ActiveKind::None));
        // Unpinned: the auto path already reaches `pick_compositor`'s `None`
        // arm via `compositor_for_kind(ActiveKind::None)`.
        assert!(!dead(false, Mutter, ActiveKind::None));
    }

    #[test]
    fn only_gamescope_survives_a_dead_session() {
        use crate::vdisplay::Compositor::*;
        assert!(!Gamescope.needs_live_session());
        for c in [Mutter, Kwin, Wlroots, Hyprland] {
            assert!(c.needs_live_session(), "{c:?} needs a live compositor");
        }
    }

    #[test]
    fn compositor_resolution_precedence() {
        use crate::vdisplay::Compositor::*;
        assert_eq!(
            pick_compositor(CompositorPref::Gamescope, &[Kwin, Gamescope], Some(Kwin)),
            Some(Gamescope)
        );
        assert_eq!(
            pick_compositor(CompositorPref::Mutter, &[Kwin, Gamescope], Some(Kwin)),
            Some(Kwin)
        );
        assert_eq!(
            pick_compositor(CompositorPref::Auto, &[Kwin, Gamescope], Some(Kwin)),
            Some(Kwin)
        );
        assert_eq!(
            pick_compositor(CompositorPref::Mutter, &[Gamescope], None),
            None
        );
        assert_eq!(
            pick_compositor(CompositorPref::Gamescope, &[Gamescope], None),
            Some(Gamescope)
        );
        // `Wlroots` pref is the family: resolve to whichever of sway/river
        // and Hyprland is the live session.
        assert_eq!(
            pick_compositor(
                CompositorPref::Hyprland,
                &[Hyprland, Gamescope],
                Some(Gamescope)
            ),
            Some(Hyprland)
        );
        assert_eq!(
            pick_compositor(CompositorPref::Windows, &[Kwin], Some(Kwin)),
            Some(Kwin)
        );
        assert_eq!(
            pick_compositor(CompositorPref::Wlroots, &[Hyprland], Some(Hyprland)),
            Some(Hyprland)
        );
        assert_eq!(
            pick_compositor(CompositorPref::Wlroots, &[Wlroots], Some(Wlroots)),
            Some(Wlroots)
        );
        assert_eq!(
            pick_compositor(CompositorPref::Wlroots, &[Hyprland], None),
            Some(Hyprland)
        );
    }
}
