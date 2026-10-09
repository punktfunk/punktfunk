//! Identify a launched title's live process(es) — the read-side counterpart to
//! [`super::launch`].
//!
//! Launch starts a title; it does not say what the game looks like after the launcher
//! hands off. Each store contributes the signals it already has on disk: a Steam appid,
//! an install directory, an executable, an environment marker. [`crate::procscan`] maps
//! those to live pids; [`crate::gamelease`] maps pids to a lifetime.
//!
//! Signals are a union: any match belongs to the game. `install_dir` covers stores that
//! expose nothing else; a sharper signal (Steam's launch reaper) still applies for that
//! store.
//!
//! [`DetectSpec`] is host-internal (`#[serde(skip)]` on [`super::GameEntry`]) so a scan
//! that already read these paths is not walked twice.

use super::*;

/// Plugin-owned stores must name this on [`DetectHint`]; the host does not
/// read the launcher's files. Heroic's `HEROIC_APP_NAME` is the only signal
/// under Proton.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EnvMarker {
    #[schema(example = "HEROIC_APP_NAME")]
    pub key: String,
    /// `None` matches key presence only — safe solely for one-game-at-a-time launchers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Cap 64: compared against every candidate process; longer is cost, never a real
/// env name. Out-of-charset keys never fire.
pub(crate) fn valid_env_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Cap on a pinned env VALUE. Compared against every candidate process; unbounded is
/// a DoS lever, never a game id.
pub(crate) const MAX_ENV_VALUE: usize = 256;

/// Fields are independent; all-`None` is untracked
/// ([`crate::gamelease::LeaseKind::Untracked`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetectSpec {
    /// Steam-installed appid, not a non-Steam shortcut (those reaper appids differ;
    /// they carry [`exe`](Self::exe)). Linux wraps every launch in
    /// `reaper SteamLaunch AppId=<appid>`.
    ///
    /// The reaper is the *appid's*, not the game's: pre-launch shader cache has one
    /// too, and only the last tree is the game. [`crate::procscan`] excludes the
    /// shader job; [`crate::gamelease`] waits a window.
    pub steam_appid: Option<u32>,
    pub env_marker: Option<EnvMarker>,
    pub exe: Option<PathBuf>,
    /// Image path or Proton/Wine command line under this dir belongs to the game.
    pub install_dir: Option<PathBuf>,
    /// Image file name, case-insensitive, and nothing else. Weakest signal, and the
    /// only one an operator types ([`super::DetectHint`]): a bare name does not say
    /// which copy is running. "Started after this launch" still bounds it — an
    /// already-open copy is never adopted.
    pub process_name: Option<String>,
}

impl DetectSpec {
    pub fn is_empty(&self) -> bool {
        self.steam_appid.is_none()
            && self.env_marker.is_none()
            && self.exe.is_none()
            && self.install_dir.is_none()
            && self.process_name.is_none()
    }

    #[cfg(test)]
    pub fn steam(appid: u32) -> Self {
        Self {
            steam_appid: Some(appid),
            ..Default::default()
        }
    }

    #[cfg(test)]
    pub fn dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            install_dir: Some(dir.into()),
            ..Default::default()
        }
    }

    pub fn exe(exe: impl Into<PathBuf>) -> Self {
        Self {
            exe: Some(exe.into()),
            ..Default::default()
        }
    }

    /// Windows has no Steam reaper, so the appid alone is not enough there.
    #[cfg(test)]
    pub fn with_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.install_dir = Some(dir.into());
        self
    }

    #[cfg(test)]
    pub fn with_env(mut self, key: impl Into<String>, value: Option<String>) -> Self {
        self.env_marker = Some(EnvMarker {
            key: key.into(),
            value,
        });
        self
    }

    /// Host findings win: a hint fills gaps only, never redirects the matcher off
    /// what the store reported.
    pub fn or_hint(mut self, hint: &DetectHint) -> Self {
        let from = Self::from(hint);
        self.install_dir = self.install_dir.or(from.install_dir);
        self.exe = self.exe.or(from.exe);
        self.process_name = self.process_name.or(from.process_name);
        self.steam_appid = self.steam_appid.or(from.steam_appid);
        self.env_marker = self.env_marker.or(from.env_marker);
        self
    }
}

/// Inbound wire half of [`DetectSpec`]: what an operator or plugin can name about
/// recognizing a title. Every field optional; all empty is no hint.
///
/// Not returned by the catalog API — detect data does not go outbound.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DetectHint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
    /// Weakest signal — see [`DetectSpec::process_name`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    /// Without this a Steam plugin degrades from reaper match to install-dir prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steam_appid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_marker: Option<EnvMarker>,
}

impl DetectHint {
    pub fn is_empty(&self) -> bool {
        self.trimmed().is_none() && self.steam_appid.is_none() && self.env_marker().is_none()
    }

    /// Drop a malformed marker rather than reject the hint: hand-written plugin
    /// input, and the matcher must not receive a rule it cannot honour.
    fn env_marker(&self) -> Option<&EnvMarker> {
        self.env_marker
            .as_ref()
            .filter(|m| valid_env_key(&m.key))
            .filter(|m| m.value.as_ref().is_none_or(|v| v.len() <= MAX_ENV_VALUE))
    }

    /// Drop blank fields (`""` means unset). An empty install dir would match every
    /// process on the box.
    fn trimmed(&self) -> Option<(Option<&str>, Option<&str>, Option<&str>)> {
        fn f(s: &Option<String>) -> Option<&str> {
            s.as_deref().map(str::trim).filter(|v| !v.is_empty())
        }
        let (dir, exe, name) = (f(&self.install_dir), f(&self.exe), f(&self.process_name));
        (dir.is_some() || exe.is_some() || name.is_some()).then_some((dir, exe, name))
    }
}

impl From<&DetectHint> for DetectSpec {
    fn from(h: &DetectHint) -> Self {
        let (install_dir, exe, process_name) = h.trimmed().unwrap_or((None, None, None));
        Self {
            install_dir: install_dir.map(PathBuf::from),
            exe: exe.map(PathBuf::from),
            process_name: process_name.map(str::to_string),
            steam_appid: h.steam_appid,
            env_marker: h.env_marker().cloned(),
        }
    }
}

/// PATH lookup would guess among installs; a wrong exe makes the lease call a
/// running game exited. Host-spawned children are the child; this is the shim
/// fallback.
pub fn spec_from_command(cmd: &str) -> DetectSpec {
    let Some(first) = shell_first_token(cmd) else {
        return DetectSpec::default();
    };
    let p = Path::new(&first);
    if p.is_absolute() && p.is_file() {
        DetectSpec::exe(p)
    } else {
        DetectSpec::default()
    }
}

/// Not a shell parser — only enough to recover a quoted absolute path
/// (`"/opt/My Game/run"`).
fn shell_first_token(cmd: &str) -> Option<String> {
    let cmd = cmd.trim_start();
    let mut chars = cmd.chars();
    match chars.next()? {
        q @ ('"' | '\'') => {
            let rest: String = chars.collect();
            let end = rest.find(q)?;
            Some(rest[..end].to_string())
        }
        _ => cmd.split_whitespace().next().map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_spec_is_untrackable() {
        assert!(DetectSpec::default().is_empty());
        assert!(!DetectSpec::steam(570).is_empty());
        assert!(!DetectSpec::dir("/games/x").is_empty());
        assert!(!DetectSpec::exe("/games/x/run").is_empty());
        assert!(!DetectSpec::default()
            .with_env("HEROIC_GAME_ID", Some("abc".into()))
            .is_empty());
    }

    #[test]
    fn builders_compose() {
        let s = DetectSpec::steam(570).with_dir("/games/dota");
        assert_eq!(s.steam_appid, Some(570));
        assert_eq!(s.install_dir.as_deref(), Some(Path::new("/games/dota")));
        let h = DetectSpec::dir("/games/quail").with_env("HEROIC_GAME_ID", Some("Quail".into()));
        assert_eq!(
            h.env_marker,
            Some(EnvMarker {
                key: "HEROIC_GAME_ID".into(),
                value: Some("Quail".into())
            })
        );
    }

    #[test]
    fn command_spec_only_trusts_an_absolute_existing_file() {
        assert!(spec_from_command("dolphin-emu --batch").is_empty());
        assert!(spec_from_command("/nope/not/here --x").is_empty());
        let me = std::env::current_exe().expect("current exe");
        let bare = format!("{} --flag", me.display());
        assert_eq!(spec_from_command(&bare).exe.as_deref(), Some(me.as_path()));
        let quoted = format!("\"{}\" --flag", me.display());
        assert_eq!(
            spec_from_command(&quoted).exe.as_deref(),
            Some(me.as_path())
        );
        assert!(spec_from_command("   ").is_empty());
    }

    #[test]
    fn a_blank_hint_says_nothing() {
        assert!(DetectHint::default().is_empty());
        let blank = DetectHint {
            install_dir: Some("".into()),
            exe: Some("   ".into()),
            process_name: Some("\t".into()),
            ..Default::default()
        };
        assert!(blank.is_empty());
        assert!(DetectSpec::from(&blank).is_empty(), "nothing to match on");

        let hint = DetectHint {
            install_dir: Some("  /games/quail  ".into()),
            exe: None,
            process_name: Some("quail".into()),
            ..Default::default()
        };
        assert!(!hint.is_empty());
        let spec = DetectSpec::from(&hint);
        assert_eq!(spec.install_dir.as_deref(), Some(Path::new("/games/quail")));
        assert_eq!(spec.process_name.as_deref(), Some("quail"));
        assert_eq!(spec.exe, None);
    }

    #[test]
    fn a_hint_only_fills_gaps() {
        let found = DetectSpec::dir("/games/real");
        let hint = DetectHint {
            install_dir: Some("/games/wrong".into()),
            exe: Some("/games/real/run".into()),
            process_name: None,
            ..Default::default()
        };
        let merged = found.or_hint(&hint);
        assert_eq!(
            merged.install_dir.as_deref(),
            Some(Path::new("/games/real")),
            "the store's own answer stands"
        );
        assert_eq!(
            merged.exe.as_deref(),
            Some(Path::new("/games/real/run")),
            "but a field the store had nothing for is filled in"
        );
        assert!(DetectSpec::default()
            .or_hint(&DetectHint::default())
            .is_empty());
    }

    #[test]
    fn a_hint_can_carry_the_store_derived_signals() {
        let hint = DetectHint {
            steam_appid: Some(440),
            env_marker: Some(EnvMarker {
                key: "HEROIC_APP_NAME".into(),
                value: Some("Quail".into()),
            }),
            ..Default::default()
        };
        assert!(!hint.is_empty(), "either field alone is a real hint");
        let spec = DetectSpec::from(&hint);
        assert_eq!(spec.steam_appid, Some(440));
        assert_eq!(spec.env_marker.as_ref().unwrap().key, "HEROIC_APP_NAME");

        let only_appid = DetectHint {
            steam_appid: Some(620),
            ..Default::default()
        };
        assert!(!only_appid.is_empty());
        assert!(!DetectSpec::from(&only_appid).is_empty());

        let found = DetectSpec::steam(70);
        assert_eq!(found.or_hint(&hint).steam_appid, Some(70));
        assert_eq!(
            DetectSpec::dir("/games/x")
                .or_hint(&hint)
                .env_marker
                .unwrap()
                .key,
            "HEROIC_APP_NAME"
        );
    }

    #[test]
    fn a_malformed_env_marker_says_nothing() {
        let bad = |key: &str, value: Option<String>| DetectHint {
            env_marker: Some(EnvMarker {
                key: key.into(),
                value,
            }),
            ..Default::default()
        };
        assert!(bad("", None).is_empty());
        assert!(bad("HAS-DASH", None).is_empty(), "not a POSIX env name");
        assert!(bad("HAS SPACE", None).is_empty());
        assert!(bad(&"K".repeat(65), None).is_empty(), "over the key cap");
        assert!(
            bad("K", Some("v".repeat(MAX_ENV_VALUE + 1))).is_empty(),
            "over the value cap"
        );
        assert!(!bad(&"K".repeat(64), Some("v".repeat(MAX_ENV_VALUE))).is_empty());
        assert!(DetectSpec::from(&bad("HAS-DASH", None))
            .env_marker
            .is_none());
    }

    #[test]
    fn first_token_handles_quotes_and_spaces() {
        assert_eq!(
            shell_first_token("\"/opt/My Game/run\" -w").as_deref(),
            Some("/opt/My Game/run")
        );
        assert_eq!(
            shell_first_token("'/opt/My Game/run'").as_deref(),
            Some("/opt/My Game/run")
        );
        assert_eq!(shell_first_token("  plain --x").as_deref(), Some("plain"));
        assert_eq!(shell_first_token("").as_deref(), None);
        // Unterminated quote: nothing, not a partial token.
        assert_eq!(shell_first_token("\"/opt/oops").as_deref(), None);
    }
}
