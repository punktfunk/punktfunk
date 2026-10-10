//! A Linux seat's own "Return to Gaming Mode" and "Switch to Desktop", without ending the stream.
//!
//! SteamOS, Bazzite and CachyOS send both through steamos-manager's `SessionManagement1` on the
//! session bus, and Steam calls it directly wherever the name exists. On a seat that service
//! would rewrite the box's own login. A seat host owns the name on the seat's bus instead; the
//! runner's `steamos-session-select` calls it too. The desktop keeps running in Game Mode, so a
//! switch only moves each of the seat's live sessions between its KWin output and a gamescope of
//! its own, in place. The mode is kept in the config dir for the next start.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use zbus::fdo;

const NAME: &str = "com.steampowered.SteamOSManager1";
const OBJECT: &str = "/com/steampowered/SteamOSManager1";
/// The only desktop a seat runs (`seat-session`).
const DESKTOP_SESSION: &str = "plasma.desktop";

static GAME: AtomicBool = AtomicBool::new(false);

/// A live session of the seat's: told each new mode, `false` once the session is gone.
pub(crate) type Follower = Box<dyn Fn(bool) -> bool + Send>;
static FOLLOWERS: Mutex<Vec<Follower>> = Mutex::new(Vec::new());

/// The input backend the runner picked for the seat's desktop, read before any session runs.
static DESKTOP_INPUT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// Whether this seat is in Game Mode.
pub(crate) fn in_game_mode() -> bool {
    pf_paths::seat::is_seat_host() && GAME.load(Ordering::Relaxed)
}

/// Whether the seat has a desktop to switch to: its runner pins KWin, else gamescope only.
fn has_desktop() -> bool {
    pf_host_config::config().compositor.as_deref() == Some("kwin")
}

/// Live sessions follow every switch until they end.
pub(crate) fn follow(follower: Follower) {
    FOLLOWERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(follower);
}

/// The injector for `compositor` on a seat. Its desktop keeps the backend the runner picked.
pub(crate) fn input_backend_id(compositor: crate::vdisplay::Compositor) -> String {
    let gamescope = compositor == crate::vdisplay::Compositor::Gamescope;
    match DESKTOP_INPUT.get().cloned().flatten() {
        Some(id) if pf_paths::seat::is_seat_host() && !gamescope => id,
        _ => crate::vdisplay::input_backend_id(compositor).to_string(),
    }
}

/// The seat's own connect opens in the seat's mode, whatever the runner pinned. Input goes to
/// that compositor: a session before this one may have left it on the other.
pub(crate) fn connect_route() -> (
    crate::vdisplay::Compositor,
    Option<crate::vdisplay::GamescopeRoute>,
) {
    use crate::vdisplay::{Compositor, GamescopeRoute};
    let picked = if in_game_mode() {
        (Compositor::Gamescope, Some(GamescopeRoute::Spawn))
    } else {
        (Compositor::Kwin, None)
    };
    crate::inject::set_backend_id(&input_backend_id(picked.0));
    picked
}

/// Reads the saved mode and owns the name for this host's life.
pub(crate) fn spawn() {
    if !pf_paths::seat::is_seat_host() {
        return;
    }
    DESKTOP_INPUT.get_or_init(|| {
        std::env::var("PUNKTFUNK_INPUT_BACKEND")
            .ok()
            .filter(|v| !v.is_empty())
    });
    let mode_file = pf_paths::config_dir().join("seat-session");
    let saved = std::fs::read_to_string(&mode_file).unwrap_or_default();
    GAME.store(saved.trim() == "game" || !has_desktop(), Ordering::Relaxed);
    let service = SessionManagement {
        mode_file,
        has_desktop: has_desktop(),
    };
    tokio::spawn(async move {
        let built = zbus::connection::Builder::session()
            .and_then(|b| b.name(NAME))
            .and_then(|b| b.serve_at(OBJECT, service));
        match built {
            Ok(builder) => match builder.build().await {
                Ok(_conn) => {
                    tracing::info!(
                        game = in_game_mode(),
                        "seat: Game Mode and desktop switches answered on the session bus"
                    );
                    std::future::pending::<()>().await;
                }
                Err(e) => tracing::warn!(error = %e,
                    "seat: session switch not offered — Return to Gaming Mode does nothing here"),
            },
            Err(e) => tracing::warn!(error = %e, "seat: session switch not offered"),
        }
    });
}

struct SessionManagement {
    mode_file: PathBuf,
    has_desktop: bool,
}

impl SessionManagement {
    /// Every switch reaches every session, so one a failed rebuild left behind catches up; a
    /// session already there ignores it.
    fn switch(&self, game: bool) -> fdo::Result<()> {
        if !game && !self.has_desktop {
            return Err(fdo::Error::Failed("this seat has no desktop".into()));
        }
        let mode = if game { "game" } else { "desktop" };
        if GAME.swap(game, Ordering::Relaxed) != game {
            if let Err(e) = std::fs::write(&self.mode_file, format!("{mode}\n")) {
                tracing::warn!(error = %e,
                    "seat: mode not saved, so the next start opens the last saved one");
            }
        }
        let mut followers = FOLLOWERS.lock().unwrap_or_else(|e| e.into_inner());
        followers.retain(|follow| follow(game));
        tracing::info!(mode, sessions = followers.len(), "seat: switched mode");
        Ok(())
    }
}

/// steamos-manager's interface, as far as a seat has a meaning for it.
#[zbus::interface(name = "com.steampowered.SteamOSManager1.SessionManagement1")]
impl SessionManagement {
    fn switch_to_game_mode(&self) -> fdo::Result<()> {
        self.switch(true)
    }

    fn switch_to_desktop_mode(&self) -> fdo::Result<()> {
        self.switch(false)
    }

    fn switch_to_login_mode(&self, mode: &str) -> fdo::Result<()> {
        match mode {
            "game" => self.switch(true),
            "desktop" => self.switch(false),
            other => Err(fdo::Error::InvalidArgs(format!(
                "unknown login mode {other}"
            ))),
        }
    }

    fn switch_to_desktop_session(&self, _session: &str) -> fdo::Result<()> {
        self.switch(false)
    }

    fn valid_desktop_sessions(&self) -> Vec<String> {
        vec![DESKTOP_SESSION.into()]
    }

    fn clean_temporary_sessions(&self) {}

    /// The mode running now, which is also the one the seat starts in next.
    #[zbus(property)]
    fn default_login_mode(&self) -> String {
        if GAME.load(Ordering::Relaxed) {
            "game"
        } else {
            "desktop"
        }
        .into()
    }

    /// Accepted and dropped: a switch is what the seat remembers.
    #[zbus(property)]
    fn set_default_login_mode(&mut self, _mode: String) {}

    #[zbus(property)]
    fn default_desktop_session(&self) -> String {
        DESKTOP_SESSION.into()
    }

    #[zbus(property)]
    fn set_default_desktop_session(&mut self, _session: String) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const IFACE: &str = "com.steampowered.SteamOSManager1.SessionManagement1";

    /// The names steamosctl and Steam call reach the live sessions and the saved mode.
    #[tokio::test]
    async fn steamos_session_calls_switch_the_seat() {
        let dir = tempfile::tempdir().unwrap();
        let mode_file = dir.path().join("seat-session");
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let server = zbus::connection::Builder::unix_stream(a)
            .server(zbus::Guid::generate())
            .unwrap()
            .p2p()
            .serve_at(
                OBJECT,
                SessionManagement {
                    mode_file: mode_file.clone(),
                    has_desktop: true,
                },
            )
            .unwrap();
        let client = zbus::connection::Builder::unix_stream(b).p2p();
        let (_server, client) = tokio::try_join!(server.build(), client.build()).unwrap();
        let proxy = zbus::Proxy::new(&client, NAME, OBJECT, IFACE)
            .await
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        follow(Box::new(move |game| tx.send(game).is_ok()));

        proxy.call_method("SwitchToGameMode", &()).await.unwrap();
        assert_eq!(rx.try_recv(), Ok(true));
        assert_eq!(std::fs::read_to_string(&mode_file).unwrap(), "game\n");
        let mode: String = proxy.get_property("DefaultLoginMode").await.unwrap();
        assert_eq!(mode, "game");

        // Already there: told again, so a session a failed rebuild left behind catches up.
        proxy
            .call_method("SwitchToLoginMode", &("game",))
            .await
            .unwrap();
        assert_eq!(rx.try_recv(), Ok(true));

        proxy
            .call_method("SwitchToDesktopSession", &("plasma.desktop",))
            .await
            .unwrap();
        assert_eq!(rx.try_recv(), Ok(false));
        assert_eq!(std::fs::read_to_string(&mode_file).unwrap(), "desktop\n");
        assert!(proxy
            .call_method("SwitchToLoginMode", &("tv",))
            .await
            .is_err());
    }
}
