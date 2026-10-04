//! Steam launches: which ones contend for the box's single Steam instance, how a launch
//! command is shaped, and holding a non-Steam shortcut until the nested Steam is up.

use super::*;
use std::process::Stdio;

/// Run `cmd` inside the live session (managed / SteamOS / attach — [`spawn()`]'s nesting does not
/// apply). `DISPLAY` and gamescope's socket come from a process already inside, Wayland and GTK
/// from [`shape_flatpak_command`] and [`GDK_X11`]; with no such process, host env (a
/// `steam steam://…` still reaches the running Steam over its pipe).
pub fn launch_into_session(
    cmd: &str,
    seat: Option<&str>,
    steam_home: Option<&std::path::Path>,
) -> Result<std::process::Child> {
    // A seat pre-warmed moments ago may still be booting Steam, and a shortcut's URI handed to a
    // Steam that has not finished starting is never answered. Held in the launch process itself,
    // so the caller still gets one child and the session thread waits on nothing.
    let held = steam_home
        .filter(|h| seat_env_applies(cmd, seat::has_steam(h)))
        .and_then(seat_steam_log_mark)
        .and_then(|(log, at)| {
            let mut from = at; // `steam_up_since` rewinds a log Steam truncated on its own start
            let up = steam_up_since(&log, &mut from);
            let uri = reuse_holds_shortcut(cmd, up)?;
            tracing::info!(
                %uri,
                "gamescope: holding this launch until the seat's Steam says it is up"
            );
            Some(hold_launch_until_steam_up(&uri, &log, from))
        });
    let flatpak = shape_flatpak_command(cmd);
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(held.as_deref().or(flatpak.as_deref()).unwrap_or(cmd));
    // Keeps AppImageLauncher's binfmt hook from replacing an .AppImage with its dialog.
    c.env("APPIMAGELAUNCHER_DISABLE", "1");
    // A kept seat's Steam answers on its own `steam.pipe`; without this the forwarder hands the
    // launch to the box's Steam instead. Nothing else in the session takes that home.
    if let Some(home) = steam_home.filter(|h| seat_env_applies(cmd, seat::has_steam(h))) {
        c.envs(seat::env(home));
    }
    match discover_session_display_env(seat) {
        Some((x11, gamescope, _xauth)) => {
            tracing::info!(
                command = %cmd,
                x11_display = x11.as_deref().unwrap_or("-"),
                gamescope = gamescope.as_deref().unwrap_or("-"),
                "gamescope: launching into the live session"
            );
            if let Some(d) = x11 {
                c.env("DISPLAY", d);
            }
            if let Some(gs) = gamescope {
                c.env("GAMESCOPE_WAYLAND_DISPLAY", gs);
            }
            c.env_remove("WAYLAND_DISPLAY");
            if flatpak.is_none() {
                c.env(GDK_X11.0, GDK_X11.1);
            }
        }
        None => tracing::warn!(
            command = %cmd,
            "gamescope: could not discover the session's display env — spawning with the host env \
             (a `steam steam://…` launch still reaches the running Steam; other apps may not land \
             in the session)"
        ),
    }
    c.spawn()
        .context("spawn launch command into gamescope session")
}

/// Steam tears down a running game (Proton included) on the way out.
const STEAM_SHUTDOWN_WAIT: Duration = Duration::from_secs(20);

/// Half of [`STEAM_SHUTDOWN_WAIT`]. `/usr/bin/steam` is `steam.sh`, not a thin IPC forwarder;
/// `status_within` killpg's the group, so a 5 s bound kills the request before it leaves.
const STEAM_SHUTDOWN_SEND_BUDGET: Duration = Duration::from_secs(STEAM_SHUTDOWN_WAIT.as_secs() / 2);

/// Clean-exit bound before the host SIGKILLs the unit or compositor a Steam runs in. An idle
/// Steam is gone in a few seconds; the shutdown restore has 20 s for this and the box's restart.
pub(super) const STEAM_STOP_WAIT: Duration = Duration::from_secs(8);

/// Ask the Steam at `pid` to quit over its home's pipe, then wait out `wait` from the call.
/// `true` once it is gone. `seat_home` aims the request at a seat's Steam.
pub(super) fn shut_steam_down(
    pid: u32,
    wait: Duration,
    seat_home: Option<&std::path::Path>,
) -> bool {
    let deadline = Instant::now() + wait;
    let mut cmd = Command::new("steam");
    cmd.arg("-shutdown")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(home) = seat_home {
        cmd.envs(seat::env(home));
    }
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
/// SESSION_UNIT) are exempt. Timeout is an actionable error, not a no-frames retry loop.
pub(super) fn free_desktop_steam() -> Result<()> {
    let Some(pid) = desktop_steam_pid() else {
        return Ok(());
    };
    tracing::info!(
        pid,
        "freeing Steam: a desktop-session Steam holds the single instance — sending `steam -shutdown`"
    );
    if shut_steam_down(pid, STEAM_SHUTDOWN_WAIT, None) {
        tracing::info!(pid, "desktop Steam exited — single instance free");
        return Ok(());
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

/// The box home's Steam, when it runs inside `unit` (a Game Mode session or [`SESSION_UNIT`]).
pub(super) fn steam_pid_in_unit(unit: &str) -> Option<u32> {
    let home = std::env::var_os("HOME")?;
    let pid = home_steam_pid(std::path::Path::new(&home))?;
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    cgroup_names_unit(&cgroup, unit).then_some(pid)
}

/// Does a `/proc/<pid>/cgroup` body place the process inside `unit`? A bare name is a service.
fn cgroup_names_unit(cgroup: &str, unit: &str) -> bool {
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

/// May `cmd` take the seat's env? Only a launch that talks to that seat's own Steam. A Lutris,
/// Heroic or operator command would otherwise lose its config to a `HOME` holding one Steam and
/// nothing else. `has_steam` is [`seat::has_steam`].
fn seat_env_applies(cmd: &str, has_steam: bool) -> bool {
    has_steam && is_steam_launch(cmd)
}

/// Does this launch contend for the box's one Steam? A seat's Steam locks its own home, so it
/// needs neither the box's gaming session stopped nor the desktop Steam shut down — and on a
/// multi-seat box that session is somebody else playing on the TV.
pub(super) fn contends_for_box_steam(steam: bool, seat_home: bool) -> bool {
    steam && !seat_home
}

/// The box session is DRM master, so Exclusive needs it gone. A launch that already stops it
/// outright ([`contends_for_box_steam`]) is not asked a second time.
pub(super) fn free_box_session_for_exclusive(box_steam: bool, exclusive: bool) -> bool {
    !box_steam && exclusive
}

/// A seat (an isolated spawn with its own Steam home) streams beside the box's own session, so it
/// never frees that session or darkens the box's panels, whatever the topology says.
pub(super) fn seat_spawn_may_darken(
    exclusive: bool,
    isolation: Option<&crate::SessionIsolation>,
) -> bool {
    exclusive && isolation.is_none_or(|i| i.steam_home.is_none())
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

/// Steam's console log, under the `.steam/steam` link a native install keeps in its home — the
/// seat's when this session has one, else the box's.
fn steam_console_log(steam_home: Option<&std::path::Path>) -> Option<std::path::PathBuf> {
    let home = match steam_home {
        Some(h) => h.to_path_buf(),
        None => std::path::PathBuf::from(std::env::var_os("HOME")?),
    };
    Some(home.join(".steam/steam/logs/console_log.txt"))
}

/// Console-log length of each seat's Steam when that seat's gamescope was spawned, so a later
/// launch into the kept session can tell this Steam's `System startup time` from the last one's.
/// One entry per seat home; a box with no seat homes never gets one.
static SEAT_STEAM_LOG_MARK: std::sync::Mutex<Vec<(std::path::PathBuf, u64)>> =
    std::sync::Mutex::new(Vec::new());

/// Mark where this seat's Steam log stands as its gamescope starts.
pub(super) fn mark_seat_steam_log(home: &std::path::Path) {
    let Some(log) = steam_console_log(Some(home)) else {
        return;
    };
    let at = std::fs::metadata(&log).map_or(0, |m| m.len());
    let mut marks = SEAT_STEAM_LOG_MARK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match marks.iter_mut().find(|(h, _)| h == home) {
        Some((_, mark)) => *mark = at,
        None => marks.push((home.to_path_buf(), at)),
    }
}

/// `(console log, offset)` of the seat's Steam at spawn. `None` for a home no spawn marked.
fn seat_steam_log_mark(home: &std::path::Path) -> Option<(std::path::PathBuf, u64)> {
    let marks = SEAT_STEAM_LOG_MARK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let at = marks.iter().find(|(h, _)| h == home).map(|(_, at)| *at)?;
    Some((steam_console_log(Some(home))?, at))
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
    let mut tail = String::new();
    // Bounded read: a cold Steam writes a few hundred KiB before it is up.
    let _ = f.take(4 << 20).read_to_string(&mut tail);
    tail.contains(STEAM_UP_MARKER)
}

/// Give the session's Steam `uri` once it is up ([`deferred_shortcut_uri`]).
///
/// The forwarder reaches the nested client over its home's `steam.pipe`, so it needs that home
/// and none of the rest of the session's env. `gamescope` is this spawn's pid: with the session
/// gone there is nothing left to launch into, and the URI would land in the box's Steam instead.
pub(super) fn hand_launch_to_steam_when_up(
    uri: String,
    gamescope: u32,
    steam_home: Option<std::path::PathBuf>,
) {
    let spawned = std::thread::Builder::new()
        .name("pf1-steamuri".into())
        .spawn(move || {
            let log = steam_console_log(steam_home.as_deref());
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
            if let Some(home) = &steam_home {
                forward.envs(seat::env(home));
            }
            if let Err(e) = crate::proc::status_within(&mut forward, STEAM_URI_BUDGET) {
                tracing::warn!(%uri, error = %format!("{e:#}"), "gamescope: launch not handed to Steam");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "gamescope: deferred Steam launch thread not started");
    }
}

/// The launch a kept seat must hold back: a shortcut's URI ([`deferred_shortcut_uri`]) whose
/// Steam has not said it is up. A pre-warmed seat is claimed while its Steam may still be
/// starting, and a shortcut handed to that Steam waits forever. An appid, a launch with no URI,
/// or a Steam already up goes straight through.
fn reuse_holds_shortcut(cmd: &str, steam_up: bool) -> Option<String> {
    deferred_shortcut_uri(cmd).filter(|_| !steam_up)
}

/// Shell that holds `uri` until this seat's Steam logs [`STEAM_UP_MARKER`] past `from`, then
/// hands it over — the wait [`hand_launch_to_steam_when_up`] does for a cold spawn, in the one
/// process the caller gets back as its launch. A timeout forwards anyway, as that wait does.
fn hold_launch_until_steam_up(uri: &str, log: &std::path::Path, from: u64) -> String {
    format!(
        "n=0; while [ $n -lt {tries} ]; do tail -c +{at} {log} 2>/dev/null | grep -q {marker} \
         && break; n=$((n+1)); sleep 1; done; sleep {margin}; exec steam {uri}",
        tries = STEAM_UP_WAIT.as_secs(),
        at = from + 1,
        log = shell_word(&log.to_string_lossy()),
        marker = shell_word(STEAM_UP_MARKER),
        margin = STEAM_UP_MARGIN.as_secs(),
        uri = shell_word(uri),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_never_darkens_the_box() {
        let iso = |home: Option<&str>| crate::SessionIsolation {
            id: "ab12cd34".into(),
            ei_relay: "/run/user/1000/pf-ei".into(),
            sink: None,
            mic_source: None,
            steam_home: home.map(Into::into),
        };
        let seat = iso(Some("/var/lib/punktfunk/seats/ab12cd34"));
        assert!(!seat_spawn_may_darken(true, Some(&seat)));
        // A lone isolated spawn on the box's own home keeps the topology it was given.
        assert!(seat_spawn_may_darken(true, Some(&iso(None))));
        assert!(seat_spawn_may_darken(true, None));
        assert!(!seat_spawn_may_darken(false, None));
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

    /// A session's other launchers keep their own home: only Steam talks to the seat's Steam.
    #[test]
    fn only_a_steam_launch_into_a_provisioned_seat_takes_the_seat_env() {
        assert!(seat_env_applies(
            "steam -gamepadui steam://rungameid/2379780",
            true
        ));
        assert!(seat_env_applies(
            "steam steam://rungameid/13843649396736",
            true
        ));
        // A seat home nothing provisioned (Flatpak-only box) holds no Steam to reach.
        assert!(!seat_env_applies("steam -gamepadui", false));
        // Everything else a kept session launches keeps the box's home.
        for other in [
            "lutris rungameid/3",
            "heroic://launch/x",
            "/opt/game/run.sh",
            "",
        ] {
            assert!(!seat_env_applies(other, true), "{other}");
        }
    }

    /// A seat's Steam is not the box's. Stopping the box's gaming session for it would end the
    /// game on the TV, which is the whole point of seats.
    #[test]
    fn a_seat_steam_launch_leaves_the_box_session_alone_unless_exclusive() {
        assert!(
            contends_for_box_steam(true, false),
            "no seat home, one Steam"
        );
        assert!(!contends_for_box_steam(true, true));
        assert!(!contends_for_box_steam(false, true));
        // A seat Steam takes the non-Steam arm, where only Exclusive frees the box session.
        let seat = contends_for_box_steam(true, true);
        assert!(free_box_session_for_exclusive(seat, true));
        assert!(!free_box_session_for_exclusive(seat, false));
        // Without a seat home the failing arm above still owns it, exactly as before.
        assert!(!free_box_session_for_exclusive(
            contends_for_box_steam(true, false),
            true
        ));
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

    /// A launch into a kept seat waits on the same condition a cold spawn does, and only for a
    /// shortcut: everything else, and a Steam already up, is forwarded the moment it arrives.
    #[test]
    fn a_kept_seat_holds_only_a_shortcut_and_only_while_its_steam_is_starting() {
        let shortcut = "steam steam://rungameid/15155503380618543104";
        assert_eq!(
            reuse_holds_shortcut(shortcut, false).as_deref(),
            Some("steam://rungameid/15155503380618543104")
        );
        assert_eq!(reuse_holds_shortcut(shortcut, true), None, "Steam is up");
        assert_eq!(
            reuse_holds_shortcut("steam steam://rungameid/570", false),
            None
        );
        assert_eq!(reuse_holds_shortcut("steam -gamepadui", false), None);

        let held = hold_launch_until_steam_up(
            "steam://rungameid/15155503380618543104",
            std::path::Path::new("/seats/cafe0123/.steam/steam/logs/console_log.txt"),
            4096,
        );
        // Reads past what the log held at spawn, waits out the same bound, forwards either way.
        assert!(held.contains("tail -c +4097"), "{held}");
        assert!(
            held.contains(&format!("$n -lt {}", STEAM_UP_WAIT.as_secs())),
            "{held}"
        );
        assert!(held.contains(STEAM_UP_MARKER), "{held}");
        assert!(
            held.ends_with("exec steam 'steam://rungameid/15155503380618543104'"),
            "{held}"
        );
    }

    #[test]
    fn desktop_steam_cgroup_ownership() {
        // A desktop-launched Steam (the instance-conflict case, as observed on a GNOME host).
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
