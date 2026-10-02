//! Live graphical-session detection, session epoch, and process-env retargeting.
//!
//! [`detect_active_session`] names the compositor running for this uid.
//! [`observe_session_instance`] bumps the session epoch when that compositor
//! *instance* changes, so the registry cannot reuse a PipeWire node id from a
//! dead desktop. [`apply_session_env`] writes the live session into the process
//! env under [`super::ENV_LOCK`]; [`settle_desktop_portal`] pushes it into
//! systemd `--user` / D-Bus activation so a mid-stream switch does not leave
//! the portal talking to the old socket.
//!
//! Evidence: `design/gamemode-and-dedicated-sessions.md`,
//! `design/hyprland-support.md`.

use super::*;

/// Upper bound for one `systemctl --user` / `dbus-update-activation-environment`.
/// A restarting session bus never answers; unbounded that hangs the stream thread.
/// 10 s covers a portal-unit restart; a miss means settle late (callers are best-effort).
#[cfg(target_os = "linux")]
const SYSTEMD_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Bumped when detection sees a different compositor instance (kind change, or same kind new PID).
/// Pooled displays stamp this at create; the registry reuses only a matching epoch and reaps the
/// rest, so a Desktop→Game→Desktop bounce cannot hand back a node id from the dead compositor.
static SESSION_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub fn session_epoch() -> u64 {
    SESSION_EPOCH.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn bump_session_epoch() -> u64 {
    SESSION_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// Last observed `(kind, pid)`, so [`observe_session_instance`] can tell a restart from a re-detect.
static LAST_INSTANCE: std::sync::Mutex<Option<(ActiveKind, Option<u32>)>> =
    std::sync::Mutex::new(None);

/// If the live compositor instance changed — different [`ActiveKind`], or same kind new PID —
/// bump [`SESSION_EPOCH`] and drop the previous backend's kept displays.
/// Idempotent per instance. Call from every site that detects the session.
pub fn observe_session_instance(active: &ActiveSession) {
    let cur = (active.kind, active.compositor_pid);
    // Decide under the lock, act outside it. The action takes the registry lock and shells out
    // to `systemctl` on a 10 s budget; holding LAST_INSTANCE across that blocks every detector.
    // Advance the baseline inside the lock so a concurrent observer cannot run the action twice.
    let changed = {
        let mut last = LAST_INSTANCE.lock().unwrap_or_else(|e| e.into_inner());
        let prev = *last;
        // `None` is not an observation ([`classify_instance_change`]). Recording it as the
        // baseline would make the next real desktop look like `None → Desktop` and bump the
        // epoch under live pooled displays. Leave the last real instance; a miss is inert.
        if cur.0 != ActiveKind::None {
            *last = Some(cur);
        }
        prev
    };
    if let Some(prev) = changed {
        if let InstanceChange::NewInstance { invalidate } = classify_instance_change(prev, cur) {
            if let Some(old_kind) = invalidate {
                if let Some(old) = compositor_for_kind(old_kind) {
                    registry::invalidate_backend(old.id());
                }
                // Dead desktop WAYLAND_DISPLAY may still sit in systemd --user. Scrub now or
                // the next `gamescope-session.target` starts nested on the dead socket.
                scrub_desktop_manager_env();
            }
            let epoch = bump_session_epoch();
            tracing::info!(
                from = ?prev.0,
                to = ?cur.0,
                epoch,
                "desktop compositor instance changed — session epoch bumped"
            );
        }
    }
}

/// Pure `prev` → `cur` classification, unit-tested without the process-global baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstanceChange {
    Nothing,
    /// Bump the epoch. `invalidate` is the outgoing desktop whose kept nodes died with it;
    /// `None` when the outgoing session was gamescope / nothing (no pooled displays).
    NewInstance {
        invalidate: Option<ActiveKind>,
    },
}

/// Epoch rules, in one place so they can be unit-tested:
///
/// * [`ActiveKind::None`] is never a change. Detection answers `None` both for
///   "no session" and for a `/proc` race; treating that as a logout dropped live
///   pool entries and scrubbed the live session's socket vars. A real logout is
///   the next real observation.
/// * Only a **desktop** compositor instance change counts. Gamescope is not
///   pooled, and a dedicated spawn's nodes outlive any active-session change, so
///   Gaming↔Gaming flaps must not bump or invalidate.
/// * A same-kind PID change is a change: a fresh KWin's node-id space is unrelated.
fn classify_instance_change(
    prev: (ActiveKind, Option<u32>),
    cur: (ActiveKind, Option<u32>),
) -> InstanceChange {
    if cur.0 == ActiveKind::None
        || prev == cur
        || !(is_desktop_kind(prev.0) || is_desktop_kind(cur.0))
    {
        return InstanceChange::Nothing;
    }
    InstanceChange::NewInstance {
        invalidate: is_desktop_kind(prev.0).then_some(prev.0),
    }
}

/// Drop the dead desktop's socket vars from systemd `--user` so a later `gamescope-session.target`
/// does not inherit them and nest against the dead socket. Best-effort; D-Bus activation has no
/// unset. A desktop restart re-imports via [`settle_desktop_portal`].
#[cfg(target_os = "linux")]
fn scrub_desktop_manager_env() {
    let _ = crate::proc::status_within(
        std::process::Command::new("systemctl").args([
            "--user",
            "unset-environment",
            "WAYLAND_DISPLAY",
            "DISPLAY",
        ]),
        SYSTEMD_BUDGET,
    );
}

#[cfg(not(target_os = "linux"))]
fn scrub_desktop_manager_env() {}

/// Desktop compositor whose kept PipeWire outputs die with the instance. `Gaming` / `None` are not.
pub(crate) fn is_desktop_kind(kind: ActiveKind) -> bool {
    matches!(
        kind,
        ActiveKind::DesktopKde
            | ActiveKind::DesktopGnome
            | ActiveKind::DesktopWlroots
            | ActiveKind::DesktopHyprland
    )
}

/// Graphical session live for this uid right now. Probed from the running compositor, not a static
/// env var, so the host follows a box that flips between Steam Gaming Mode and a desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveKind {
    Gaming,
    DesktopKde,
    DesktopGnome,
    DesktopWlroots,
    /// Distinct from [`DesktopWlroots`](ActiveKind::DesktopWlroots): own `hyprctl` IPC + xdph
    /// portal, though it shares the wlr virtual-input path.
    DesktopHyprland,
    None,
}

/// Env that points a backend at the detected session. [`apply_session_env`] writes the first four
/// into the process env; `hyprland_signature` / `sway_socket` are per-spawn (`Command::env`) because
/// `hyprctl` / `swaymsg` are children we launch, not process-wide readers.
#[derive(Clone, Debug, Default)]
pub struct SessionEnv {
    /// `WAYLAND_DISPLAY` of the live compositor. `None` for Gaming-attach / Mutter (PipeWire /
    /// D-Bus; they never talk Wayland to us).
    pub wayland_display: Option<String>,
    /// Per-user runtime dir (PipeWire + session bus). Never a world-writable `/tmp`.
    pub xdg_runtime_dir: String,
    pub dbus_session_bus_address: String,
    /// `XDG_CURRENT_DESKTOP` (KDE/GNOME/sway/Hyprland/gamescope). Drives portal/EIS routing;
    /// xdph keys Hyprland-specific behavior off `Hyprland`.
    pub xdg_current_desktop: Option<String>,
    /// `HYPRLAND_INSTANCE_SIGNATURE` (`Some` only for [`ActiveKind::DesktopHyprland`]).
    /// Handed to `hyprctl` per spawn ([`hypr_signature`]) so a systemd `--user` host need not
    /// inherit the session env.
    pub hyprland_signature: Option<String>,
    /// `SWAYSOCK` (`Some` only for sway [`ActiveKind::DesktopWlroots`]). Same per-spawn handoff
    /// as [`hypr_signature`]. `None` on river (wlroots, no sway IPC).
    pub sway_socket: Option<String>,
}

pub struct ActiveSession {
    pub kind: ActiveKind,
    pub env: SessionEnv,
    /// Winning compositor PID (`None` if nothing live). Compared across polls so a same-kind restart
    /// bumps the epoch — a fresh instance's node-id space is unrelated.
    pub compositor_pid: Option<u32>,
}

impl ActiveSession {
    // Linux always has a real session to describe; only the non-Linux stub calls this.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    fn none() -> ActiveSession {
        let probe = EnvProbe::sample();
        ActiveSession {
            kind: ActiveKind::None,
            env: SessionEnv {
                xdg_runtime_dir: default_runtime_dir(&probe),
                dbus_session_bus_address: default_bus(&probe, &default_runtime_dir(&probe)),
                ..Default::default()
            },
            compositor_pid: None,
        }
    }
}

pub fn compositor_for_kind(kind: ActiveKind) -> Option<Compositor> {
    match kind {
        ActiveKind::Gaming => Some(Compositor::Gamescope),
        ActiveKind::DesktopKde => Some(Compositor::Kwin),
        ActiveKind::DesktopGnome => Some(Compositor::Mutter),
        ActiveKind::DesktopWlroots => Some(Compositor::Wlroots),
        ActiveKind::DesktopHyprland => Some(Compositor::Hyprland),
        ActiveKind::None => None,
    }
}

/// Session-scoped vars, sampled once under [`ENV_LOCK`].
///
/// Detection must not call `std::env::var` mid-scan: writers take this lock, and glibc `setenv`
/// can realloc `environ` under a concurrent reader. Sampling up front is one acquisition; holding
/// the lock across the `/proc` walk would stall every other env reader for a directory walk that
/// the session watcher runs every second. Readers below are then pure in their inputs.
#[derive(Clone, Debug, Default)]
struct EnvProbe {
    xdg_runtime_dir: Option<String>,
    dbus_session_bus_address: Option<String>,
    wayland_display: Option<String>,
    hyprland_signature: Option<String>,
    swaysock: Option<String>,
}

impl EnvProbe {
    /// Drop `Ok("")`: an empty `XDG_RUNTIME_DIR` is not a path, and treating it as one yields a
    /// relative path off CWD.
    fn sample() -> EnvProbe {
        fn v(k: &str) -> Option<String> {
            std::env::var(k).ok().filter(|s| !s.is_empty())
        }
        crate::with_env_lock(|| EnvProbe {
            xdg_runtime_dir: v("XDG_RUNTIME_DIR"),
            dbus_session_bus_address: v("DBUS_SESSION_BUS_ADDRESS"),
            wayland_display: v("WAYLAND_DISPLAY"),
            hyprland_signature: v("HYPRLAND_INSTANCE_SIGNATURE"),
            swaysock: v("SWAYSOCK"),
        })
    }
}

/// Per-user runtime dir. Never `/tmp`: world-writable, and one caller hands the path to
/// xdg-desktop-portal-hyprland as an executable location.
#[cfg(target_os = "linux")]
pub(crate) fn runtime_dir() -> String {
    default_runtime_dir(&EnvProbe::sample())
}

#[cfg(target_os = "linux")]
fn default_runtime_dir(env: &EnvProbe) -> String {
    env.xdg_runtime_dir.clone().unwrap_or_else(|| {
        let uid = crate::proc::current_uid();
        format!("/run/user/{uid}")
    })
}

#[cfg(not(target_os = "linux"))]
fn default_runtime_dir(env: &EnvProbe) -> String {
    env.xdg_runtime_dir.clone().unwrap_or_default()
}

fn default_bus(env: &EnvProbe, runtime: &str) -> String {
    env.dbus_session_bus_address
        .clone()
        .unwrap_or_else(|| format!("unix:path={runtime}/bus"))
}

/// Graphical session live for this uid. Authority is the running compositor; a desktop outranks a
/// lingering gamescope. Cheap (`/proc` + socket scan).
#[cfg(target_os = "linux")]
pub fn detect_active_session() -> ActiveSession {
    let uid = crate::proc::current_uid();
    // One snapshot before any scan — see [`EnvProbe`]. Everything below reads this, never the env.
    let env = EnvProbe::sample();
    let xdg_runtime_dir = default_runtime_dir(&env);
    let dbus = default_bus(&env, &xdg_runtime_dir);

    // Names go through [`crate::proc::match_name`], not raw `comm`: NixOS wrappers report
    // `.<name>-wrapped`, and a cap_sys_nice KWin refuses `/proc/<pid>/exe` to an uncapped reader.
    let mut kind = ActiveKind::None;
    let mut best = 0u8;
    // So a same-kind restart (new PID) bumps the epoch, not just a kind change.
    let mut winning_pid: Option<u32> = None;
    // scroll shares sway's backend but not its desktop name, which portal routing reads.
    let mut winning_scroll = false;
    for (pid, pid_path) in crate::proc::own_pids() {
        let Some(comm) = crate::proc::match_name(&pid_path) else {
            continue;
        };
        let (k, prio) = match comm.as_str() {
            "gamescope" | "gamescope-wl" => (ActiveKind::Gaming, 1),
            "kwin_wayland" => (ActiveKind::DesktopKde, 4),
            "gnome-shell" => (ActiveKind::DesktopGnome, 4),
            // Own backend (hyprctl + xdph), not the sway/river wlroots family.
            "Hyprland" | "hyprland" => (ActiveKind::DesktopHyprland, 4),
            // scroll is a sway fork: same IPC, `scrollmsg`, `scroll-ipc.*` socket.
            "sway" | "river" | "scroll" => (ActiveKind::DesktopWlroots, 4),
            _ => continue,
        };
        // Lowest pid among same-priority hits, so `/proc` order cannot flap `winning_pid`
        // and look like a compositor restart.
        if prio > best || (prio == best && winning_pid.is_some_and(|w| pid < w)) {
            best = prio;
            kind = k;
            winning_pid = Some(pid);
            winning_scroll = comm == "scroll";
        }
    }

    let wayland_display = match kind {
        ActiveKind::DesktopKde | ActiveKind::DesktopWlroots | ActiveKind::DesktopHyprland => {
            find_wayland_socket(&env, &xdg_runtime_dir, uid)
        }
        _ => None,
    };
    let xdg_current_desktop = match kind {
        ActiveKind::DesktopKde => Some("KDE".to_string()),
        ActiveKind::DesktopGnome => Some("GNOME".to_string()),
        ActiveKind::DesktopWlroots if winning_scroll => Some("scroll".to_string()),
        ActiveKind::DesktopWlroots => Some("sway".to_string()),
        // Real desktop name so portal routing (`[Hyprland]`) and xdph's own checks fire.
        ActiveKind::DesktopHyprland => Some("Hyprland".to_string()),
        ActiveKind::Gaming => Some("gamescope".to_string()),
        ActiveKind::None => None,
    };
    let hyprland_signature = match kind {
        ActiveKind::DesktopHyprland => find_hypr_signature(&env, &xdg_runtime_dir, uid),
        _ => None,
    };
    let sway_socket = match kind {
        ActiveKind::DesktopWlroots => find_sway_socket(&env, &xdg_runtime_dir, uid, winning_pid),
        _ => None,
    };
    ActiveSession {
        kind,
        env: SessionEnv {
            wayland_display,
            xdg_runtime_dir,
            dbus_session_bus_address: dbus,
            xdg_current_desktop,
            hyprland_signature,
            sway_socket,
        },
        compositor_pid: winning_pid,
    }
}

/// Live Hyprland instance signature for this uid. Trust a valid inherited value; else the newest
/// owned instance dir under `$XDG_RUNTIME_DIR/hypr/` that still has `.socket.sock`.
#[cfg(target_os = "linux")]
fn find_hypr_signature(env: &EnvProbe, runtime: &str, uid: u32) -> Option<String> {
    let hypr = std::path::Path::new(runtime).join("hypr");
    if let Some(sig) = &env.hyprland_signature {
        if hypr.join(sig).join(".socket.sock").exists() {
            return Some(sig.clone());
        }
    }
    newest_owned(&hypr, uid, |name, md, path| {
        (md.is_dir() && path.join(".socket.sock").exists()).then(|| name.to_string())
    })
}

/// What `accept` makes of the newest entry of `dir` owned by `uid` that it takes. `accept` sees
/// the entry's name, metadata and path. Equal mtimes keep `read_dir` order.
#[cfg(target_os = "linux")]
fn newest_owned(
    dir: &std::path::Path,
    uid: u32,
    accept: impl Fn(&str, &std::fs::Metadata, &std::path::Path) -> Option<String>,
) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let md = e.metadata().ok().filter(|md| md.uid() == uid)?;
            let answer = accept(&e.file_name().to_string_lossy(), &md, &e.path())?;
            Some((md.modified().unwrap_or(std::time::UNIX_EPOCH), answer))
        })
        .min_by_key(|(mtime, _)| std::cmp::Reverse(*mtime))
        .map(|(_, answer)| answer)
}

/// sway and scroll name their IPC socket `<stem>.<uid>.<pid>.sock`.
#[cfg(target_os = "linux")]
const SWAY_IPC_STEMS: [&str; 2] = ["sway-ipc", "scroll-ipc"];

/// Live sway-protocol IPC socket for this uid. Inherited path if it still exists; else
/// `<stem>.<uid>.<pid>.sock` for the detected compositor PID (identity, not a guess); else newest
/// owned `<stem>.<uid>.*.sock` (re-exec / wrapper). `None` on river: no sway IPC, and the wlroots
/// backend talks through `swaymsg`.
#[cfg(target_os = "linux")]
fn find_sway_socket(env: &EnvProbe, runtime: &str, uid: u32, pid: Option<u32>) -> Option<String> {
    if let Some(s) = &env.swaysock {
        if std::path::Path::new(s).exists() {
            return Some(s.clone());
        }
    }
    if let Some(pid) = pid {
        for stem in SWAY_IPC_STEMS {
            let exact = std::path::Path::new(runtime).join(format!("{stem}.{uid}.{pid}.sock"));
            if exact.exists() {
                return Some(exact.to_string_lossy().into_owned());
            }
        }
    }
    let prefixes = SWAY_IPC_STEMS.map(|stem| format!("{stem}.{uid}."));
    newest_owned(std::path::Path::new(runtime), uid, |name, _, path| {
        (prefixes.iter().any(|p| name.starts_with(p)) && name.ends_with(".sock"))
            .then(|| path.to_string_lossy().into_owned())
    })
}

/// `HYPRLAND_INSTANCE_SIGNATURE` for a `hyprctl` child, resolved at spawn.
///
/// Handed via `Command::env`, not process `setenv`: nothing outside this crate reads it, and a
/// Hyprland↔sway switch must not leave a stale export.
#[cfg(target_os = "linux")]
pub(crate) fn hypr_signature() -> Option<String> {
    let probe = EnvProbe::sample();
    let runtime = default_runtime_dir(&probe);
    find_hypr_signature(&probe, &runtime, crate::proc::current_uid())
}

/// `SWAYSOCK` for a `swaymsg` child — sway counterpart of [`hypr_signature`].
///
/// No compositor PID here (detection's `/proc` scan is too expensive per spawn), so this uses
/// [`find_sway_socket`]'s inherited / newest-owned arms. `None` on river and when nothing listens.
#[cfg(target_os = "linux")]
pub(crate) fn sway_socket() -> Option<String> {
    let probe = EnvProbe::sample();
    let runtime = default_runtime_dir(&probe);
    find_sway_socket(&probe, &runtime, crate::proc::current_uid(), None)
}

/// `(DISPLAY, XAUTHORITY)` for an X11 child of the live session — Steam, Lutris, most native
/// games. Per-spawn like [`hypr_signature`]: [`apply_session_env`] cannot carry it, and a systemd
/// `--user` host has no `DISPLAY` of its own (the unit starts before the session exports one, and
/// `import-environment` never reaches a running unit).
///
/// Inherited first when its socket is still there, else read off a same-uid process already inside
/// the session. `/proc` holds the exec-time block, so the compositor's own late `setenv` is
/// invisible: what answers is something the session launched (plasmashell, the polkit agent).
#[cfg(target_os = "linux")]
pub fn session_x11_env() -> Option<(String, Option<String>)> {
    let (display, xauthority) = crate::with_env_lock(|| {
        let v = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
        (v("DISPLAY"), v("XAUTHORITY"))
    });
    if let Some(d) = display.filter(|d| x11_socket_live(d)) {
        return Some((d, xauthority));
    }
    let want_wayland = EnvProbe::sample().wayland_display;
    let mut any = None;
    for (_, path) in crate::proc::own_pids() {
        let Some((d, xauth, wayland)) = crate::proc::display_env(&path).and_then(desktop_x11)
        else {
            continue;
        };
        if !x11_socket_live(&d) {
            continue;
        }
        // The compositor we stream is the certain answer; another session's is a last resort.
        if want_wayland.is_some() && want_wayland == wayland {
            return Some((d, xauth));
        }
        any.get_or_insert((d, xauth));
    }
    any
}

/// `(DISPLAY, XAUTHORITY, WAYLAND_DISPLAY)` of one process. `None` when it names no display, or
/// when it belongs to a nested gamescope — that Xwayland dies with the game, and a desktop launch
/// aimed at it lands nowhere.
#[cfg(target_os = "linux")]
fn desktop_x11(env: crate::proc::DisplayEnv) -> Option<(String, Option<String>, Option<String>)> {
    if env.gamescope_wayland.is_some() {
        return None;
    }
    Some((env.display?, env.xauthority, env.wayland))
}

/// Does this `DISPLAY` still have a socket? Guards against a value inherited from an X server that
/// is gone. Only the local `:N[.S]` form is checkable; a `host:N` passes on trust.
#[cfg(target_os = "linux")]
fn x11_socket_live(display: &str) -> bool {
    let Some(rest) = display.strip_prefix(':') else {
        return true;
    };
    let n = rest.split('.').next().unwrap_or(rest);
    !n.is_empty() && std::path::Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
}

#[cfg(not(target_os = "linux"))]
pub fn detect_active_session() -> ActiveSession {
    ActiveSession::none()
}

/// Live `wayland-*` socket in `runtime` for this uid. Inherited `WAYLAND_DISPLAY` if it still exists;
/// else newest-mtime owned socket (skip `.lock`).
#[cfg(target_os = "linux")]
fn find_wayland_socket(env: &EnvProbe, runtime: &str, uid: u32) -> Option<String> {
    if let Some(w) = env.wayland_display.clone() {
        {
            let p = if w.starts_with('/') {
                std::path::PathBuf::from(&w)
            } else {
                std::path::Path::new(runtime).join(&w)
            };
            if p.exists() {
                return Some(w);
            }
        }
    }
    newest_owned(std::path::Path::new(runtime), uid, |name, _, _| {
        (name.starts_with("wayland-") && !name.ends_with(".lock")).then(|| name.to_string())
    })
}

/// Write the live session into the process env so backends that can only read
/// `WAYLAND_DISPLAY` / `XDG_RUNTIME_DIR` / `DBUS_SESSION_BUS_ADDRESS` / `XDG_CURRENT_DESKTOP`
/// (wayland-client, zbus, libpipewire, Mesa, [`settle_desktop_portal`]) open against it.
///
/// [`ENV_LOCK`] orders these writes against this crate's own readers. It does not make `setenv`
/// sound — see its doc — so the list stays these four.
#[cfg(target_os = "linux")]
pub fn apply_session_env(active: &ActiveSession) {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let e = &active.env;
    // SAFETY: PARTIAL, and deliberately not a proof. `_env_guard` holds [`ENV_LOCK`], which orders
    // this against every env reader and writer *inside pf-vdisplay*; streaming threads read cached
    // config, not the environment. Nothing else in the process takes that lock — not glibc, not
    // zbus, not wayland-client, not Mesa — so each write is still a race with a concurrent `getenv`.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", &e.xdg_runtime_dir);
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &e.dbus_session_bus_address);
        if let Some(w) = &e.wayland_display {
            std::env::set_var("WAYLAND_DISPLAY", w);
        }
        if let Some(d) = &e.xdg_current_desktop {
            std::env::set_var("XDG_CURRENT_DESKTOP", d);
        }
        // Nothing live: leftover session vars keep `available()` true (stale
        // `XDG_CURRENT_DESKTOP=GNOME` after a compositor crash). Clear them so the
        // handshake fails fast instead of routing into a dead session.
        if active.kind == ActiveKind::None {
            std::env::remove_var("XDG_CURRENT_DESKTOP");
            std::env::remove_var("WAYLAND_DISPLAY");
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn apply_session_env(_active: &ActiveSession) {}

/// Operator session-recovery hook (`PUNKTFUNK_RECOVER_SESSION_CMD`) when a client connects with
/// no live graphical session. Detached `sh -c`; one launch per minute so a retrying client cannot
/// stack restarts. Returns whether a recovery is underway so the handshake can tell the client to retry.
#[cfg(target_os = "linux")]
pub fn try_recover_session() -> bool {
    let Some(cmd) = pf_host_config::config().recover_session_cmd.clone() else {
        return false;
    };
    static LAST_LAUNCH: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(60);
    let mut last = LAST_LAUNCH.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|t| t.elapsed() < DEBOUNCE) {
        return true;
    }
    match std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(mut child) => {
            *last = Some(std::time::Instant::now());
            tracing::warn!(cmd = %cmd,
                "no live graphical session — launched the operator's session-recovery command");
            // Reap off-thread so the finished child never lingers as a zombie.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            true
        }
        Err(e) => {
            tracing::error!(cmd = %cmd, error = %e,
                "the session-recovery command did not launch");
            false
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn try_recover_session() -> bool {
    false
}

/// Mid-stream switch to a desktop: push the live session env into systemd `--user` / D-Bus
/// activation and restart the portal so it re-reads it. Best-effort. GNOME uses Mutter's direct
/// EIS (no xdg portal), so it only needs the env push.
#[cfg(target_os = "linux")]
pub fn settle_desktop_portal(chosen: Compositor) {
    const VARS: &[&str] = &[
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
    ];
    let _ = crate::proc::status_within(
        std::process::Command::new("systemctl")
            .args(["--user", "import-environment"])
            .args(VARS),
        SYSTEMD_BUDGET,
    );
    let _ = crate::proc::status_within(
        std::process::Command::new("dbus-update-activation-environment")
            .arg("--systemd")
            .args(VARS),
        SYSTEMD_BUDGET,
    );
    // KWin input rides the xdg RemoteDesktop portal, which keys the backend off its *startup*
    // `XDG_CURRENT_DESKTOP`. Restart, then wait 600 ms for it to re-read before the injector reopens.
    // Plasma 6 names the KDE backend `plasma-xdg-desktop-portal-kde`; older builds drop the prefix.
    if chosen == Compositor::Kwin {
        let _ = crate::proc::status_within(
            std::process::Command::new("systemctl").args([
                "--user",
                "try-restart",
                "plasma-xdg-desktop-portal-kde.service",
                "xdg-desktop-portal-kde.service",
                "xdg-desktop-portal.service",
            ]),
            SYSTEMD_BUDGET,
        );
        std::thread::sleep(std::time::Duration::from_millis(600));
    }
    // xdph ScreenCast may still hold the old Wayland/instance env; restart it the same way.
    if chosen == Compositor::Hyprland {
        let _ = crate::proc::status_within(
            std::process::Command::new("systemctl").args([
                "--user",
                "try-restart",
                "xdg-desktop-portal-hyprland.service",
                "xdg-desktop-portal.service",
            ]),
            SYSTEMD_BUDGET,
        );
        std::thread::sleep(std::time::Duration::from_millis(600));
    }
    tracing::info!(
        compositor = chosen.id(),
        "settled desktop portal env for the switched-to session"
    );
}

#[cfg(not(target_os = "linux"))]
pub fn settle_desktop_portal(_chosen: Compositor) {}

/// Epoch rules are pure over [`ActiveKind`] + PID, so these run on every host this crate builds on.
#[cfg(test)]
mod instance_change_tests {
    use super::*;

    /// A `None` scan while a desktop is still up must not invalidate — that drops live pool entries.
    #[test]
    fn a_none_observation_is_never_a_change() {
        for prev in [
            (ActiveKind::DesktopKde, Some(42)),
            (ActiveKind::DesktopGnome, Some(7)),
            (ActiveKind::Gaming, Some(9)),
            (ActiveKind::None, None),
        ] {
            assert_eq!(
                classify_instance_change(prev, (ActiveKind::None, None)),
                InstanceChange::Nothing,
                "a None scan result must not invalidate {prev:?}"
            );
        }
    }

    #[test]
    fn a_desktop_swap_invalidates_the_outgoing_desktop() {
        assert_eq!(
            classify_instance_change(
                (ActiveKind::DesktopKde, Some(1)),
                (ActiveKind::DesktopGnome, Some(2))
            ),
            InstanceChange::NewInstance {
                invalidate: Some(ActiveKind::DesktopKde)
            }
        );
        assert_eq!(
            classify_instance_change(
                (ActiveKind::DesktopKde, Some(1)),
                (ActiveKind::Gaming, Some(2))
            ),
            InstanceChange::NewInstance {
                invalidate: Some(ActiveKind::DesktopKde)
            }
        );
        assert_eq!(
            classify_instance_change(
                (ActiveKind::Gaming, Some(1)),
                (ActiveKind::DesktopKde, Some(2))
            ),
            InstanceChange::NewInstance { invalidate: None }
        );
    }

    #[test]
    fn a_same_kind_restart_is_a_new_instance_but_a_gamescope_flap_is_not() {
        assert_eq!(
            classify_instance_change(
                (ActiveKind::DesktopKde, Some(1)),
                (ActiveKind::DesktopKde, Some(2))
            ),
            InstanceChange::NewInstance {
                invalidate: Some(ActiveKind::DesktopKde)
            }
        );
        assert_eq!(
            classify_instance_change(
                (ActiveKind::DesktopKde, Some(1)),
                (ActiveKind::DesktopKde, Some(1))
            ),
            InstanceChange::Nothing
        );
        assert_eq!(
            classify_instance_change((ActiveKind::Gaming, Some(1)), (ActiveKind::Gaming, Some(2))),
            InstanceChange::Nothing
        );
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    struct FakeRuntime {
        dir: std::path::PathBuf,
        uid: u32,
    }

    impl FakeRuntime {
        fn new(tag: &str, pids: &[u32]) -> FakeRuntime {
            let uid = crate::proc::current_uid();
            let dir =
                std::env::temp_dir().join(format!("pf-swaysock-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            for pid in pids {
                std::fs::write(dir.join(format!("sway-ipc.{uid}.{pid}.sock")), b"").unwrap();
            }
            FakeRuntime { dir, uid }
        }
        fn path(&self) -> &str {
            self.dir.to_str().unwrap()
        }
    }

    impl Drop for FakeRuntime {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Empty probe so the inherited-value rung cannot decide the test. Argument, not `set_var`.
    fn no_inherited_env() -> EnvProbe {
        EnvProbe::default()
    }

    /// The socket that belongs to the compositor PID detection found, not merely *a* sway socket.
    #[test]
    fn the_socket_matching_the_detected_pid_wins() {
        let rt = FakeRuntime::new("exact", &[4242, 9999]);
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(4242));
        assert_eq!(
            got,
            Some(format!("{}/sway-ipc.{}.4242.sock", rt.path(), rt.uid))
        );
    }

    /// sway re-exec / wrapper can name the socket for a PID we did not see. One socket is unambiguous.
    #[test]
    fn an_unmatched_pid_falls_back_to_the_socket_that_is_there() {
        let rt = FakeRuntime::new("fallback", &[777]);
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(12345));
        assert_eq!(
            got,
            Some(format!("{}/sway-ipc.{}.777.sock", rt.path(), rt.uid))
        );
    }

    /// scroll's socket is `scroll-ipc.*`; both the exact-pid and the newest-owned rungs find it.
    #[test]
    fn a_scroll_socket_is_found_like_a_sway_one() {
        let rt = FakeRuntime::new("scroll", &[]);
        let sock = format!("{}/scroll-ipc.{}.4242.sock", rt.path(), rt.uid);
        std::fs::write(&sock, b"").unwrap();
        for pid in [Some(4242), None] {
            let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, pid);
            assert_eq!(got.as_deref(), Some(sock.as_str()));
        }
    }

    /// river has no sway IPC. `None` keeps [`sway_socket`] from handing `swaymsg` a dead path.
    #[test]
    fn no_sway_ipc_socket_reports_none() {
        let rt = FakeRuntime::new("none", &[]);
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(1));
        assert_eq!(got, None);
    }

    /// Filename filter only (`sway-ipc.<uid>.` prefix). The ownership guard is the next test.
    #[test]
    fn another_uids_socket_name_is_ignored() {
        let rt = FakeRuntime::new("otheruid", &[]);
        let other = rt.uid.wrapping_add(1);
        std::fs::write(rt.dir.join(format!("sway-ipc.{other}.500.sock")), b"").unwrap();
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(500));
        assert_eq!(got, None);
    }

    /// A socket named as ours but owned by someone else must fail the metadata check — the name
    /// is attacker-chosen. Ignored by default: needs root to `chown`.
    #[test]
    #[ignore = "needs root to chown the socket to another uid"]
    fn another_uids_owned_socket_is_ignored() {
        use std::os::unix::fs::MetadataExt;
        let rt = FakeRuntime::new("ownedbyother", &[]);
        // Prefix admits it; only the metadata check can reject it.
        let path = rt.dir.join(format!("sway-ipc.{}.500.sock", rt.uid));
        std::fs::write(&path, b"").unwrap();
        let target_uid = rt.uid.wrapping_add(1);
        let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();
        // SAFETY: `c_path` is a live NUL-terminated CString borrowed for the duration of the call,
        // and `chown` reads it without retaining it. `u32::MAX` is the unsigned spelling of the
        // `-1` gid that means "leave the group unchanged".
        let rc = unsafe { libc::chown(c_path.as_ptr(), target_uid, u32::MAX) };
        assert_eq!(rc, 0, "chown failed — this test needs root");
        assert_eq!(std::fs::metadata(&path).unwrap().uid(), target_uid);

        // Pid that does not match the filename, or the exact-path shortcut returns before the
        // ownership guard and this test is vacuous.
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(999));
        assert_eq!(got, None, "a socket owned by another uid must be rejected");
    }

    fn environ(vars: &[&str]) -> Vec<u8> {
        vars.join("\0").into_bytes()
    }

    fn x11_env_from_environ(raw: &[u8]) -> Option<(String, Option<String>, Option<String>)> {
        desktop_x11(crate::proc::DisplayEnv::parse(raw))
    }

    /// The session's own display, with the auth file that goes with it.
    #[test]
    fn a_session_process_yields_its_display_and_auth() {
        let raw = environ(&[
            "HOME=/home/u",
            "WAYLAND_DISPLAY=wayland-kde",
            "DISPLAY=:0",
            "XAUTHORITY=/run/user/1000/xauth_ab",
        ]);
        assert_eq!(
            x11_env_from_environ(&raw),
            Some((
                ":0".into(),
                Some("/run/user/1000/xauth_ab".into()),
                Some("wayland-kde".into())
            ))
        );
    }

    /// A nested gamescope's Xwayland dies with the game — launching a desktop app at it lands
    /// nowhere, so its block must not answer even though it carries a perfectly good `DISPLAY`.
    #[test]
    fn a_gamescope_display_is_refused() {
        let raw = environ(&["DISPLAY=:1", "GAMESCOPE_WAYLAND_DISPLAY=gamescope-0"]);
        assert_eq!(x11_env_from_environ(&raw), None);
    }

    /// An empty `GAMESCOPE_WAYLAND_DISPLAY=` names no gamescope, so the display still answers.
    #[test]
    fn an_empty_gamescope_key_is_not_a_nested_session() {
        let raw = environ(&["DISPLAY=:0", "GAMESCOPE_WAYLAND_DISPLAY="]);
        assert_eq!(x11_env_from_environ(&raw), Some((":0".into(), None, None)));
    }

    /// Every Wayland-only process has an empty or absent `DISPLAY`. Neither is a display.
    #[test]
    fn no_usable_display_reports_none() {
        assert_eq!(x11_env_from_environ(&environ(&["DISPLAY="])), None);
        assert_eq!(
            x11_env_from_environ(&environ(&["WAYLAND_DISPLAY=w-1"])),
            None
        );
    }

    /// `WAYLAND_DISPLAY` starts with the same letters as no key we read — a Wayland-only session
    /// must not be mistaken for an X11 one.
    #[test]
    fn wayland_display_is_not_read_as_display() {
        let raw = environ(&["WAYLAND_DISPLAY=:0"]);
        assert_eq!(x11_env_from_environ(&raw), None);
    }

    /// A dead X server leaves the value behind; the socket is what says it is gone.
    #[test]
    fn only_a_display_with_a_socket_is_live() {
        assert!(!x11_socket_live(":99999"));
        assert!(!x11_socket_live(":"));
        assert!(
            x11_socket_live("box.lan:0"),
            "remote displays pass on trust"
        );
    }
}
