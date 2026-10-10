//! Files the host writes for the box's systemd user units: the SteamOS headless shim and
//! drop-in, the `gamescope-session-plus@` bind drop-in, and the idle drop-in that parks
//! Game Mode during a takeover.

use super::*;

fn headless_shim_dir() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base).join("punktfunk-gsbin")
}

/// PATH shim: rewrite SteamOS's hardcoded panel args to headless at `PF_W`/`PF_H`/`PF_HZ`.
/// `PF_HZ` is [`game_hz`] on `-r` only — it must not change the negotiated resolution.
///
/// The session reads gamescope's ready line (`-R`) for 3 s, and its unit fails at 5 s without
/// it. A headless start can take longer, so the shim takes the line on its own pipe for
/// [`HEADLESS_READY_SECS`] and does what the session would: hand the line on, write
/// `gamescope-environment`, notify systemd.
fn headless_shim_body(bin: &str) -> String {
    // `$PF_HDR_ARGS` is unquoted for the same reason as in the GAMESCOPE_BIN wrapper: it is our
    // own flag list ([`hdr_args`]) and must word-split into separate argv entries.
    format!(
        r#"#!/bin/bash
W="${{PF_W:-1920}}"; H="${{PF_H:-1080}}"; HZ="${{PF_HZ:-60}}"
keep=(); ready=
while [ $# -gt 0 ]; do
  case "$1" in
    --generate-drm-mode|-w|-h|-W|-H|-O|--prefer-output) shift 2;;
    -R) ready="$2"; shift 2;;
    *) keep+=("$1"); shift;;
  esac
done
set -- --backend headless -W "$W" -H "$H" -w "$W" -h "$H" -r "$HZ" ${{PF_HDR_ARGS}} "${{keep[@]}}"
[ -n "$ready" ] || exec {bin} "$@"
own="${{0%/*}}/ready.$$"
rm -f "$own"; mkfifo "$own"
{bin} "$@" -R "$own" &
gs=$!
if read -r -t {secs} x wl <> "$own"; then
  printf '%s %s\n' "$x" "$wl" 1<> "$ready"
  export DISPLAY="$x" GAMESCOPE_WAYLAND_DISPLAY="$wl"
  env > "$XDG_RUNTIME_DIR/gamescope-environment"
  systemd-notify --ready
fi
rm -f "$own"
wait "$gs"
"#,
        secs = HEADLESS_READY_SECS,
    )
}

/// How long a headless gamescope may take to answer. The drop-in's start timeout sits above it.
const HEADLESS_READY_SECS: u32 = 40;

pub(super) fn write_headless_shim() -> Result<std::path::PathBuf> {
    let shim_body = headless_shim_body(gamescope_bin());
    let dir = headless_shim_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let shim = dir.join("gamescope");
    std::fs::write(&shim, &shim_body).with_context(|| format!("write shim {}", shim.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("chmod shim {}", shim.display()))?;
    Ok(dir)
}

/// `zz-` sorts last, overriding any distro drop-in. Runtime dir, like the bind drop-in: a host that
/// dies mid-takeover must not leave the panel's Game Mode headless past a reboot.
fn steamos_dropin_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base)
        .join("systemd/user/gamescope-session.service.d/zz-punktfunk-headless.conf")
}

/// `$HOME` copy an older host wrote. Nothing else removes it. Swept on sight.
fn legacy_steamos_dropin_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/deck".to_string());
    std::path::Path::new(&home)
        .join(".config/systemd/user/gamescope-session.service.d/zz-punktfunk-headless.conf")
}

/// SIGKILL for gamescope when the target stops: its SIGTERM leaks the NVIDIA GPU context (see
/// [`kill_unit`]). Steam stops first, cleanly, in its own `steam-launcher.service`.
const STEAMOS_KILL_LINE: &str = "KillSignal=SIGKILL\n";

pub(super) fn write_steamos_dropin(
    shim_dir: &std::path::Path,
    mode: Mode,
    hdr: bool,
) -> Result<()> {
    // Stale desktop DISPLAY/WAYLAND_DISPLAY in the manager env would make gamescope attach instead
    // of becoming the display server. The start timeout sits above the shim's ready wait.
    let body = format!(
        "[Service]\n\
         {STEAMOS_KILL_LINE}\
         TimeoutStartSec={timeout}\n\
         Environment=PATH={shim}:/usr/bin:/bin:/usr/local/bin\n\
         Environment=PF_W={w}\n\
         Environment=PF_H={h}\n\
         Environment=PF_HZ={hz}\n\
         Environment=\"PF_HDR_ARGS={hdr_args}\"\n\
         {xkb}\
         UnsetEnvironment=DISPLAY WAYLAND_DISPLAY\n",
        timeout = HEADLESS_READY_SECS + 20,
        shim = shim_dir.display(),
        xkb = xkb_unit_lines(),
        w = mode.width,
        h = mode.height,
        hz = game_hz(mode.refresh_hz),
        // Quoted: systemd `Environment=` with spaces otherwise keeps only the first flag.
        // SteamOS never reads `CUSTOM_REFRESH_RATES`; the shim only forwards `PF_HDR_ARGS`.
        hdr_args = our_flags(hdr, game_hz(mode.refresh_hz))
            .into_iter()
            // Advertised set vs `-r` = `PF_HZ` (frame-limited) — same split as `launch_session`.
            .chain(refresh_rate_args(mode.refresh_hz.max(1)))
            .collect::<Vec<_>>()
            .join(" "),
    );
    tracing::info!(game_cap = %game_cap(game_hz(mode.refresh_hz)), "gamescope: managed session's game cap");
    write_steamos_dropin_body(&body)
}

/// Hand-back: only the kill signal stays, so the restart that stops our headless gamescope uses it.
pub(super) fn write_steamos_handback_dropin() -> Result<()> {
    write_steamos_dropin_body(&format!("[Service]\n{STEAMOS_KILL_LINE}"))
}

fn write_steamos_dropin_body(body: &str) -> Result<()> {
    let path = steamos_dropin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    std::fs::write(&path, body).with_context(|| format!("write drop-in {}", path.display()))
}

/// `true` when either copy existed.
pub(super) fn remove_steamos_dropin() -> bool {
    let runtime = std::fs::remove_file(steamos_dropin_path()).is_ok();
    std::fs::remove_file(legacy_steamos_dropin_path()).is_ok() || runtime
}

/// Autologin-unit bind drop-in. Must live in `$XDG_RUNTIME_DIR`, not `$HOME`: it applies to the
/// whole `gamescope-session-plus@` template, and both paths it names are tmpfs. A `$HOME` copy
/// survives a reboot that deletes its sources, and a missing bind source fails Game Mode outright.
fn session_plus_dropin_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base)
        .join("systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-bind.conf")
}

/// `$HOME` copy of the bind drop-in: outlives the tmpfs paths it names. Swept on sight.
fn legacy_session_plus_dropin_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/deck".to_string());
    std::path::Path::new(&home)
        .join(".config/systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-bind.conf")
}

/// Idle drop-in replaces Game Mode `ExecStart`. Runtime-dir so a dead host cannot leave Game Mode
/// as a sleep; [`restore_takeover_on_startup`] still sweeps it.
pub(super) fn idle_dropin_path() -> std::path::PathBuf {
    let base = crate::session::runtime_dir();
    std::path::Path::new(&base)
        .join("systemd/user/gamescope-session-plus@.service.d/zz-punktfunk-idle.conf")
}

/// Idle `ExecStart` must actually execute: a unit that dies on start is the relogin storm.
fn sleep_binary() -> &'static str {
    ["/usr/bin/sleep", "/bin/sleep"]
        .into_iter()
        .find(|p| std::path::Path::new(p).exists())
        .unwrap_or("/usr/bin/sleep")
}

/// Idle autologin for the stream: replace `ExecStart` with sleep on the template. Steam is freed,
/// the autologin still succeeds (so the DM does not storm), and a user session-switch can still
/// be serviced — a stopped DM cannot.
pub(super) fn install_idle_dropin() -> Result<()> {
    let path = idle_dropin_path();
    let dir = path
        .parent()
        .context("the idle drop-in path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(&path, idle_dropin_body(sleep_binary()))
        .with_context(|| format!("write {}", path.display()))?;
    systemctl_user(&["daemon-reload"]);
    takeover().idle_dropin_armed = true;
    Ok(())
}

/// Empty `ExecStart=` first: the directive is list-valued, so an add-only drop-in would run the
/// real session *and* the sleep.
fn idle_dropin_body(sleep_bin: &str) -> String {
    format!("[Service]\nExecStart=\nExecStart={sleep_bin} infinity\n")
}

/// Not gated on the armed flag: a drop-in that outlived a dead host still has to be swept.
pub(super) fn remove_idle_dropin() -> bool {
    let removed = std::fs::remove_file(idle_dropin_path()).is_ok();
    takeover().idle_dropin_armed = false;
    if removed {
        systemctl_user(&["daemon-reload"]);
    }
    removed
}

/// Box-session drop-in: bind + WSI opt-out. No bind to arm → remove any drop-in (`Ok(false)`);
/// keeping a bind the host decided against is the crash-loop the backstop exists to prevent.
pub(super) fn write_session_plus_dropin(
    wrapper: &std::path::Path,
    mode: Mode,
    hdr: bool,
    wsi: WsiPlan,
) -> Result<bool> {
    let Some(bind) = arm_session_bind(wrapper) else {
        remove_session_plus_dropin();
        return Ok(false);
    };
    let path = session_plus_dropin_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let body = format!(
        "[Service]\n\
         {binds}\
         Environment=PF_HZ={hz}\n\
         Environment=\"PF_HDR_ARGS={hdr_args}\"\n\
         {xkb}\
         {wsi}",
        binds = bind.unit_lines(),
        xkb = xkb_unit_lines(),
        hz = game_hz(mode.refresh_hz),
        hdr_args = our_flags(hdr, game_hz(mode.refresh_hz)).join(" "),
        wsi = wsi.unit_lines(hdr),
    );
    tracing::info!(game_cap = %game_cap(game_hz(mode.refresh_hz)), "gamescope: managed session's game cap");
    std::fs::write(&path, body).with_context(|| format!("write drop-in {}", path.display()))?;
    Ok(true)
}

/// Both homes: runtime and [`legacy_session_plus_dropin_path`]. Caller owes `daemon-reload` if
/// anything was removed — a removal that isn't reloaded still applies at next boot.
pub(super) fn remove_session_plus_dropin() -> bool {
    // Both paths every time; short-circuit would leave the `$HOME` copy that outlives a reboot.
    let mut removed = false;
    for path in [
        session_plus_dropin_path(),
        legacy_session_plus_dropin_path(),
    ] {
        removed |= std::fs::remove_file(&path).is_ok();
    }
    removed
}

/// Remove, clear the armed flag, `daemon-reload`: the flag and the template cannot disagree.
pub(super) fn disarm_session_plus_dropin() {
    let removed = remove_session_plus_dropin();
    takeover().session_dropin_armed = false;
    if removed {
        systemctl_user(&["daemon-reload"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_dropin_replaces_exec_start_rather_than_appending() {
        let body = idle_dropin_body("/usr/bin/sleep");
        assert_eq!(
            body, "[Service]\nExecStart=\nExecStart=/usr/bin/sleep infinity\n",
            "{body}"
        );
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines[1], "ExecStart=", "the reset must come first: {body}");
        // The path is resolved per box ([`sleep_binary`]) and must reach the unit verbatim — a
        // bare `sleep` would depend on the unit's PATH, and an ExecStart that fails to execute is
        // the failing unit the display manager relogin-loops against.
        assert!(idle_dropin_body("/bin/sleep").contains("ExecStart=/bin/sleep infinity"));
    }

    /// The shim answers for a gamescope slower than the session's 3 s ready wait.
    #[test]
    fn headless_shim_relays_a_late_ready_line() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pf-shim-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = |path: &std::path::Path, body: &str| {
            std::fs::write(path, body).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let fake = dir.join("fake-gamescope");
        script(
            &fake,
            "#!/bin/bash\nprintf '%s\\n' \"$@\" > \"$PF_T/argv\"\n\
             while [ $# -gt 0 ]; do [ \"$1\" = -R ] && r=\"$2\"; shift; done\n\
             sleep 1; echo ':7 gamescope-7' > \"$r\"\n",
        );
        script(
            &bin.join("systemd-notify"),
            "#!/bin/sh\necho \"$1\" > \"$PF_T/notified\"\n",
        );
        let shim = dir.join("gamescope");
        script(&shim, &headless_shim_body(fake.to_str().unwrap()));
        let session_socket = dir.join("startup.socket");
        let status = Command::new("mkfifo")
            .arg(&session_socket)
            .status()
            .unwrap();
        assert!(status.success());

        let status = Command::new(&shim)
            .args(["--generate-drm-mode", "fixed", "-e", "-R"])
            .arg(&session_socket)
            .args(["-T", "/dev/null", "-O", "*,eDP-1"])
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("XDG_RUNTIME_DIR", &dir)
            .env("PF_T", &dir)
            .env("PF_W", "1280")
            .env("PF_H", "720")
            .status()
            .unwrap();
        assert!(status.success());

        let argv = std::fs::read_to_string(dir.join("argv")).unwrap();
        let argv: Vec<&str> = argv.lines().collect();
        assert_eq!(&argv[..4], ["--backend", "headless", "-W", "1280"]);
        assert!(
            !argv.contains(&"-O"),
            "panel args must not reach gamescope: {argv:?}"
        );
        let ready = argv[argv.iter().position(|a| *a == "-R").unwrap() + 1];
        assert_ne!(
            ready,
            session_socket.to_str().unwrap(),
            "the shim answers on its own pipe"
        );
        let env = std::fs::read_to_string(dir.join("gamescope-environment")).unwrap();
        assert!(env.lines().any(|l| l == "DISPLAY=:7"), "{env}");
        assert!(env
            .lines()
            .any(|l| l == "GAMESCOPE_WAYLAND_DISPLAY=gamescope-7"));
        let notified = std::fs::read_to_string(dir.join("notified")).unwrap();
        assert_eq!(notified.trim(), "--ready");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
