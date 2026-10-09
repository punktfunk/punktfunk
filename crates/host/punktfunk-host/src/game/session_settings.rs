//! Session⇄game lifetime settings (`<config>/session-settings.json`).
//!
//! Two operator choices (`design/session-game-lifetime.md`):
//!
//! * [`SessionSettings::session_on_game_exit`] — end the session when the
//!   launched game exits. **On** by default.
//! * [`SessionSettings::game_on_session_end`] — end the launched game when the
//!   session ends. **Off** by default ([`GameOnSessionEnd::Keep`]): ending a
//!   game can cost unsaved progress. `Always` waits
//!   [`SessionSettings::disconnect_grace_seconds`] so a blip never costs a save.
//!
//! Its own store, not a display-policy axis: keep-alive is how long a *display*
//! survives a disconnect (default 10 s); this is a *game* (default 5 minutes).
//!
//! Same shape as `DisplayPolicyStore` / `pf_gpu::GpuPrefStore`: missing or
//! corrupt means unconfigured with a warning (never fail host startup), writes
//! are private-dir + temp + atomic rename, and memory changes only after the
//! write lands.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use utoipa::ToSchema;

/// What to do with the launched game when its session ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GameOnSessionEnd {
    /// Default: nothing is killed unless the operator asks.
    #[default]
    Keep,
    /// End it when the client stops the session deliberately; leave it running if
    /// the client vanished, so a drop stays reconnectable.
    OnQuit,
    /// End it whenever the session ends. A deliberate stop ends it at once; a drop
    /// starts the reconnect window and ends it only if nobody comes back.
    Always,
}

impl GameOnSessionEnd {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::OnQuit => "on_quit",
            Self::Always => "always",
        }
    }
}

/// What to do with a title this client already has running when it launches a
/// **different** one.
///
/// A third axis, not a fourth [`GameOnSessionEnd`] value: that one is "this
/// session is over"; this one is "the player asked for something else". An
/// operator who wants a game to survive a disconnect can still want it closed
/// when they pick another title.
///
/// Scoped to this client's own launches, and only launches this host performed
/// ([`crate::launchreg`]). A game the player started at the machine was never
/// recorded, so this cannot close it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GameOnNewLaunch {
    /// Same posture as [`GameOnSessionEnd::Keep`]: ending a game can cost unsaved progress.
    #[default]
    Keep,
    /// Close it, politely first (`WM_CLOSE` / `SIGTERM`), before starting the new title.
    End,
}

/// Default reconnect window before `Always` ends a game. 300 s covers a Wi-Fi
/// roam or a client restart; being wrong costs unsaved progress.
const DEFAULT_GRACE_SECS: u32 = 300;
/// Floor keeps a typed `1` from making `Always` an instant kill; ceiling (24 h)
/// keeps a stray large value from pinning a lease forever.
const MIN_GRACE_SECS: u32 = 10;
const MAX_GRACE_SECS: u32 = 86_400;

fn default_grace() -> u32 {
    DEFAULT_GRACE_SECS
}

fn default_true() -> bool {
    true
}

fn one() -> u32 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct SessionSettings {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub game_on_session_end: GameOnSessionEnd,
    #[serde(default = "default_true")]
    pub session_on_game_exit: bool,
    #[serde(default)]
    pub game_on_new_launch: GameOnNewLaunch,
    /// Ignored unless `game_on_session_end` is `Always`.
    #[serde(default = "default_grace")]
    pub disconnect_grace_seconds: u32,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            version: 1,
            game_on_session_end: GameOnSessionEnd::default(),
            session_on_game_exit: true,
            game_on_new_launch: GameOnNewLaunch::default(),
            disconnect_grace_seconds: DEFAULT_GRACE_SECS,
        }
    }
}

impl SessionSettings {
    /// Clamp anything a hand-edited file or plugin could get wrong.
    pub fn sanitized(mut self) -> Self {
        self.version = 1;
        self.disconnect_grace_seconds = self
            .disconnect_grace_seconds
            .clamp(MIN_GRACE_SECS, MAX_GRACE_SECS);
        self
    }
}

pub struct SessionSettingsStore {
    path: PathBuf,
    cur: Mutex<Option<SessionSettings>>,
}

impl SessionSettingsStore {
    /// Missing ⇒ unconfigured; corrupt ⇒ unconfigured **with a warning**, never a
    /// startup failure.
    pub fn load_from(path: PathBuf) -> Self {
        let cur = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<SessionSettings>(&bytes) {
                Ok(s) => Some(s.sanitized()),
                Err(e) => {
                    tracing::warn!(path = %path.display(),
                        "session-settings.json unreadable — using built-in defaults: {e}");
                    None
                }
            },
            Err(_) => None,
        };
        Self {
            path,
            cur: Mutex::new(cur),
        }
    }

    pub fn get(&self) -> SessionSettings {
        self.cur
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default()
    }

    /// Drives the console's "using defaults" hint.
    pub fn configured(&self) -> bool {
        self.cur.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    /// Persist then adopt (sanitized first). Memory changes only if the write lands,
    /// so a full disk cannot leave the running host disagreeing with its file.
    pub fn set(&self, settings: SessionSettings) -> Result<()> {
        let settings = settings.sanitized();
        pf_paths::replace_secret_file(&self.path, &serde_json::to_vec_pretty(&settings)?)?;
        *self.cur.lock().unwrap_or_else(|e| e.into_inner()) = Some(settings);
        Ok(())
    }
}

/// Process-wide store, loaded once on first access. Same shape as
/// `vdisplay::prefs()` / `pf_gpu::prefs()`: lifetime decisions happen in session
/// teardown, where no app state is threaded.
pub fn store() -> &'static SessionSettingsStore {
    static STORE: OnceLock<SessionSettingsStore> = OnceLock::new();
    STORE.get_or_init(|| {
        SessionSettingsStore::load_from(pf_paths::config_dir().join("session-settings.json"))
    })
}

/// The settings in force. A contract seat has none of its own: the built-in defaults, with the
/// box's choice of what a game's exit does to the stream (`seat_end_on_game_exit`).
pub fn get() -> SessionSettings {
    if pf_paths::seat::follows_contract() {
        return contract_settings(pf_host_config::seat_defaults::current());
    }
    store().get()
}

fn contract_settings(
    defaults: Option<pf_host_config::seat_defaults::SeatDefaults>,
) -> SessionSettings {
    let mut settings = SessionSettings::default();
    if let Some(d) = defaults {
        settings.session_on_game_exit = d.end_on_game_exit;
    }
    settings
}

/// Lifetime axes this build acts on, for the console to grey out the rest.
/// Both directions need a launch path and process visibility; macOS has neither.
pub fn enforced() -> Vec<String> {
    // A contract seat has nothing to set: the box hands it the one choice it makes.
    if pf_paths::seat::follows_contract() {
        return Vec::new();
    }
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    {
        vec![
            "session_on_game_exit".to_string(),
            "game_on_session_end".to_string(),
            "game_on_new_launch".to_string(),
            "disconnect_grace_seconds".to_string(),
        ]
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_ones() {
        let d = SessionSettings::default();
        // session_on_game_exit ships on; both axes that can kill a game ship off.
        assert!(d.session_on_game_exit);
        assert_eq!(d.game_on_session_end, GameOnSessionEnd::Keep);
        assert_eq!(d.game_on_new_launch, GameOnNewLaunch::Keep);
        assert_eq!(d.disconnect_grace_seconds, 300);
    }

    #[test]
    fn an_empty_object_decodes_to_the_defaults() {
        // Every field is `#[serde(default)]`, so a partial or older writer still loads.
        let s: SessionSettings = serde_json::from_str("{}").expect("empty object");
        assert!(s.session_on_game_exit);
        assert_eq!(s.game_on_session_end, GameOnSessionEnd::Keep);
        assert_eq!(s.disconnect_grace_seconds, 300);
        assert_eq!(s.version, 1);
    }

    #[test]
    fn policy_names_are_snake_case_on_the_wire() {
        // These strings are the API contract with the console.
        let json = serde_json::to_string(&GameOnSessionEnd::OnQuit).unwrap();
        assert_eq!(json, "\"on_quit\"");
        let back: GameOnSessionEnd = serde_json::from_str("\"always\"").unwrap();
        assert_eq!(back, GameOnSessionEnd::Always);
        assert_eq!(GameOnSessionEnd::Keep.as_str(), "keep");
    }

    #[test]
    fn grace_is_clamped_both_ways() {
        let low = SessionSettings {
            disconnect_grace_seconds: 0,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(low.disconnect_grace_seconds, MIN_GRACE_SECS);
        let high = SessionSettings {
            disconnect_grace_seconds: u32::MAX,
            ..Default::default()
        }
        .sanitized();
        assert_eq!(high.disconnect_grace_seconds, MAX_GRACE_SECS);
    }

    #[test]
    fn missing_file_is_unconfigured_and_corrupt_file_does_not_fail() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("session-settings.json");
        let store = SessionSettingsStore::load_from(path.clone());
        assert!(!store.configured());
        assert_eq!(store.get().game_on_session_end, GameOnSessionEnd::Keep);

        std::fs::write(&path, b"{ not json").unwrap();
        let store = SessionSettingsStore::load_from(path.clone());
        assert!(
            !store.configured(),
            "corrupt file must read as unconfigured"
        );
        assert!(store.get().session_on_game_exit, "defaults still apply");
    }

    #[test]
    fn set_persists_sanitized_and_round_trips() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("session-settings.json");
        let store = SessionSettingsStore::load_from(path.clone());
        store
            .set(SessionSettings {
                version: 99,
                game_on_session_end: GameOnSessionEnd::Always,
                session_on_game_exit: false,
                game_on_new_launch: GameOnNewLaunch::End,
                disconnect_grace_seconds: 5,
            })
            .expect("write");
        assert!(store.configured());
        let got = store.get();
        assert_eq!(got.game_on_session_end, GameOnSessionEnd::Always);
        assert!(!got.session_on_game_exit);
        assert_eq!(got.game_on_new_launch, GameOnNewLaunch::End);
        assert_eq!(got.disconnect_grace_seconds, MIN_GRACE_SECS);
        assert_eq!(got.version, 1, "version is normalized, not echoed");
        let reloaded = SessionSettingsStore::load_from(path.clone());
        assert_eq!(reloaded.get().game_on_session_end, GameOnSessionEnd::Always);
        let files = std::fs::read_dir(td.path()).unwrap().count();
        assert_eq!(files, 1, "the temp does not outlive the rename");
    }
}
