//! Linux (and every non-Windows) launch: the host runs a resolved shell command, nested in
//! gamescope or into the live session. [`command_for`] is the pure `LaunchSpec` → command map;
//! [`launch_session_command`] is the spawn.

use super::*;

#[cfg(target_os = "linux")]
pub(super) const LAUNCHER_UI_STORES: &[&str] =
    &["heroic", "heroic-console", "lutris", "hydra-big-picture"];
#[cfg(not(target_os = "linux"))]
pub(super) const LAUNCHER_UI_STORES: &[&str] = &[];

/// No command ⇒ nothing to launch; same `None` as an unknown id.
pub(super) fn launch_target(
    entry: GameEntry,
    game: crate::gamelease::GameRef,
) -> Option<LaunchTarget> {
    let command = exec_recipe(&entry)
        .map(|r| r.shell_command())
        .or_else(|| entry.launch.as_ref().and_then(command_for))?;
    Some(LaunchTarget {
        game,
        launcher: entry.role == GameRole::Launcher,
        detect: entry.detect,
        command: Some(command),
        own_workspace: entry.on_window.own_workspace(),
        on_window: entry.on_window,
    })
}

/// Can this box open a known `launcher_ui` value now? Both Heroic tiles share
/// `heroic_launch_prefix`. Plugin `detect` is only `~/.config/heroic`, which can survive
/// uninstall — so probe the binary. Hydra needs a desktop entry that handles its link.
pub(super) fn launcher_ui_installed(value: &str) -> bool {
    #[cfg(target_os = "linux")]
    if matches!(value, "heroic" | "heroic-console") {
        return heroic_launch_prefix().is_some();
    }
    #[cfg(target_os = "linux")]
    if value == "hydra-big-picture" {
        return hydra_big_picture().is_some();
    }
    let _ = value;
    true
}

/// Cheap "will this launch" bit for handshake routing. An `exec` entry resolves against the
/// installed manifest, which is on disk and needs no running plugin.
pub fn launch_is_resolvable(id: &str) -> bool {
    let Some(entry) = all_games().into_iter().find(|g| g.id == id) else {
        return false;
    };
    let Some(spec) = entry.launch.as_ref() else {
        return false;
    };
    if spec.kind == "exec" {
        return exec_recipe(&entry).is_some();
    }
    command_for(spec).is_some()
}

/// Pure map from [`LaunchSpec`] to a shell command. `exec` is absent: it is built from the
/// publishing plugin's manifest by [`exec_recipe`] first.
fn command_for(spec: &LaunchSpec) -> Option<String> {
    match spec.kind.as_str() {
        "steam_appid" => valid_steam_appid(&spec.value)
            .then(|| format!("steam steam://rungameid/{}", spec.value)),
        #[cfg(target_os = "linux")]
        "lutris_id" => (!spec.value.is_empty() && spec.value.bytes().all(|b| b.is_ascii_digit()))
            .then(|| format!("lutris lutris:rungameid/{}", spec.value)),
        #[cfg(target_os = "linux")]
        "heroic" => heroic_command(&spec.value),
        // Steam client UI (design D4). `-gamepadui` boots a fresh Steam into Big Picture (SteamOS
        // game-mode when nested); an already-running Steam ignores it and obeys only the URI.
        "steam_ui" => match spec.value.as_str() {
            "bigpicture" => Some("steam -gamepadui steam://open/bigpicture".into()),
            "desktop" => Some("steam".into()),
            _ => None,
        },
        // Other launcher UIs (design D4). Host builds the command; the plugin names the launcher.
        #[cfg(target_os = "linux")]
        "launcher_ui" => match spec.value.as_str() {
            // Same prefix as a game launch, minus `--no-gui` and URI: the window is the tile.
            "heroic" => heroic_launch_prefix(),
            // Both flags: `--console` routes the UI; `--fullscreen` fills the screen.
            // `heroic://` has only ping/launch, so a URI cannot open console mode.
            // Heroic < 2.21.0 ignores `--console` and still honours `--fullscreen`.
            "heroic-console" => {
                heroic_launch_prefix().map(|p| format!("{p} --console --fullscreen"))
            }
            // Bare `lutris` opens the window; a `lutris:rungameid/…` URI would launch a game.
            "lutris" => Some("lutris".into()),
            "hydra-big-picture" => hydra_big_picture(),
            _ => None,
        },
        // The plugin sends an id; the command is whatever the installed entry says (`desktop.rs`),
        // run from its `Path=` the way a desktop launcher does: Wine and GOG entries load data
        // by relative path.
        "desktop_id" => super::desktop::desktop_command(&spec.value).map(|(cmd, cwd)| match cwd {
            Some(dir) => format!(
                "cd {} && {cmd}",
                super::exec::sh_quote(&dir.to_string_lossy())
            ),
            None => cmd,
        }),
        "command" => (!spec.value.trim().is_empty()).then(|| spec.value.clone()),
        _ => None,
    }
}

/// `<runner>:<appName>` → Heroic command, nested in gamescope.
///
/// Heroic is single-instance Electron. A fresh gamescope keeps its hidden
/// process alive; an existing GUI forwards the URI and exits, which tears the
/// session. Quote the URI: every launch route runs this value as a shell command.
#[cfg(target_os = "linux")]
pub(crate) fn heroic_command(value: &str) -> Option<String> {
    let (runner, app) = value.split_once(':')?;
    if !matches!(runner, "legendary" | "gog" | "nile") {
        return None;
    }
    // appName charset: keep the URI a single token (Epic/Amazon alnum, GOG digits).
    if app.is_empty()
        || !app
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return None;
    }
    let prefix = heroic_launch_prefix()?;
    Some(format!(
        "{prefix} --no-gui 'heroic://launch?appName={app}&runner={runner}'"
    ))
}

/// Hydra opens Big Picture from argv only, a running instance included. Every Linux build's entry
/// handles `hydralauncher://`, so this finds a deb, rpm, snap or integrated AppImage alike.
#[cfg(target_os = "linux")]
fn hydra_big_picture() -> Option<String> {
    super::desktop::mime_handler_command("x-scheme-handler/hydralauncher")
        .map(|cmd| format!("{cmd} --big-picture"))
}

#[cfg(target_os = "linux")]
fn heroic_launch_prefix() -> Option<String> {
    let on_path = std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join("heroic").is_file()));
    if on_path {
        return Some("heroic".into());
    }
    let flatpak = std::env::var_os("HOME")
        .map(PathBuf::from)
        .is_some_and(|h| h.join(".var/app/com.heroicgameslauncher.hgl").is_dir());
    flatpak.then(|| "flatpak run com.heroicgameslauncher.hgl".into())
}

/// Child from a session launch, for lifetime tracking
/// (design/session-game-lifetime.md).
#[cfg(target_os = "linux")]
pub struct SpawnedLaunch {
    pub child: std::process::Child,
    /// Own process group (plain session spawn) vs host group (gamescope).
    /// A non-leader must never be signalled by negative pid
    /// ([`crate::gamelease::OwnedChild::group_leader`]).
    pub group_leader: bool,
    /// Workspace this launch owns on the streamed head. Hand it to the lease:
    /// the claim ends with the game, not with this call.
    pub workspace: Option<crate::vdisplay::WorkspaceClaim>,
    /// A `steam …` forwarder, which becomes the Steam client itself when none runs. Never the
    /// lease's child: End game would signal Steam. The lease finds the game by its app id.
    pub steam_forwarder: bool,
}

/// Aim the streamed head at this launch, and — when `own` and the backend can
/// place — at a workspace of its own there.
///
/// Head first: a workspace id nothing owns is minted on whichever monitor
/// holds focus. `want` is the workspace an earlier session already claimed for
/// this launch, so a keep-alive reconnect goes back to the game instead of
/// opening a second empty one.
#[cfg(target_os = "linux")]
fn focus_and_claim(
    compositor: crate::vdisplay::Compositor,
    own: bool,
    want: Option<i64>,
) -> Option<crate::vdisplay::WorkspaceClaim> {
    let out = crate::inject::stream_output()?;
    if crate::vdisplay::focus_streamed_output(compositor, &out) {
        tracing::debug!(output = %out, "claimed focus for the streamed head before launching");
    }
    own.then(|| crate::vdisplay::claim_workspace(compositor, &out, want))
        .flatten()
}

/// Keep-alive reconnect: put the streamed head back on the running game's
/// workspace. The claim belongs to the launch, so this session re-focuses
/// `want` instead of taking a second workspace for a game already placed.
#[cfg(target_os = "linux")]
pub fn adopt_launch_workspace(
    compositor: crate::vdisplay::Compositor,
    want: i64,
) -> Option<crate::vdisplay::WorkspaceClaim> {
    focus_and_claim(compositor, true, Some(want))
}

/// Host-resolved command into the live Linux session, after capture is up.
/// Shared by native and GameStream. Best-effort: failure leaves the user on
/// the streamed desktop rather than tearing the stream down.
///
/// * **KWin / Mutter** — session env already retargeted, virtual output is
///   primary; a plain spawn lands on the stream.
/// * **Hyprland / wlroots (sway)** — EXTEND-only: the streamed head sits
///   beside the operator's. [`focus_and_claim`] claims it here, and with
///   `own_workspace` an empty workspace on it; capture also focused the head,
///   but portal handshake / encoder / first frame sit in between and can steal
///   focus back.
/// * **gamescope (managed / SteamOS / attach)** — spawn inside the running
///   session ([`crate::vdisplay::launch_into_gamescope_session`]). `steam
///   steam://…` also forwards over Steam's pipe.
/// * **gamescope (bare spawn)** — only after a keep-alive reuse, which spawned
///   nothing. A fresh spawn nests via `set_launch_command`
///   ([`crate::vdisplay::launch_is_nested`]). `steam_home` is the seat's, so
///   the forwarder reaches the Steam that reuse kept, not the box's.
#[cfg(target_os = "linux")]
pub fn launch_session_command(
    compositor: crate::vdisplay::Compositor,
    cmd: &str,
    seat: Option<&str>,
    own_workspace: bool,
    steam_home: Option<&std::path::Path>,
) -> Result<SpawnedLaunch> {
    use std::os::unix::process::CommandExt;
    let cmd = cmd.trim();
    anyhow::ensure!(!cmd.is_empty(), "empty command");
    // Before the spawn, so the game's first window maps where it belongs. Same
    // head as the absolute-input pointer, so focus and cursor share one.
    let workspace = focus_and_claim(compositor, own_workspace, None);
    let steam_forwarder = crate::vdisplay::launch_is_steam(cmd);
    let (child, group_leader) = match compositor {
        crate::vdisplay::Compositor::Gamescope => (
            crate::vdisplay::launch_into_gamescope_session(cmd, seat, steam_home)?,
            false,
        ),
        _ => {
            // A Steam this forwarder cold-starts gets its own scope, outside the host's unit: a
            // host restart would take it down mid-write.
            let mut c = if steam_forwarder && user_manager_up() {
                let mut c = std::process::Command::new("systemd-run");
                c.args([
                    "--user",
                    "--scope",
                    "--collect",
                    "--quiet",
                    "--",
                    "sh",
                    "-c",
                ]);
                c
            } else {
                let mut c = std::process::Command::new("sh");
                c.arg("-c");
                c
            };
            c.arg(cmd)
                // Own process group: later teardown signals the shell and its
                // children, and not the host's group.
                .process_group(0);
            // AppImageLauncher's binfmt hook would swap an .AppImage for its integration
            // dialog on the host's own screen; this makes it exec the image directly.
            c.env("APPIMAGELAUNCHER_DISABLE", "1");
            // X11 apps (Steam, Lutris, most native games) need a display of their own. A systemd
            // `--user` host has none to pass on, so take the session's — without it Steam opens
            // "Unable to open a connection to X" instead of the game.
            match crate::vdisplay::session_x11_env() {
                Some((x11, xauthority)) => {
                    c.env("DISPLAY", &x11);
                    if let Some(x) = xauthority {
                        c.env("XAUTHORITY", x);
                    }
                    tracing::debug!(x11_display = %x11, "handed the launch the session's display");
                }
                None => tracing::warn!(
                    "no X display for the launch — an X11 app (Steam, Lutris) will refuse to start"
                ),
            }
            // A Steam forwarder's group can hold the Steam client; never signal it as a group.
            (c.spawn().context("spawn launch command")?, !steam_forwarder)
        }
    };
    tracing::info!(
        command = %cmd,
        pid = child.id(),
        compositor = compositor.id(),
        "launched app into the live session"
    );
    Ok(SpawnedLaunch {
        child,
        group_leader,
        workspace,
        steam_forwarder,
    })
}

/// A user manager to open a scope under; without one `systemd-run --user` fails the launch.
fn user_manager_up() -> bool {
    std::env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|dir| std::path::Path::new(&dir).join("systemd/private").exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn launcher_ui_accepts_only_launchers_this_host_can_open() {
        assert!(known_launcher_ui("heroic"));
        assert!(known_launcher_ui("heroic-console"));
        assert!(known_launcher_ui("lutris"));
        assert!(!known_launcher_ui("gog"));
        // Both Heroic tiles share one probe; a miss drops both, not one dead tile.
        assert_eq!(
            resolvable_launcher_ui("heroic"),
            heroic_launch_prefix().is_some()
        );
        assert_eq!(
            resolvable_launcher_ui("heroic-console"),
            heroic_launch_prefix().is_some()
        );
        assert_eq!(
            resolvable_launcher_ui("hydra-big-picture"),
            hydra_big_picture().is_some()
        );
        if let Some(cmd) = hydra_big_picture() {
            assert!(cmd.ends_with(" --big-picture"), "{cmd}");
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn launcher_ui_knows_nothing_off_linux() {
        assert!(!known_launcher_ui("heroic"));
        assert!(!known_launcher_ui("gog"));
    }

    #[test]
    fn launch_command_resolves_and_guards() {
        let steam = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570".into(),
            ..Default::default()
        };
        assert_eq!(
            command_for(&steam).as_deref(),
            Some("steam steam://rungameid/570")
        );
        let evil = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570; rm -rf ~".into(),
            ..Default::default()
        };
        assert_eq!(command_for(&evil), None);
        let custom = LaunchSpec {
            kind: "command".into(),
            value: "dolphin-emu --batch".into(),
            ..Default::default()
        };
        assert_eq!(command_for(&custom).as_deref(), Some("dolphin-emu --batch"));
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "command".into(),
                value: "  ".into(),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "wat".into(),
                value: "x".into(),
                ..Default::default()
            }),
            None
        );
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn command_for_lutris_and_heroic_guards() {
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "lutris_id".into(),
                value: "42".into(),
                ..Default::default()
            })
            .as_deref(),
            Some("lutris lutris:rungameid/42")
        );
        assert_eq!(
            command_for(&LaunchSpec {
                kind: "lutris_id".into(),
                value: "42; rm -rf ~".into(),
                ..Default::default()
            }),
            None
        );
        assert_eq!(heroic_command("badrunner:Quail"), None);
        assert_eq!(heroic_command("legendary:bad name"), None);
        assert_eq!(heroic_command("nile:"), None);
        // Prefix exists only on boxes with Heroic; assert URI shape only then.
        if let Some(cmd) = heroic_command("legendary:Quail-1.2_x") {
            assert!(cmd.contains("heroic://launch?appName=Quail-1.2_x&runner=legendary"));
            assert!(cmd.contains("--no-gui"));
        }
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn launcher_ui_opens_the_launcher_itself() {
        let ui = |v: &str| {
            command_for(&LaunchSpec {
                kind: "launcher_ui".into(),
                value: v.into(),
                ..Default::default()
            })
        };
        // Bare `lutris` opens the window; the URI form is `lutris_id` and launches a game.
        assert_eq!(ui("lutris").as_deref(), Some("lutris"));
        assert!(!ui("lutris").unwrap().contains("rungameid"));
        // Same prefix as a game launch, no `--no-gui` / URI. `None` without Heroic is correct.
        if let Some(cmd) = ui("heroic") {
            assert!(!cmd.contains("--no-gui"), "the GUI is the point: {cmd:?}");
            assert!(!cmd.contains("heroic://"), "no game URI: {cmd:?}");
            assert!(
                !cmd.contains("--console"),
                "that is the other tile: {cmd:?}"
            );
        }
        // Both flags: `--console` alone does not fill the screen. Same prefix as `heroic`.
        assert_eq!(ui("heroic-console").is_some(), ui("heroic").is_some());
        if let Some(cmd) = ui("heroic-console") {
            assert!(cmd.contains("--console"), "{cmd:?}");
            assert!(cmd.contains("--fullscreen"), "{cmd:?}");
            assert!(!cmd.contains("--no-gui"), "the GUI is the point: {cmd:?}");
        }
        assert_eq!(ui("nonsense"), None);
        assert_eq!(ui(""), None);
    }
    #[test]
    fn steam_ui_resolves_to_the_client_ui_on_linux() {
        let ui = |v: &str| {
            command_for(&LaunchSpec {
                kind: "steam_ui".into(),
                value: v.into(),
                ..Default::default()
            })
        };
        // The flag covers a cold Steam; the URI is all a running desktop Steam acts on.
        assert_eq!(
            ui("bigpicture").as_deref(),
            Some("steam -gamepadui steam://open/bigpicture")
        );
        assert_eq!(ui("desktop").as_deref(), Some("steam"));
        assert_eq!(ui("nonsense"), None);
        assert_eq!(ui(""), None);
    }
}
