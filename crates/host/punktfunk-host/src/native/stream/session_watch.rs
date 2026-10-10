//! Following a mid-stream Gaming↔Desktop flip.
//!
//! Bazzite and SteamOS switch session under a live stream, which changes the compositor and the
//! capture backend. The watcher only signals ([`SessionSwitch`]); the env is applied on the
//! encode thread, so nothing here ever `setenv`s.

use super::*;

/// Mid-stream Gaming↔Desktop flip. Env is applied on the encode thread — the watcher never `setenv`s.
pub(super) struct SessionSwitch {
    pub(super) kind: crate::vdisplay::ActiveKind,
    pub(super) compositor: crate::vdisplay::Compositor,
    pub(super) env: crate::vdisplay::SessionEnv,
}

/// `PUNKTFUNK_SESSION_WATCH` on/off wins. Auto is on for Bazzite/SteamOS (they flip
/// Gaming↔Desktop mid-stream) and off elsewhere.
pub(super) fn session_watch_enabled() -> bool {
    pf_host_config::row_tri("PUNKTFUNK_SESSION_WATCH").unwrap_or_else(is_steam_htpc_platform)
}

/// Bazzite or SteamOS (`ID`/`ID_LIKE`). Absent os-release (non-Linux) → false.
fn is_steam_htpc_platform() -> bool {
    let os = pf_host_config::os_release::os_release();
    os.is("bazzite") || os.is("steamos")
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
                // Unmask before compositor_for_kind: a switch we cannot follow still has to unbar
                // "Return to Gaming Mode" (a masked autologin unit will not start).
                vdisplay::release_autologin_mask(cur);
                match vdisplay::compositor_for_kind(cur) {
                    Some(comp) => {
                        tracing::info!(from = ?current, to = ?cur, compositor = comp.id(),
                            "session watcher: mid-stream switch — signaling backend rebuild");
                        if tx
                            .send(SessionSwitch {
                                kind: cur,
                                compositor: comp,
                                env: active.env,
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
                pending = None;
            }
            Some((k, _)) if k == cur => {}
            _ => pending = Some((cur, std::time::Instant::now())),
        }
    }
}
