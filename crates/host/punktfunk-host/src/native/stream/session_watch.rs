//! Following a mid-stream Gaming↔Desktop flip.
//!
//! Bazzite and SteamOS switch session under a live stream, which changes the compositor and the
//! capture backend. The watcher only signals ([`SessionSwitch`]); the env is applied on the
//! encode thread, so nothing here ever `setenv`s. A Linux seat switches by itself and signals
//! the same way ([`SessionSwitch::seat`]).

use super::*;

/// Mid-stream Gaming↔Desktop flip. Env is applied on the encode thread — the watcher never `setenv`s.
pub(super) struct SessionSwitch {
    pub(super) kind: crate::vdisplay::ActiveKind,
    pub(super) compositor: crate::vdisplay::Compositor,
    /// The new session's env. `None` on a seat: its desktop keeps running under Game Mode.
    pub(super) env: Option<crate::vdisplay::SessionEnv>,
    /// What the new display starts. Only a seat entering Game Mode starts anything.
    pub(super) launch: Option<String>,
}

impl SessionSwitch {
    /// A seat's switch (`seats/session_switch.rs`): its own gamescope with Steam, or its desktop.
    #[cfg(target_os = "linux")]
    pub(super) fn seat(game: bool) -> Self {
        use crate::vdisplay::{ActiveKind, Compositor};
        SessionSwitch {
            kind: if game {
                ActiveKind::Gaming
            } else {
                ActiveKind::DesktopKde
            },
            compositor: if game {
                Compositor::Gamescope
            } else {
                Compositor::Kwin
            },
            env: None,
            launch: game
                .then(crate::library::game_mode_launch)
                .flatten()
                .and_then(|t| t.command),
        }
    }
}

/// The session's `PUNKTFUNK_SESSION_WATCH` (`row`) on/off wins. Auto is on where the box has a
/// Game Mode to flip to mid-stream, and off elsewhere.
pub(super) fn session_watch_enabled(row: Option<bool>) -> bool {
    row.unwrap_or_else(has_game_mode)
}

/// Bazzite, SteamOS, or any box with a gamescope session (Nobara). Only the watcher removes the
/// takeover's idle drop-in when a capture-loss rebuild reaches the desktop first.
fn has_game_mode() -> bool {
    let os = pf_host_config::os_release::os_release();
    os.is("bazzite") || os.is("steamos") || crate::vdisplay::managed_session_available()
}

pub(super) fn session_watcher_loop(
    tx: std::sync::mpsc::Sender<SessionSwitch>,
    stop: Arc<AtomicBool>,
) {
    use crate::vdisplay;
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(3);
    let mut current = vdisplay::detect_active_session().kind;
    let mut pending: Option<(vdisplay::ActiveKind, std::time::Instant)> = None;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let active = vdisplay::detect_active_session();
        // Kind change OR same-kind restart: bump the epoch even when no SessionSwitch will fire.
        vdisplay::observe_session_instance(&active);
        let cur = active.kind;
        if cur == current {
            pending = None;
            continue;
        }
        match pending {
            Some((k, since)) if k == cur && since.elapsed() >= DEBOUNCE => {
                match vdisplay::compositor_for_kind(cur) {
                    Some(comp) => {
                        tracing::info!(from = ?current, to = ?cur, compositor = comp.id(),
                            "session watcher: mid-stream switch — signaling backend rebuild");
                        if tx
                            .send(SessionSwitch {
                                kind: cur,
                                compositor: comp,
                                env: Some(active.env),
                                launch: None,
                            })
                            .is_err()
                        {
                            break;
                        }
                        current = cur;
                    }
                    None => tracing::debug!(to = ?cur,
                        "session watcher: no usable backend for the new session — staying put"),
                }
                // After the signal: it waits out our Steam quitting. Also for a switch we cannot
                // follow, or "Return to Gaming Mode" meets a masked autologin unit.
                vdisplay::release_autologin_mask(cur);
                pending = None;
            }
            Some((k, _)) if k == cur => {}
            _ => pending = Some((cur, std::time::Instant::now())),
        }
    }
}
