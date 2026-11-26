//! Linux and macOS launch: the host runs a resolved shell command, nested in gamescope or into
//! the live session on Linux, onto the logged-in desktop on macOS. [`command_for`] is the
//! `LaunchSpec` → command map, which spawns nothing; [`launch_session_command`] and
//! [`launch_mac_command`] are the spawns.

use super::*;

#[cfg(target_os = "linux")]
pub(super) const LAUNCHER_UI_STORES: &[&str] = &[
    "heroic",
    "heroic-console",
    "lutris",
    "hydra-big-picture",
    "itch",
    "bottles",
];
#[cfg(not(target_os = "linux"))]
pub(super) const LAUNCHER_UI_STORES: &[&str] = &[];

/// No command ⇒ nothing to launch; same `None` as an unknown id.
pub(super) fn launch_target(entry: GameEntry, game: GameRef) -> Option<LaunchTarget> {
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

/// The command a `steam_ui` tile with `value` runs, or `None` for a value outside the set.
pub(super) fn steam_ui_command(value: &str) -> Option<String> {
    command_for(&LaunchSpec {
        kind: "steam_ui".into(),
        value: value.into(),
        ..Default::default()
    })
}

/// Can this box open a known `launcher_ui` value now? Both Heroic tiles share
/// `heroic_launch_prefix`. Plugin `detect` is only `~/.config/heroic`, which can survive
/// uninstall — so probe the binary. Hydra needs a desktop entry that handles its link; itch
/// and Bottles need their own.
pub(super) fn launcher_ui_installed(value: &str) -> bool {
    #[cfg(target_os = "linux")]
    if matches!(value, "heroic" | "heroic-console") {
        return heroic_launch_prefix().is_some();
    }
    #[cfg(target_os = "linux")]
    if value == "hydra-big-picture" {
        return hydra_big_picture().is_some();
    }
    #[cfg(target_os = "linux")]
    if matches!(value, "itch" | "bottles") {
        return app_entry_command(value).is_some();
    }
    let _ = value;
    true
}

/// Cheap "will this launch" bit for handshake routing. An `exec` entry resolves against the
/// installed manifest, which is on disk and needs no running plugin.
pub fn launch_is_resolvable(id: &str) -> bool {
    let Some(entry) = find_game_for(&View::here(), id) else {
        return false;
    };
    let Some(spec) = entry.launch.as_ref() else {
        return false;
    };
    if matches!(spec.kind.as_str(), "exec" | "emulator") {
        return exec_recipe(&entry).is_some();
    }
    command_for(spec).is_some()
}

/// What hands a `steam://` link to Steam: its own binary on Linux, LaunchServices on macOS,
/// where Steam.app is no command on `PATH`.
#[cfg(not(target_os = "macos"))]
const STEAM_LINK: &str = "steam";
#[cfg(target_os = "macos")]
const STEAM_LINK: &str = "/usr/bin/open";

/// Map from [`LaunchSpec`] to a shell command; it spawns nothing. `exec` and `emulator` are
/// absent: [`exec_recipe`] builds them first.
fn command_for(spec: &LaunchSpec) -> Option<String> {
    match spec.kind.as_str() {
        "steam_appid" => valid_steam_appid(&spec.value)
            .then(|| format!("{STEAM_LINK} steam://rungameid/{}", spec.value)),
        #[cfg(target_os = "linux")]
        "lutris_id" => (!spec.value.is_empty() && spec.value.bytes().all(|b| b.is_ascii_digit()))
            .then(|| format!("lutris lutris:rungameid/{}", spec.value)),
        #[cfg(target_os = "linux")]
        "heroic" => heroic_command(&spec.value),
        // LaunchServices hands the link to the launcher, running or not, as it does Steam's.
        // `gui=false` shuts Heroic's window as `--no-gui` does on Linux; an older Heroic ignores it.
        #[cfg(target_os = "macos")]
        "heroic" => {
            heroic_launch_uri(&spec.value).map(|uri| open_link(&format!("{uri}&gui=false")))
        }
        #[cfg(target_os = "macos")]
        "epic" => epic_launch_uri(&spec.value).map(|uri| open_link(&uri)),
        // Steam client UI (design D4). `-gamepadui` boots a fresh Steam into Big Picture (SteamOS
        // game-mode when nested); an already-running Steam ignores it and obeys only the URI.
        #[cfg(not(target_os = "macos"))]
        "steam_ui" => match spec.value.as_str() {
            "bigpicture" => Some("steam -gamepadui steam://open/bigpicture".into()),
            "desktop" => Some("steam".into()),
            _ => None,
        },
        #[cfg(target_os = "macos")]
        "steam_ui" => match spec.value.as_str() {
            "bigpicture" => Some(format!("{STEAM_LINK} steam://open/bigpicture")),
            "desktop" => Some(format!("{STEAM_LINK} steam://open/main")),
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
            "itch" | "bottles" => app_entry_command(&spec.value),
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
    let uri = heroic_launch_uri(value)?;
    let prefix = heroic_launch_prefix()?;
    Some(format!("{prefix} --no-gui '{uri}'"))
}

/// `open` with one quoted link: LaunchServices starts the app that claims its scheme.
#[cfg(target_os = "macos")]
fn open_link(uri: &str) -> String {
    format!("/usr/bin/open {}", super::exec::sh_quote(uri))
}

/// Hydra opens Big Picture from argv only, a running instance included. Every Linux build's entry
/// handles `hydralauncher://`, so this finds a deb, rpm, snap or integrated AppImage alike.
#[cfg(target_os = "linux")]
fn hydra_big_picture() -> Option<String> {
    super::desktop::mime_handler_command("x-scheme-handler/hydralauncher")
        .map(|cmd| format!("{cmd} --big-picture"))
}

/// The `Exec=` of the app's installed desktop entry, native or Flatpak: both use the reverse-DNS
/// id. itch's own installer names its entry `itch`.
#[cfg(target_os = "linux")]
fn app_entry_command(value: &str) -> Option<String> {
    let ids: &[&str] = match value {
        "itch" => &["io.itch.itch", "itch"],
        "bottles" => &["com.usebottles.bottles"],
        _ => &[],
    };
    ids.iter()
        .find_map(|id| super::desktop::desktop_command(id))
        .map(|(cmd, _)| cmd)
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

/// Where a launch into the live session lands.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// On the streamed head, which the last stream's capture published.
    Stream,
    /// On the desk as it is: the box's own screen, with no stream.
    Desk,
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

/// What a launch runs on top of the session it lands in: the `sh -c` line and the variables it
/// sets. [`launch_session_command`] spawns it; `punktfunk-host play` runs it in Steam's tree.
#[cfg(target_os = "linux")]
pub struct DeskCommand {
    pub command: String,
    pub env: Vec<(&'static str, &'static str)>,
}

/// `cmd` as a launch runs it. Errors on an empty command.
#[cfg(target_os = "linux")]
pub fn desk_command(cmd: &str) -> Result<DeskCommand> {
    let command = cmd.trim();
    anyhow::ensure!(!command.is_empty(), "empty command");
    Ok(DeskCommand {
        command: command.to_string(),
        // AppImageLauncher's binfmt hook would swap an .AppImage for its integration dialog on
        // the host's own screen; this makes it exec the image directly.
        env: vec![("APPIMAGELAUNCHER_DISABLE", "1")],
    })
}

/// Host-resolved command ([`desk_command`]) into the live Linux session, after capture is up,
/// or onto the desk with no stream ([`Place::Desk`]), which aims at no head. Shared by native,
/// GameStream and the desk. Best-effort: failure leaves the user on the streamed desktop rather
/// than tearing the stream down.
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
///   ([`crate::vdisplay::launch_is_nested`]).
///
/// Every route starts the program unraised ([`pf_frame::thread_qos::unraised`]): the caller
/// is a boosted stream thread, and a game must not run level with capture and encode.
#[cfg(target_os = "linux")]
pub fn launch_session_command(
    compositor: crate::vdisplay::Compositor,
    cmd: &str,
    seat: Option<&str>,
    own_workspace: bool,
    place: Place,
) -> Result<SpawnedLaunch> {
    use std::os::unix::process::CommandExt;
    let DeskCommand { command, env } = desk_command(cmd)?;
    let cmd = command.as_str();
    // Before the spawn, so the game's first window maps where it belongs. Same
    // head as the absolute-input pointer, so focus and cursor share one.
    let workspace = match place {
        Place::Stream => focus_and_claim(compositor, own_workspace, None),
        Place::Desk => None,
    };
    let steam_forwarder = crate::vdisplay::launch_is_steam(cmd);
    let (child, group_leader) = match compositor {
        crate::vdisplay::Compositor::Gamescope => (
            crate::vdisplay::launch_into_gamescope_session(cmd, seat)?,
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
                .process_group(0)
                .envs(env.iter().copied());
            // Wayland apps take the live session's socket from the host, not its environment.
            pf_host_config::session::export_to(&mut c);
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
            pf_frame::thread_qos::unraised(&mut c);
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

/// `cmd` started on the logged-in user's desktop, in a process group of its own so the end-game
/// ladder reaches what it starts: an app bundle through LaunchServices (`open -a`), anything else
/// through `sh -c`, as on Linux. `open` returns once the app is up, so the lease finds the game
/// by its detect signals.
#[cfg(target_os = "macos")]
pub fn launch_mac_command(cmd: &str) -> Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    let cmd = cmd.trim();
    anyhow::ensure!(!cmd.is_empty(), "empty command");
    let mut c = match app_bundle(cmd) {
        Some(app) => {
            let mut c = std::process::Command::new("/usr/bin/open");
            c.arg("-a").arg(app);
            c
        }
        None => {
            let mut c = std::process::Command::new("/bin/sh");
            c.arg("-c").arg(cmd);
            c
        }
    };
    let child = c.process_group(0).spawn().context("spawn launch command")?;
    tracing::info!(command = %cmd, pid = child.id(), "launched app on the desktop");
    Ok(child)
}

/// A user manager to open a scope under; without one `systemd-run --user` fails the launch.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn user_manager_up() -> bool {
    pf_host_config::session::runtime_dir()
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
        assert!(known_launcher_ui("itch"));
        assert!(known_launcher_ui("bottles"));
        assert!(!known_launcher_ui("gog"));
        for v in ["itch", "bottles"] {
            assert_eq!(
                resolvable_launcher_ui(v),
                app_entry_command(v).is_some(),
                "{v}"
            );
        }
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

    /// A launch onto the desk starts its command as it is: no streamed head, no workspace of
    /// its own, its own process group.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_desk_launch_claims_no_head() {
        let compositor = crate::vdisplay::Compositor::Wlroots;
        let mut spawned = launch_session_command(compositor, "exit 0", None, true, Place::Desk)
            .expect("spawn the desk launch");
        assert!(spawned.workspace.is_none());
        assert!(spawned.group_leader && !spawned.steam_forwarder);
        assert!(spawned.child.wait().expect("reap it").success());
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
            command_for(&steam),
            Some(format!("{STEAM_LINK} steam://rungameid/570"))
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
    /// macOS hands every Steam link to LaunchServices: Steam.app is no command on `PATH`.
    #[cfg(target_os = "macos")]
    #[test]
    fn steam_links_go_through_open_on_macos() {
        let ui = |v: &str| {
            command_for(&LaunchSpec {
                kind: "steam_ui".into(),
                value: v.into(),
                ..Default::default()
            })
        };
        assert_eq!(
            ui("bigpicture").as_deref(),
            Some("/usr/bin/open steam://open/bigpicture")
        );
        assert_eq!(
            ui("desktop").as_deref(),
            Some("/usr/bin/open steam://open/main")
        );
        assert_eq!(ui("nonsense"), None);
        let game = LaunchSpec {
            kind: "steam_appid".into(),
            value: "570".into(),
            ..Default::default()
        };
        assert_eq!(
            command_for(&game).as_deref(),
            Some("/usr/bin/open steam://rungameid/570")
        );
    }

    /// Epic and Heroic titles open their launcher's link, quoted whole; a bad value runs nothing.
    #[cfg(target_os = "macos")]
    #[test]
    fn epic_and_heroic_links_go_through_open_on_macos() {
        let cmd = |kind: &str, value: &str| {
            command_for(&LaunchSpec {
                kind: kind.into(),
                value: value.into(),
                ..Default::default()
            })
        };
        assert_eq!(
            cmd("epic", "fn:abc:Fortnite").as_deref(),
            Some(
                "/usr/bin/open \
                 'com.epicgames.launcher://apps/fn%3Aabc%3AFortnite?action=launch&silent=true'"
            )
        );
        assert_eq!(
            cmd("heroic", "gog:1207658924").as_deref(),
            Some("/usr/bin/open 'heroic://launch?appName=1207658924&runner=gog&gui=false'")
        );
        assert_eq!(cmd("epic", "a'b"), None);
        assert_eq!(cmd("heroic", "sideload:x"), None);
        assert_eq!(cmd("heroic", "legendary:bad name"), None);
    }

    /// A plain command runs in a process group of its own and reports its exit.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_mac_launch_runs_a_shell_line_in_its_own_group() {
        let mut child = launch_mac_command("exit 3").expect("spawn the launch");
        assert_eq!(child.wait().expect("reap it").code(), Some(3));
        assert!(launch_mac_command("  ").is_err());
    }

    #[cfg(not(target_os = "macos"))]
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
