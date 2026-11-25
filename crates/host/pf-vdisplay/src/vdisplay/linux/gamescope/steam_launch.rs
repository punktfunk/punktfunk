//! Steam launches: which ones contend for the box's single Steam instance, how a launch
//! command is shaped, and holding a non-Steam shortcut until the nested Steam is up.

use super::*;
use std::process::Stdio;

/// Run `cmd` inside the live session (managed / SteamOS / attach — [`spawn()`]'s nesting does not
/// apply). The env is a gamescope child's ([`as_gamescope_child`]), read from a process already
/// inside; with no such process, host env (a `steam steam://…` still reaches the running Steam
/// over its pipe). The program starts unraised ([`pf_frame::thread_qos::unraised`]).
pub fn launch_into_session(cmd: &str, seat: Option<&str>) -> Result<std::process::Child> {
    let flatpak = shape_flatpak_command(cmd);
    let mut c = Command::new("sh");
    c.arg("-c").arg(flatpak.as_deref().unwrap_or(cmd));
    // Keeps AppImageLauncher's binfmt hook from replacing an .AppImage with its dialog.
    c.env("APPIMAGELAUNCHER_DISABLE", "1");
    // The host's Wayland socket, for the arm below that finds no session display.
    pf_host_config::session::export_to(&mut c);
    match discover_session_display_env(seat) {
        Some((x11, gamescope, _xauth)) => {
            tracing::info!(
                command = %cmd,
                x11_display = x11.as_deref().unwrap_or("-"),
                gamescope = gamescope.as_deref().unwrap_or("-"),
                "gamescope: launching into the live session"
            );
            as_gamescope_child(&mut c, x11, gamescope, flatpak.is_some());
        }
        None => tracing::warn!(
            command = %cmd,
            "gamescope: could not discover the session's display env — spawning with the host env \
             (a `steam steam://…` launch still reaches the running Steam; other apps may not land \
             in the session)"
        ),
    }
    pf_frame::thread_qos::unraised(&mut c)
        .spawn()
        .context("spawn launch command into gamescope session")
}

/// The env gamescope gives its own children: its Xwayland, its socket's name, session type
/// `x11`, desktop `gamescope`, no `WAYLAND_DISPLAY`, plus [`GDK_X11`] outside a flatpak.
///
/// The host's env names the desktop beside it. With that session type, or none, SDL3 opens
/// `wayland-0`, the desktop's socket: a window the stream never shows. Qt follows the type too.
fn as_gamescope_child(
    c: &mut Command,
    x11: Option<String>,
    gamescope: Option<String>,
    flatpak: bool,
) {
    if let Some(d) = x11 {
        c.env("DISPLAY", d);
    }
    if let Some(gs) = gamescope {
        c.env("GAMESCOPE_WAYLAND_DISPLAY", gs);
    }
    c.env_remove("WAYLAND_DISPLAY")
        .env("XDG_SESSION_TYPE", "x11")
        .env("XDG_CURRENT_DESKTOP", "gamescope");
    if !flatpak {
        c.env(GDK_X11.0, GDK_X11.1);
    }
}

/// Steam tears down a running game (Proton included) on the way out.
pub(super) const STEAM_SHUTDOWN_WAIT: Duration = Duration::from_secs(20);

/// Half of [`STEAM_SHUTDOWN_WAIT`]. `/usr/bin/steam` is `steam.sh`, not a thin IPC forwarder;
/// `status_within` killpg's the group, so a 5 s bound kills the request before it leaves.
const STEAM_SHUTDOWN_SEND_BUDGET: Duration = Duration::from_secs(STEAM_SHUTDOWN_WAIT.as_secs() / 2);

/// Clean-exit bound before the host SIGKILLs the unit or compositor a Steam runs in. An idle
/// Steam is gone in a few seconds; the shutdown restore has 20 s for this and the box's restart.
pub(super) const STEAM_STOP_WAIT: Duration = Duration::from_secs(8);

/// Ask the box's Steam at `pid` to quit over its pipe, then wait out `wait` from the call.
/// `true` once it is gone.
pub(super) fn shut_steam_down(pid: u32, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    let mut cmd = Command::new("steam");
    cmd.arg("-shutdown")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    pf_host_config::session::export_to(&mut cmd);
    // Reaped: dropping Child does not wait, and the loop below polls the TARGET, not this helper.
    let _ = crate::proc::status_within(&mut cmd, wait.min(STEAM_SHUTDOWN_SEND_BUDGET));
    loop {
        if !crate::proc::pid_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Desktop Steam holds the single instance; autologin stop cannot see it. Ours (host tree /
/// SESSION_UNIT) are exempt. `true` when one was asked to quit and did: whoever asked starts it
/// again ([`restart_swallowed_desktop_steam`]). Timeout is an actionable error, not a no-frames
/// retry loop.
pub(super) fn free_desktop_steam() -> Result<bool> {
    let Some(pid) = desktop_steam_pid() else {
        return Ok(false);
    };
    tracing::info!(
        pid,
        "freeing Steam: a desktop-session Steam holds the single instance — sending `steam -shutdown`"
    );
    if shut_steam_down(pid, STEAM_SHUTDOWN_WAIT) {
        tracing::info!(pid, "desktop Steam exited — single instance free");
        return Ok(true);
    }
    bail!(
        "Steam is already running in the host's desktop session (pid {pid}) and did not exit \
         within {}s of `steam -shutdown` — close Steam on the host, then launch again",
        STEAM_SHUTDOWN_WAIT.as_secs()
    )
}

/// The Steam client `home` runs, from its `.steam/steam.pid`. `None` once that pid is not Steam.
pub(super) fn home_steam_pid(home: &std::path::Path) -> Option<u32> {
    let pid = std::fs::read_to_string(home.join(".steam/steam.pid"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    // Steam's own processes report comm `steam` (the ubuntu12_32 binary) or `steam.sh`; anything
    // else means the pid was recycled since Steam last ran.
    (matches!(comm.trim(), "steam" | "steam.sh") && crate::proc::pid_alive(pid)).then_some(pid)
}

/// The desktop's Steam autostart as systemd's XDG autostart generator names it. Bazzite's
/// `/etc/xdg/autostart/steam.desktop` and Steam's own login entry both become this unit.
const DESKTOP_STEAM_AUTOSTART: &str = "app-steam@autostart.service";

/// The desktop's Steam made way for ours: [`free_desktop_steam`] asked it to quit, or it started
/// a few seconds into the desktop while ours held the single instance, handed its launch over
/// and exited. Call once ours, which was running, is gone; starts the autostart again when it
/// ran and no Steam runs. A desktop without the unit is left alone.
pub(super) fn restart_swallowed_desktop_steam() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    if home_steam_pid(std::path::Path::new(&home)).is_some() {
        return;
    }
    let Ok(out) = crate::proc::output_within(
        Command::new("systemctl").args([
            "--user",
            "show",
            DESKTOP_STEAM_AUTOSTART,
            "-p",
            "LoadState",
            "-p",
            "ActiveState",
            "-p",
            "ExecMainStartTimestampMonotonic",
        ]),
        UNIT_QUERY_BUDGET,
    ) else {
        return;
    };
    if !ran_and_exited(&String::from_utf8_lossy(&out.stdout)) {
        return;
    }
    systemctl_user(&["start", "--no-block", DESKTOP_STEAM_AUTOSTART]);
    tracing::info!(
        unit = DESKTOP_STEAM_AUTOSTART,
        "gamescope: started the desktop's Steam again — it had made way for ours"
    );
}

/// [`restart_swallowed_desktop_steam`] for a dedicated launch that freed it, while the box shows
/// a desktop: under a Game Mode the autostart would take the instance from the box's Steam.
pub(super) fn give_desktop_steam_back() {
    if crate::session::is_desktop_kind(crate::detect_active_session().kind) {
        restart_swallowed_desktop_steam();
    }
}

/// A `systemctl show` body of a unit that is loaded, has run a main process, and runs none
/// now. `failed` counts: a Steam that hands its launch to the one holding the instance exits 1.
fn ran_and_exited(show: &str) -> bool {
    let prop = |key: &str| {
        show.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or("")
    };
    prop("LoadState") == "loaded"
        && matches!(prop("ActiveState"), "inactive" | "failed")
        && !matches!(prop("ExecMainStartTimestampMonotonic"), "" | "0")
}

/// The box home's Steam, when it runs inside `unit` (a Game Mode session or [`SESSION_UNIT`]).
pub(super) fn steam_pid_in_unit(unit: &str) -> Option<u32> {
    let home = std::env::var_os("HOME")?;
    let pid = home_steam_pid(std::path::Path::new(&home))?;
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    cgroup_names_unit(&cgroup, unit).then_some(pid)
}

/// Does a `/proc/<pid>/cgroup` body place the process inside `unit`? A bare name is a service.
pub(super) fn cgroup_names_unit(cgroup: &str, unit: &str) -> bool {
    let name = if unit.contains('.') {
        unit.to_string()
    } else {
        format!("{unit}.service")
    };
    cgroup.split(['/', '\n']).any(|seg| seg == name)
}

/// Desktop Steam via `~/.steam/steam.pid`. `None` if stale, our descendant, or in SESSION_UNIT.
fn desktop_steam_pid() -> Option<u32> {
    let home = std::env::var_os("HOME")?;
    let pid = home_steam_pid(std::path::Path::new(&home))?;
    if descends_from(pid, std::process::id()) {
        return None; // our own dedicated spawn's Steam
    }
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
    if cgroup_is_punktfunk_owned(&cgroup) {
        return None; // the host service's tree or the managed session unit
    }
    Some(pid)
}

fn cgroup_is_punktfunk_owned(cgroup: &str) -> bool {
    cgroup.contains("punktfunk-host.service") || cgroup.contains(&format!("{SESSION_UNIT}.service"))
}

/// First token, not a `steam://` URI: a bare `steam -gamepadui` needs the instance free more, not less.
pub(crate) fn is_steam_launch(cmd: &str) -> bool {
    cmd.split_whitespace().next() == Some("steam")
}

/// Pins GTK to gamescope's Xwayland. GTK tries Wayland first and, with `WAYLAND_DISPLAY` unset,
/// opens `wayland-0`: on a desktop box, the desktop's socket, a window the stream never shows.
pub(super) const GDK_X11: (&str, &str) = ("GDK_BACKEND", "x11");

/// Points a flatpak at gamescope's socket under a `wayland-` name. Flatpak reads an unset
/// `WAYLAND_DISPLAY` as `wayland-0`, the desktop's; from 1.19 it reads any other name that way too.
pub(super) const FLATPAK_WAYLAND: &str = "ln -sfn \"$GAMESCOPE_WAYLAND_DISPLAY\" \
    \"$XDG_RUNTIME_DIR/wayland-$GAMESCOPE_WAYLAND_DISPLAY\" 2>/dev/null; \
    export WAYLAND_DISPLAY=\"wayland-$GAMESCOPE_WAYLAND_DISPLAY\"; ";

/// `cmd` with gamescope's Wayland socket ([`FLATPAK_WAYLAND`]) and `--socket=x11` on its
/// `flatpak run`; `None` when it runs no flatpak. Flatpak withholds X11 from a `fallback-x11`
/// app once any Wayland socket is there.
///
/// Every other launch keeps `WAYLAND_DISPLAY` unset, as gamescope does: pressure-vessel rewrites
/// any value to `wayland-0`, which Proton's Wayland switch and the WSI layer both act on.
pub(super) fn shape_flatpak_command(cmd: &str) -> Option<String> {
    // Exec templates single-quote every element; desktop entries name `/usr/bin/flatpak`.
    let word = |t: &str| t.trim_matches('\'').to_string();
    let run = cmd
        .split_whitespace()
        .skip_while(|t| word(t).rsplit('/').next() != Some("flatpak"))
        .skip(1)
        .find(|t| !word(t).starts_with('-'))
        .filter(|t| word(t) == "run")?;
    // `run` is a subslice of `cmd`, so its end is a char boundary of `cmd`.
    let at = run.as_ptr() as usize - cmd.as_ptr() as usize + run.len();
    Some(format!(
        "{FLATPAK_WAYLAND}{} --socket=x11{}",
        &cmd[..at],
        &cmd[at..]
    ))
}

/// The box session is DRM master, so Exclusive needs it gone. A Steam launch, which stops it
/// outright for the box's one Steam instance, is not asked a second time.
pub(super) fn free_box_session_for_exclusive(steam: bool, exclusive: bool) -> bool {
    !steam && exclusive
}

/// Steam URI → insert `-gamepadui` so nested Steam is Big Picture. Idempotent. Custom cmds unchanged.
pub(super) fn shape_dedicated_command(app: &str) -> String {
    let mut it = app.split_whitespace();
    if it.next() == Some("steam") {
        let rest: Vec<&str> = it.collect();
        if !rest.contains(&"-gamepadui") && rest.iter().any(|t| t.starts_with("steam://")) {
            return format!("steam -gamepadui {}", rest.join(" "));
        }
    }
    app.to_string()
}

const RUNGAMEID: &str = "steam://rungameid/";

/// The `rungameid` URI in `app`, when it names a non-Steam shortcut.
///
/// A shortcut's id is 64-bit; a Steam game's is its 32-bit appid. Steam runs a URI off its own
/// command line the moment it finishes starting, and the UI that answers the launch resolves the
/// id against an app list it has not filled yet. Anything wider than an appid throws there, and
/// the launch waits forever for an answer nobody sends — Big Picture and desktop UI alike. Steam
/// takes the same URI once it is up, which is how Steam's own Game Mode starts a game.
pub(super) fn deferred_shortcut_uri(app: &str) -> Option<String> {
    if !is_steam_launch(app) {
        return None;
    }
    let uri = app.split_whitespace().find(|t| t.starts_with(RUNGAMEID))?;
    let id: u64 = uri[RUNGAMEID.len()..].parse().ok()?;
    (id > u64::from(u32::MAX)).then(|| uri.to_string())
}

/// `app` without `uri`, leaving the command that boots Steam with nothing to run.
pub(super) fn without_uri(app: &str, uri: &str) -> String {
    app.split_whitespace()
        .filter(|t| *t != uri)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Steam writes this line when its client is up; the UI fills its app list right after.
const STEAM_UP_MARKER: &str = "System startup time";

/// Covers a cold Steam that updates itself first. A miss sends the launch anyway.
const STEAM_UP_WAIT: Duration = Duration::from_secs(90);

/// The app list lands just behind the marker, so the launch waits out this much more.
const STEAM_UP_MARGIN: Duration = Duration::from_secs(2);

/// `steam <uri>` hands off over Steam's pipe and exits; a hung forwarder is not worth waiting on.
const STEAM_URI_BUDGET: Duration = Duration::from_secs(20);

/// The box's Steam console log, under the `.steam/steam` link a native install keeps.
fn steam_console_log() -> Option<std::path::PathBuf> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
    Some(home.join(".steam/steam/logs/console_log.txt"))
}

/// Has Steam logged [`STEAM_UP_MARKER`] past `from`? `from` resets on a log Steam truncated.
fn steam_up_since(path: &std::path::Path, from: &mut u64) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    if f.metadata().map_or(0, |m| m.len()) < *from {
        *from = 0;
    }
    if f.seek(SeekFrom::Start(*from)).is_err() {
        return false;
    }
    let mut tail = Vec::new();
    // Bounded read: a cold Steam writes a few hundred KiB before it is up. Bytes, not a
    // string: Steam's log is not UTF-8 throughout, and one bad byte would hide the marker.
    let _ = f.take(4 << 20).read_to_end(&mut tail);
    String::from_utf8_lossy(&tail).contains(STEAM_UP_MARKER)
}

/// Give the session's Steam `uri` once it is up ([`deferred_shortcut_uri`]).
///
/// The forwarder reaches the nested client over the home's `steam.pipe` and needs none of the
/// session's env. `gamescope` is this spawn's pid: with the session gone there is nothing left
/// to launch into, and the URI would land in a desktop Steam instead.
pub(super) fn hand_launch_to_steam_when_up(uri: String, gamescope: u32) {
    let spawned = std::thread::Builder::new()
        .name("pf1-steamuri".into())
        .spawn(move || {
            let log = steam_console_log();
            let mut from = log
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map_or(0, |m| m.len());
            let deadline = Instant::now() + STEAM_UP_WAIT;
            let mut up = false;
            while Instant::now() < deadline {
                if !crate::proc::pid_alive(gamescope) {
                    tracing::info!(
                        %uri,
                        "gamescope: the session ended before its Steam was up — launch not sent"
                    );
                    return;
                }
                if log.as_ref().is_some_and(|p| steam_up_since(p, &mut from)) {
                    up = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            if up {
                std::thread::sleep(STEAM_UP_MARGIN);
            } else {
                tracing::warn!(
                    %uri,
                    secs = STEAM_UP_WAIT.as_secs(),
                    "gamescope: the nested Steam never logged that it was up — handing it the \
                     launch anyway. A Steam that is still starting drops it, and Big Picture is \
                     then all this session shows"
                );
            }
            tracing::info!(%uri, steam_up = up, "gamescope: handing the launch to the nested Steam");
            let mut forward = Command::new("steam");
            forward.arg(&uri).stdout(Stdio::null()).stderr(Stdio::null());
            if let Err(e) = crate::proc::status_within(&mut forward, STEAM_URI_BUDGET) {
                tracing::warn!(%uri, error = %format!("{e:#}"), "gamescope: launch not handed to Steam");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "gamescope: deferred Steam launch thread not started");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only an autostart that ran and is gone is started again, however it exited: one still
    /// running is the desktop's Steam, one that never ran is the desktop's to start, a missing
    /// one was disabled.
    #[test]
    fn only_an_autostart_that_ran_and_exited_is_started_again() {
        let show = |load: &str, active: &str, started: &str| {
            format!(
                "LoadState={load}\nActiveState={active}\nExecMainStartTimestampMonotonic={started}\n"
            )
        };
        assert!(ran_and_exited(&show("loaded", "inactive", "1234567")));
        assert!(!ran_and_exited(&show("loaded", "active", "1234567")));
        assert!(!ran_and_exited(&show("loaded", "inactive", "0")));
        assert!(!ran_and_exited(&show("not-found", "inactive", "0")));
        assert!(ran_and_exited(&show("loaded", "failed", "1234567")));
        assert!(!ran_and_exited(&show("loaded", "failed", "0")));
        assert!(!ran_and_exited(""));
    }

    /// The restart reaches systemd only for an autostart that ran and exited, and never while a
    /// Steam holds the box's home. `systemctl` is a fake on the child's PATH.
    #[test]
    fn a_swallowed_desktop_steam_is_started_again() {
        let name = "a_swallowed_desktop_steam_is_started_again";
        let (fake, path) = crate::gamescope::tests::fake_systemctl(name);
        crate::fake::in_child(module_path!(), name, &[("PATH", &path)], || {
            let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("HOME"));
            let started = || {
                std::fs::read_to_string(home.join("systemctl-calls"))
                    .unwrap_or_default()
                    .lines()
                    .any(|l| l == format!("--user start --no-block {DESKTOP_STEAM_AUTOSTART}"))
            };
            let show = |active: &str| {
                let body = format!(
                    "LoadState=loaded\nActiveState={active}\nExecMainStartTimestampMonotonic=99\n"
                );
                std::fs::write(home.join("systemctl-show"), body).unwrap();
            };
            std::fs::write(home.join("systemctl-exit"), "0").unwrap();
            show("active");
            restart_swallowed_desktop_steam();
            assert!(!started(), "the desktop's own Steam is still running");
            show("inactive");
            restart_swallowed_desktop_steam();
            assert!(started(), "the swallowed autostart was not started again");
        });
        let _ = std::fs::remove_dir_all(&fake);
    }

    /// Steam's console log is not UTF-8 throughout. One such byte before the marker must not
    /// hold a shortcut launch for the whole wait.
    #[test]
    fn steam_is_seen_up_past_a_byte_that_is_not_utf8() {
        let dir = std::env::temp_dir().join(format!("pf-steam-log-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let log = dir.join("console-linux.txt");
        let mut bytes = b"\xff boot\n".to_vec();
        bytes.extend_from_slice(STEAM_UP_MARKER.as_bytes());
        bytes.push(b'\n');
        std::fs::write(&log, bytes).unwrap();
        assert!(steam_up_since(&log, &mut 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_steam_is_matched_to_its_own_unit_only() {
        let game_mode = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/\
                         gamescope-session-plus@steam.service\n";
        assert!(cgroup_names_unit(
            game_mode,
            "gamescope-session-plus@steam.service"
        ));
        assert!(!cgroup_names_unit(game_mode, SESSION_UNIT));
        let ours = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/\
                    punktfunk-gamescope.service/sub\n";
        assert!(cgroup_names_unit(ours, SESSION_UNIT));
        // A prefix of another unit's name is not that unit.
        assert!(!cgroup_names_unit(ours, "punktfunk"));
    }

    #[test]
    fn exclusive_frees_the_box_session_for_a_non_steam_launch_too() {
        // Non-Steam Exclusive: the box session is DRM master of the TV.
        assert!(free_box_session_for_exclusive(false, true));
        // A Steam launch is already handled by the arm above this one — and that arm is the
        // failing one (the single instance is not optional), so this gate must not also fire and
        // free the session a second time.
        assert!(!free_box_session_for_exclusive(true, true));
        // Not exclusive: the operator did not ask for their screens to go dark, so a non-Steam
        // launch must keep leaving the box's session strictly alone. This is what makes `extend`
        // and the `SharedDesktop` preset ("never blank the real monitors") mean what they say.
        assert!(!free_box_session_for_exclusive(false, false));
        assert!(!free_box_session_for_exclusive(true, false));
    }

    #[test]
    fn steam_launch_detection() {
        assert!(is_steam_launch("steam steam://rungameid/570"));
        assert!(is_steam_launch("steam -silent steam://rungameid/570"));
        assert!(!is_steam_launch("vkcube"));
        assert!(!is_steam_launch("lutris lutris:rungameid/42"));
        // A `steam_ui` launcher entry carries no URI, and must still count: it needs the single
        // instance freed and gamescope's `--steam` mode on. Gating on `steam://` would skip both
        // for the one launch that is Big Picture itself.
        assert!(is_steam_launch("steam -gamepadui"));
        assert!(is_steam_launch("steam"));
        // A command that merely mentions steam elsewhere is not a Steam client launch.
        assert!(!is_steam_launch("mygame --steam-overlay"));
    }

    /// A launch into a gamescope beside a live desktop: the host's env names the desktop.
    #[test]
    fn a_launch_into_the_session_sees_gamescope_not_the_desktop() {
        let env = |flatpak| {
            let mut c = Command::new("true");
            c.env("WAYLAND_DISPLAY", "wayland-0")
                .env("XDG_SESSION_TYPE", "wayland")
                .env("XDG_CURRENT_DESKTOP", "KDE");
            as_gamescope_child(
                &mut c,
                Some(":1".into()),
                Some("gamescope-0".into()),
                flatpak,
            );
            c.get_envs()
                .map(|(k, v)| {
                    let s = |o: &std::ffi::OsStr| o.to_string_lossy().into_owned();
                    (s(k), v.map(s))
                })
                .collect::<std::collections::HashMap<_, _>>()
        };
        let native = env(false);
        let get = |name: &str| native[name].as_deref();
        assert_eq!(get("WAYLAND_DISPLAY"), None);
        assert_eq!(get("XDG_SESSION_TYPE"), Some("x11"));
        assert_eq!(get("XDG_CURRENT_DESKTOP"), Some("gamescope"));
        assert_eq!(get("DISPLAY"), Some(":1"));
        assert_eq!(get("GAMESCOPE_WAYLAND_DISPLAY"), Some("gamescope-0"));
        assert_eq!(get("GDK_BACKEND"), Some("x11"));
        // A flatpak takes Wayland from its command's prefix; GTK in it follows that.
        assert!(!env(true).contains_key("GDK_BACKEND"));
    }

    #[test]
    fn only_flatpak_gets_gamescopes_wayland_socket() {
        let shaped = |cmd| shape_flatpak_command(cmd);
        let socket = FLATPAK_WAYLAND;
        assert_eq!(shaped("steam -gamepadui steam://rungameid/1145350"), None);
        assert_eq!(shaped("lutris lutris:rungameid/42"), None);
        assert_eq!(shaped("flatpak-spawn --host mygame"), None);
        assert_eq!(shaped("flatpak update -y"), None);
        assert_eq!(
            shaped("flatpak run com.heroicgameslauncher.hgl"),
            Some(format!(
                "{socket}flatpak run --socket=x11 com.heroicgameslauncher.hgl"
            ))
        );
        // The export reaches the whole compound command, not only its first word.
        assert_eq!(
            shaped("cd '/mnt/games' && 'flatpak' 'run' 'net.rpcs3.RPCS3' '--no-gui'"),
            Some(format!(
                "{socket}cd '/mnt/games' && 'flatpak' 'run' --socket=x11 'net.rpcs3.RPCS3' \
                 '--no-gui'"
            ))
        );
        // A flatpak desktop entry's `Exec=`, field codes stripped, behind a global option.
        assert_eq!(
            shaped("/usr/bin/flatpak --user run --branch=stable org.xonotic.Xonotic"),
            Some(format!(
                "{socket}/usr/bin/flatpak --user run --socket=x11 --branch=stable \
                 org.xonotic.Xonotic"
            ))
        );
    }

    /// Flatpak 1.19 honours only a `wayland-*` name: the prefix must leave one that reaches
    /// gamescope's socket.
    #[test]
    fn the_flatpak_prefix_links_a_wayland_name_to_gamescope() {
        let dir = std::env::temp_dir().join(format!("pf-gs-wl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("{FLATPAK_WAYLAND}printf %s \"$WAYLAND_DISPLAY\""))
            .env("GAMESCOPE_WAYLAND_DISPLAY", "gamescope-7")
            .env("XDG_RUNTIME_DIR", &dir)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "wayland-gamescope-7");
        let link = std::fs::read_link(dir.join("wayland-gamescope-7")).unwrap();
        assert_eq!(link, std::path::Path::new("gamescope-7"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dedicated_command_shaping() {
        // Steam URI → -gamepadui inserted so the nested Steam is Big Picture (not the desktop UI).
        assert_eq!(
            shape_dedicated_command("steam steam://rungameid/570"),
            "steam -gamepadui steam://rungameid/570"
        );
        // Idempotent: an already-gamepadui command is left alone.
        assert_eq!(
            shape_dedicated_command("steam -gamepadui steam://rungameid/570"),
            "steam -gamepadui steam://rungameid/570"
        );
        // Non-Steam launches and operator custom commands are untouched.
        assert_eq!(shape_dedicated_command("vkcube"), "vkcube");
        assert_eq!(
            shape_dedicated_command("lutris lutris:rungameid/42"),
            "lutris lutris:rungameid/42"
        );
        // A bare `steam` with no URI is left alone (not a game launch).
        assert_eq!(
            shape_dedicated_command("steam -bigpicture"),
            "steam -bigpicture"
        );
        // The `steam_ui` launcher entries pass through untouched — the shaping only ever fires on
        // a `steam://` game launch, so there is no way to end up with `-gamepadui` twice.
        assert_eq!(
            shape_dedicated_command("steam -gamepadui"),
            "steam -gamepadui"
        );
        assert_eq!(shape_dedicated_command("steam"), "steam");
    }

    #[test]
    fn only_a_shortcut_launch_is_held_back_from_steams_command_line() {
        // 64-bit `rungameid` (appid << 32 | the shortcut marker) — the id Steam's UI cannot
        // resolve while it is still starting.
        let shortcut = "steam -gamepadui steam://rungameid/15155503380618543104";
        assert_eq!(
            deferred_shortcut_uri(shortcut).as_deref(),
            Some("steam://rungameid/15155503380618543104")
        );
        assert_eq!(
            without_uri(shortcut, "steam://rungameid/15155503380618543104"),
            "steam -gamepadui"
        );
        // A Steam game's id IS its appid: it survives that lookup, so it keeps riding the spawn.
        assert_eq!(
            deferred_shortcut_uri("steam -gamepadui steam://rungameid/570"),
            None
        );
        // Highest appid that still fits, and the first that does not.
        assert_eq!(
            deferred_shortcut_uri("steam steam://rungameid/4294967295"),
            None
        );
        assert!(deferred_shortcut_uri("steam steam://rungameid/4294967296").is_some());
        // Other launchers and a Steam client with nothing to run carry no launch to hold.
        assert_eq!(deferred_shortcut_uri("steam -gamepadui"), None);
        assert_eq!(deferred_shortcut_uri("lutris lutris:rungameid/2"), None);
        assert_eq!(
            deferred_shortcut_uri("heroic --no-gui steam://rungameid/15155503380618543104"),
            None
        );
    }

    #[test]
    fn desktop_steam_cgroup_ownership() {
        // A desktop-launched Steam (the instance-conflict case).
        assert!(!cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-gnome-steam-48605.scope"
        ));
        // KDE spawns app scopes too; still foreign.
        assert!(!cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-steam@0f3a.service"
        ));
        // Our own dedicated spawn tree (Steam nested under the host service).
        assert!(cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/punktfunk-host.service"
        ));
        // The host-managed gamescope session unit (SESSION_UNIT).
        assert!(cgroup_is_punktfunk_owned(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/punktfunk-gamescope.service"
        ));
        assert!(!cgroup_is_punktfunk_owned(""));
    }
}
