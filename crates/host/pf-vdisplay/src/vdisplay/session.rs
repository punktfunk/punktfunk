//! Live graphical-session detection, session epoch, and session retargeting.
//!
//! [`detect_active_session`] names the compositor running for this uid.
//! [`observe_session_instance`] bumps the session epoch when that compositor
//! *instance* changes, so the registry cannot reuse a PipeWire node id from a
//! dead desktop. [`apply_session_env`] stores the live session; the host never
//! writes it into its own environment. [`settle_desktop_portal`] pushes it into
//! systemd `--user` / D-Bus activation so a mid-stream switch does not leave the
//! portal talking to the old socket.
//!
//! Evidence: `design/gamemode-and-dedicated-sessions.md`.

use super::*;

/// Upper bound for one `systemctl --user` / `dbus-update-activation-environment`.
/// A restarting session bus never answers; unbounded that hangs the stream thread.
/// 10 s covers a portal-unit restart; a miss means settle late (callers are best-effort).
#[cfg(target_os = "linux")]
const SYSTEMD_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);

/// Bumped when detection sees a different desktop compositor instance, per
/// [`classify_instance_change`]. Pooled displays stamp this at create; the registry reuses only a
/// matching epoch and reaps the rest, so a Desktop→Game→Desktop bounce cannot hand back a node id
/// from the dead compositor.
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

/// If the live desktop compositor instance changed ([`classify_instance_change`]), bump
/// [`SESSION_EPOCH`] and drop the previous backend's kept displays. A new desktop also
/// takes back the desk changes a dead host left. Idempotent per instance. Call from every site
/// that detects the session.
pub fn observe_session_instance(active: &ActiveSession) {
    let cur = (active.kind, active.compositor_pid);
    let Some((from, invalidate, scrub, epoch)) =
        advance_instance(&LAST_INSTANCE, cur, bump_session_epoch)
    else {
        return;
    };
    if let Some(old_kind) = invalidate {
        if let Some(old) = compositor_for_kind(old_kind) {
            registry::invalidate_backend(old.id());
        }
    }
    // Dead desktop WAYLAND_DISPLAY may still sit in systemd --user. Scrub now or
    // the next `gamescope-session.target` starts nested on the dead socket.
    if scrub {
        scrub_desktop_manager_env();
    }
    tracing::info!(
        from = ?from,
        to = ?cur.0,
        epoch,
        "desktop compositor instance changed — session epoch bumped"
    );
    // A desktop that just came up can run the desk undos a dead host left for it. Game Mode
    // runs none, and this is the stream thread mid-rebuild.
    if is_desktop_kind(cur.0) {
        crate::routing::reclaim_topology_journal();
    }
}

/// The decision half of [`observe_session_instance`], under `last`'s lock: advance the baseline
/// to `cur` and, on a new instance, `bump` the epoch, so a second observer of the same change
/// returns already seeing the new epoch. `Some((from, invalidate, scrub, epoch))` names the work
/// left; it runs unlocked, since it takes the registry lock and shells out on a 10 s budget.
fn advance_instance(
    last: &std::sync::Mutex<Option<(ActiveKind, Option<u32>)>>,
    cur: (ActiveKind, Option<u32>),
    bump: impl FnOnce() -> u64,
) -> Option<(ActiveKind, Option<ActiveKind>, bool, u64)> {
    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
    let prev = *last;
    // `None` is not an observation ([`classify_instance_change`]). Recording it as the
    // baseline would make the next real desktop look like `None → Desktop` and bump the
    // epoch under live pooled displays. Leave the last real instance; a miss is inert.
    if cur.0 != ActiveKind::None {
        *last = Some(cur);
    }
    let prev = prev?;
    match classify_instance_change(prev, cur) {
        InstanceChange::Nothing => None,
        InstanceChange::NewInstance { invalidate, scrub } => {
            Some((prev.0, invalidate, scrub, bump()))
        }
    }
}

/// Pure `prev` → `cur` classification, unit-tested without the process-global baseline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InstanceChange {
    Nothing,
    /// Bump the epoch. `invalidate` is the outgoing desktop whose kept nodes died with it;
    /// `None` when the outgoing session was gamescope / nothing (no pooled displays).
    /// `scrub`: drop the dead desktop's socket vars from systemd `--user`, which only a
    /// non-desktop successor wants — a new desktop has just imported its own.
    NewInstance {
        invalidate: Option<ActiveKind>,
        scrub: bool,
    },
}

/// Epoch rules, in one place so they can be unit-tested:
///
/// * [`ActiveKind::None`] is never a change. Detection answers `None` both for
///   "no session" and for a `/proc` race; treating that as a logout would drop live
///   pool entries and scrub the live session's socket vars. A real logout is
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
    let invalidate = is_desktop_kind(prev.0).then_some(prev.0);
    InstanceChange::NewInstance {
        invalidate,
        scrub: invalidate.is_some() && !is_desktop_kind(cur.0),
    }
}

/// Drop the dead desktop's socket vars from systemd `--user` so a later `gamescope-session.target`
/// does not inherit them and nest against the dead socket. Best-effort; D-Bus activation has no
/// unset. Never after a desktop restart: the new desktop has imported its own values.
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
            | ActiveKind::DesktopNiri
            | ActiveKind::DesktopUmbriel
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
    /// niri or its biri fork: JSON IPC on `$NIRI_SOCKET`, wlr virtual input.
    DesktopNiri,
    /// Umbriel: JSON IPC on `$UMBRIEL_SOCKET`, wlr virtual input.
    DesktopUmbriel,
    None,
}

pub use pf_host_config::session::SessionEnv;

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
        ActiveKind::DesktopNiri => Some(Compositor::Niri),
        ActiveKind::DesktopUmbriel => Some(Compositor::Umbriel),
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
    niri_socket: Option<String>,
    umbriel_socket: Option<String>,
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
            wayland_display: pf_host_config::session::wayland_display(),
            hyprland_signature: v("HYPRLAND_INSTANCE_SIGNATURE"),
            swaysock: v("SWAYSOCK"),
            niri_socket: v("NIRI_SOCKET"),
            umbriel_socket: v("UMBRIEL_SOCKET"),
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
    let mut umbriels = Vec::new();
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
            // biri is the niri fork with virtual outputs; `biri-altinstall` keeps its own name.
            "niri" | "biri" => (ActiveKind::DesktopNiri, 4),
            "umbriel" => {
                umbriels.push(pid);
                (ActiveKind::DesktopUmbriel, 4)
            }
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
        ActiveKind::DesktopKde
        | ActiveKind::DesktopWlroots
        | ActiveKind::DesktopHyprland
        | ActiveKind::DesktopNiri
        | ActiveKind::DesktopUmbriel => find_wayland_socket(&env, &xdg_runtime_dir, uid),
        _ => None,
    };
    let xdg_current_desktop = match kind {
        ActiveKind::DesktopKde => Some("KDE".to_string()),
        ActiveKind::DesktopGnome => Some("GNOME".to_string()),
        ActiveKind::DesktopWlroots if winning_scroll => Some("scroll".to_string()),
        ActiveKind::DesktopWlroots => Some("sway".to_string()),
        // Real desktop name so portal routing (`[Hyprland]`) and xdph's own checks fire.
        ActiveKind::DesktopHyprland => Some("Hyprland".to_string()),
        ActiveKind::DesktopNiri => Some("niri".to_string()),
        ActiveKind::DesktopUmbriel => Some("umbriel".to_string()),
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
    let niri_socket = match kind {
        ActiveKind::DesktopNiri => find_niri_socket(&env, &xdg_runtime_dir, uid, winning_pid),
        _ => None,
    };
    // A `niri msg` child shares the compositor's name; the socket names the compositor itself,
    // and counts only while that compositor runs.
    if let Some(pid) = niri_socket
        .as_deref()
        .and_then(ipc_socket_pid)
        .filter(|&pid| crate::proc::pid_alive(pid))
    {
        winning_pid = Some(pid);
    }
    // A `umbriel msg` child shares the compositor's name too; the socket's holder is the
    // compositor. Its socket is found per call ([`umbriel_socket`]), not stored.
    if kind == ActiveKind::DesktopUmbriel {
        winning_pid = find_umbriel_socket(&env, &xdg_runtime_dir, uid, wayland_display.as_deref())
            .and_then(|s| listener_pid(&s, &umbriels))
            .or_else(|| {
                umbriels
                    .iter()
                    .copied()
                    .min_by_key(|&p| crate::proc::start_ticks(p))
            });
    }
    ActiveSession {
        kind,
        env: SessionEnv {
            wayland_display,
            xdg_runtime_dir,
            dbus_session_bus_address: dbus,
            xdg_current_desktop,
            hyprland_signature,
            sway_socket,
            niri_socket,
        },
        compositor_pid: winning_pid,
    }
}

/// Live Hyprland instance signature for this uid: the inherited one while its `.socket.sock`
/// answers ([`inherited_live`]), else the newest owned instance dir under
/// `$XDG_RUNTIME_DIR/hypr/` that still has `.socket.sock`.
#[cfg(target_os = "linux")]
fn find_hypr_signature(env: &EnvProbe, runtime: &str, uid: u32) -> Option<String> {
    let hypr = std::path::Path::new(runtime).join("hypr");
    if let Some(sig) = &env.hyprland_signature {
        if inherited_live(&hypr.join(sig).join(".socket.sock")) {
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

/// The compositor pid a sway, scroll or niri IPC socket name ends in; `None` for any other name.
#[cfg(target_os = "linux")]
fn ipc_socket_pid(path: &str) -> Option<u32> {
    let name = std::path::Path::new(path).file_name()?.to_str()?;
    let known = is_niri_socket_name(name)
        || SWAY_IPC_STEMS
            .iter()
            .any(|stem| name.starts_with(&format!("{stem}.")));
    let stem = name.strip_suffix(".sock").filter(|_| known)?;
    stem.rsplit('.').next()?.parse().ok()
}

/// Whether an inherited IPC socket still has its compositor: the pid its name ends in runs, or,
/// for a name without one, a connect succeeds. A crashed compositor leaves the file behind.
#[cfg(target_os = "linux")]
fn inherited_live(path: &std::path::Path) -> bool {
    match path.to_str().and_then(ipc_socket_pid) {
        Some(pid) => path.exists() && crate::proc::pid_alive(pid),
        None => connect_within(path).is_ok(),
    }
}

/// Live sway-protocol IPC socket for this uid: `<stem>.<uid>.<pid>.sock` for the detected
/// compositor PID (identity, not a guess); else the inherited path while its compositor runs
/// ([`inherited_live`]); else newest owned `<stem>.<uid>.*.sock` (re-exec / wrapper). `None` on
/// river: no sway IPC, and the wlroots backend talks through `swaymsg`.
#[cfg(target_os = "linux")]
fn find_sway_socket(env: &EnvProbe, runtime: &str, uid: u32, pid: Option<u32>) -> Option<String> {
    if let Some(pid) = pid {
        for stem in SWAY_IPC_STEMS {
            let exact = std::path::Path::new(runtime).join(format!("{stem}.{uid}.{pid}.sock"));
            if exact.exists() {
                return Some(exact.to_string_lossy().into_owned());
            }
        }
    }
    if let Some(s) = &env.swaysock {
        if inherited_live(std::path::Path::new(s)) {
            return Some(s.clone());
        }
    }
    let prefixes = SWAY_IPC_STEMS.map(|stem| format!("{stem}.{uid}."));
    newest_owned(std::path::Path::new(runtime), uid, |name, _, path| {
        (prefixes.iter().any(|p| name.starts_with(p)) && name.ends_with(".sock"))
            .then(|| path.to_string_lossy().into_owned())
    })
}

/// niri names its IPC socket `niri.<wayland-display>.<pid>.sock` (biri too).
#[cfg(target_os = "linux")]
fn is_niri_socket_name(name: &str) -> bool {
    name.starts_with("niri.") && name.ends_with(".sock")
}

/// Live niri IPC socket for this uid: the one `pid` opened, else the inherited `NIRI_SOCKET`
/// while its compositor runs ([`inherited_live`]), else the newest owned `niri.*.sock`.
#[cfg(target_os = "linux")]
fn find_niri_socket(env: &EnvProbe, runtime: &str, uid: u32, pid: Option<u32>) -> Option<String> {
    let dir = std::path::Path::new(runtime);
    if let Some(pid) = pid {
        let suffix = format!(".{pid}.sock");
        let exact = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .find_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                (is_niri_socket_name(&name) && name.ends_with(&suffix))
                    .then(|| e.path().to_string_lossy().into_owned())
            });
        if exact.is_some() {
            return exact;
        }
    }
    if let Some(s) = &env.niri_socket {
        if inherited_live(std::path::Path::new(s)) {
            return Some(s.clone());
        }
    }
    newest_owned(dir, uid, |name, _, path| {
        is_niri_socket_name(name).then(|| path.to_string_lossy().into_owned())
    })
}

/// `NIRI_SOCKET` for the niri backend, resolved per call like [`sway_socket`].
#[cfg(target_os = "linux")]
pub(crate) fn niri_socket() -> Option<String> {
    let probe = EnvProbe::sample();
    let runtime = default_runtime_dir(&probe);
    find_niri_socket(&probe, &runtime, crate::proc::current_uid(), None)
}

/// Live Umbriel IPC socket for this uid: the inherited `UMBRIEL_SOCKET` while it exists, else
/// `umbriel-<wayland>.sock` beside the Wayland socket, else the newest owned `umbriel-*.sock`.
#[cfg(target_os = "linux")]
fn find_umbriel_socket(
    env: &EnvProbe,
    runtime: &str,
    uid: u32,
    wayland: Option<&str>,
) -> Option<String> {
    if let Some(s) = env
        .umbriel_socket
        .as_ref()
        .filter(|s| std::path::Path::new(s).exists())
    {
        return Some(s.clone());
    }
    let dir = std::path::Path::new(runtime);
    if let Some(w) = wayland {
        let exact = dir.join(format!("umbriel-{w}.sock"));
        if exact.exists() {
            return Some(exact.to_string_lossy().into_owned());
        }
    }
    newest_owned(dir, uid, |name, _, path| {
        (name.starts_with("umbriel-") && name.ends_with(".sock"))
            .then(|| path.to_string_lossy().into_owned())
    })
}

/// `UMBRIEL_SOCKET` for the Umbriel backend, resolved per call like [`niri_socket`].
#[cfg(target_os = "linux")]
pub(crate) fn umbriel_socket() -> Option<String> {
    let probe = EnvProbe::sample();
    let runtime = default_runtime_dir(&probe);
    let wayland = probe.wayland_display.clone();
    find_umbriel_socket(
        &probe,
        &runtime,
        crate::proc::current_uid(),
        wayland.as_deref(),
    )
}

/// The inode of the socket listening at `path`, from `/proc/net/unix` text. Accepted
/// connections list the same path; only the listener carries `__SO_ACCEPTCON` (0x10000).
#[cfg(target_os = "linux")]
fn listening_inode(net_unix: &str, path: &str) -> Option<u64> {
    net_unix.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        let flags = u32::from_str_radix(f.get(3)?, 16).ok()?;
        (f.get(7) == Some(&path) && flags & 0x1_0000 != 0).then(|| f.get(6)?.parse().ok())?
    })
}

/// Which of `pids` holds the socket listening at `path`: the compositor, never a CLI child.
#[cfg(target_os = "linux")]
fn listener_pid(path: &str, pids: &[u32]) -> Option<u32> {
    let net = std::fs::read_to_string("/proc/net/unix").ok()?;
    let want = format!("socket:[{}]", listening_inode(&net, path)?);
    pids.iter().copied().find(|pid| {
        std::fs::read_dir(format!("/proc/{pid}/fd"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|l| l.as_os_str() == want.as_str()))
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

/// Live `wayland-N` socket in `runtime` for this uid. Inherited `WAYLAND_DISPLAY` if it still
/// exists; else the newest-mtime owned one ([`is_wayland_socket_name`]).
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
        is_wayland_socket_name(name).then(|| name.to_string())
    })
}

/// `wayland-` and digits, as `wl_display_add_socket_auto` names a compositor's socket. Other
/// clients put sockets beside it under the same prefix (awww's `wayland-1-awww-daemon.sock`).
#[cfg(target_os = "linux")]
fn is_wayland_socket_name(name: &str) -> bool {
    name.strip_prefix("wayland-")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Point the host at the live session: store it ([`pf_host_config::session::set`]). Every
/// reader of the four session variables, the bus, PipeWire and each child
/// ([`pf_host_config::session::export_to`]) takes them from there. Nothing live stores no
/// desktop name, so a stale `GNOME` after a compositor crash cannot route into a dead session.
#[cfg(target_os = "linux")]
pub fn apply_session_env(active: &ActiveSession) {
    pf_host_config::session::set(active.env.clone(), active.kind != ActiveKind::None);
}

#[cfg(not(target_os = "linux"))]
pub fn apply_session_env(_active: &ActiveSession) {}

/// A connection to the live compositor: the stored session's socket within
/// [`CONNECT_BUDGET`], or the environment's while nothing is stored (a host started inside its
/// desktop, before the first connect).
#[cfg(target_os = "linux")]
pub(crate) fn wayland_connect() -> Result<wayland_client::Connection, wayland_client::ConnectError>
{
    use wayland_client::{ConnectError, Connection};
    let Some(session) = pf_host_config::session::get() else {
        return Connection::connect_to_env();
    };
    let path = session.wayland_socket().ok_or(ConnectError::NoCompositor)?;
    let stream = connect_within(&path).map_err(|_| ConnectError::NoCompositor)?;
    Connection::from_socket(stream)
}

/// One connect's ceiling. A compositor that stops accepting fills its listen backlog, and a
/// plain `connect` then waits for it without end; a live one drains the backlog within frames.
#[cfg(target_os = "linux")]
const CONNECT_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Connect to the Unix socket at `path` within [`CONNECT_BUDGET`]. Linux bounds a Unix
/// connect's wait for backlog room by `SO_SNDTIMEO`; a nonblocking one answers `EAGAIN`, with
/// no pending connect to poll. The timeout is cleared before the stream is handed on.
#[cfg(target_os = "linux")]
fn connect_within(path: &std::path::Path) -> std::io::Result<std::os::unix::net::UnixStream> {
    use socket2::{Domain, SockAddr, Socket, Type};
    let socket = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    socket.set_write_timeout(Some(CONNECT_BUDGET))?;
    socket.connect(&SockAddr::unix(path)?)?;
    socket.set_write_timeout(None)?;
    Ok(socket.into())
}

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
    let mut recover = std::process::Command::new("/bin/sh");
    recover
        .arg("-c")
        .arg(&cmd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    match pf_host_config::session::export_to(&mut recover).spawn() {
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
/// activation, and on KWin and Hyprland restart the portal so it re-reads it. Best-effort.
/// GNOME uses Mutter's direct EIS (no xdg portal), so it only needs the env push.
#[cfg(target_os = "linux")]
pub fn settle_desktop_portal(chosen: Compositor) {
    const VARS: &[&str] = &[
        "WAYLAND_DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
    ];
    // Both tools forward these from their own environment, which `status_within` fills from
    // the stored session.
    let mut import = std::process::Command::new("systemctl");
    import.args(["--user", "import-environment"]).args(VARS);
    let _ = crate::proc::status_within(&mut import, SYSTEMD_BUDGET);
    let mut activation = std::process::Command::new("dbus-update-activation-environment");
    activation.arg("--systemd").args(VARS);
    let _ = crate::proc::status_within(&mut activation, SYSTEMD_BUDGET);
    // On KWin, libei input rides the xdg RemoteDesktop portal (fake_input does not), which keys
    // the backend off its *startup* `XDG_CURRENT_DESKTOP`. Restart, then wait 600 ms for it to
    // re-read before the injector reopens. Plasma 6 names the KDE backend
    // `plasma-xdg-desktop-portal-kde`; older builds drop the prefix.
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
                invalidate: Some(ActiveKind::DesktopKde),
                scrub: false,
            },
            "the new desktop imported its own socket vars; scrubbing takes them away"
        );
        assert_eq!(
            classify_instance_change(
                (ActiveKind::DesktopKde, Some(1)),
                (ActiveKind::Gaming, Some(2))
            ),
            InstanceChange::NewInstance {
                invalidate: Some(ActiveKind::DesktopKde),
                scrub: true,
            }
        );
        assert_eq!(
            classify_instance_change(
                (ActiveKind::Gaming, Some(1)),
                (ActiveKind::DesktopKde, Some(2))
            ),
            InstanceChange::NewInstance {
                invalidate: None,
                scrub: false,
            }
        );
    }

    /// Two observers of one change: the decision alone bumps the epoch, so the second returns
    /// seeing it before the first has invalidated or scrubbed anything.
    #[test]
    fn a_second_observer_of_a_change_sees_the_new_epoch_at_once() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let last = std::sync::Mutex::new(None);
        let epoch = AtomicU64::new(7);
        let bump = || epoch.fetch_add(1, Ordering::Relaxed) + 1;
        let first = (ActiveKind::DesktopKde, Some(1));
        assert_eq!(advance_instance(&last, first, bump), None, "a baseline");
        let restart = (ActiveKind::DesktopKde, Some(2));
        assert_eq!(
            advance_instance(&last, restart, bump),
            Some((
                ActiveKind::DesktopKde,
                Some(ActiveKind::DesktopKde),
                false,
                8
            ))
        );
        assert_eq!(advance_instance(&last, restart, bump), None);
        assert_eq!(
            epoch.load(Ordering::Relaxed),
            8,
            "bumped once, by the decision"
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
                invalidate: Some(ActiveKind::DesktopKde),
                scrub: false,
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

    #[test]
    fn only_a_numbered_wayland_socket_is_a_compositors() {
        for (name, want) in [
            ("wayland-0", true),
            ("wayland-12", true),
            ("wayland-1.lock", false),
            ("wayland-1-awww-daemon.sock", false),
            ("wayland-", false),
            ("wayland-x", false),
        ] {
            assert_eq!(is_wayland_socket_name(name), want, "{name}");
        }
    }

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

    /// Filename filter only (`sway-ipc.<uid>.` prefix). The ownership guard is
    /// `another_uids_owned_socket_is_ignored`.
    #[test]
    fn another_uids_socket_name_is_ignored() {
        let rt = FakeRuntime::new("otheruid", &[]);
        let other = rt.uid.wrapping_add(1);
        std::fs::write(rt.dir.join(format!("sway-ipc.{other}.500.sock")), b"").unwrap();
        let got = find_sway_socket(&no_inherited_env(), rt.path(), rt.uid, Some(500));
        assert_eq!(got, None);
    }

    /// No process has this pid: it is above the kernel's `pid_max` ceiling (2^22).
    const DEAD_PID: u32 = 4_194_305;

    /// Make `path` older than anything made after it, as a crashed compositor's leftover is.
    fn backdate(path: &std::path::Path) {
        let then = std::time::UNIX_EPOCH + std::time::Duration::from_secs(3600);
        std::fs::File::open(path)
            .and_then(|f| f.set_modified(then))
            .unwrap();
    }

    /// A crashed sway leaves its socket. Inherited, it loses to the live one beside it, whether
    /// or not detection named the live pid.
    #[test]
    fn an_inherited_socket_of_a_dead_sway_is_passed_over() {
        let live = std::process::id();
        let rt = FakeRuntime::new("deadsway", &[DEAD_PID, live]);
        let stale = format!("{}/sway-ipc.{}.{DEAD_PID}.sock", rt.path(), rt.uid);
        backdate(std::path::Path::new(&stale));
        let env = EnvProbe {
            swaysock: Some(stale),
            ..EnvProbe::default()
        };
        let want = format!("{}/sway-ipc.{}.{live}.sock", rt.path(), rt.uid);
        for pid in [Some(live), None] {
            let got = find_sway_socket(&env, rt.path(), rt.uid, pid);
            assert_eq!(got.as_deref(), Some(want.as_str()), "pid {pid:?}");
        }
    }

    /// An inherited socket whose compositor runs is taken as it is, wherever it lives.
    #[test]
    fn an_inherited_socket_of_a_live_sway_is_kept() {
        let rt = FakeRuntime::new("livesway", &[]);
        let elsewhere = FakeRuntime::new("livesway-elsewhere", &[std::process::id()]);
        let sock = format!(
            "{}/sway-ipc.{}.{}.sock",
            elsewhere.path(),
            rt.uid,
            std::process::id()
        );
        let env = EnvProbe {
            swaysock: Some(sock.clone()),
            ..EnvProbe::default()
        };
        assert_eq!(find_sway_socket(&env, rt.path(), rt.uid, None), Some(sock));
    }

    /// niri after a `kill -9` and a restart: the inherited `NIRI_SOCKET` names the dead one.
    #[test]
    fn an_inherited_socket_of_a_dead_niri_is_passed_over() {
        let live = std::process::id();
        let rt = FakeRuntime::new("deadniri", &[]);
        let stale = rt.dir.join(format!("niri.wayland-1.{DEAD_PID}.sock"));
        let fresh = rt.dir.join(format!("niri.wayland-1.{live}.sock"));
        std::fs::write(&stale, b"").unwrap();
        std::fs::write(&fresh, b"").unwrap();
        backdate(&stale);
        let env = EnvProbe {
            niri_socket: Some(stale.to_string_lossy().into_owned()),
            ..EnvProbe::default()
        };
        let want = fresh.to_string_lossy().into_owned();
        for pid in [Some(live), None] {
            let got = find_niri_socket(&env, rt.path(), rt.uid, pid);
            assert_eq!(got.as_deref(), Some(want.as_str()), "pid {pid:?}");
        }
    }

    /// Umbriel's socket: the inherited path while it exists, else the one named after the
    /// Wayland socket, else the newest owned `umbriel-*.sock`.
    #[test]
    fn the_umbriel_socket_is_found_inherited_by_wayland_name_or_newest() {
        let rt = FakeRuntime::new("umbriel", &[]);
        let ours = rt.dir.join("umbriel-wayland-1.sock");
        let other = rt.dir.join("umbriel-wayland-9.sock");
        std::fs::write(&ours, b"").unwrap();
        std::fs::write(&other, b"").unwrap();
        backdate(&ours);
        let path = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let inherited = EnvProbe {
            umbriel_socket: Some(path(&ours)),
            ..EnvProbe::default()
        };
        let find = |env: &EnvProbe, w| find_umbriel_socket(env, rt.path(), rt.uid, w);
        assert_eq!(find(&inherited, None), Some(path(&ours)));
        let gone = EnvProbe {
            umbriel_socket: Some(path(&rt.dir.join("umbriel-wayland-0.sock"))),
            ..EnvProbe::default()
        };
        assert_eq!(find(&gone, Some("wayland-1")), Some(path(&ours)));
        assert_eq!(find(&gone, None), Some(path(&other)), "the newest");
    }

    /// Only the listener carries `__SO_ACCEPTCON`; an accepted connection lists the same path.
    #[test]
    fn the_listening_socket_is_told_from_its_connections() {
        let net = "Num       RefCount Protocol Flags    Type St Inode Path\n\
            0000000000000000: 00000003 00000000 00000000 0001 03 4711 /run/user/1000/umbriel-wayland-1.sock\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 4242 /run/user/1000/umbriel-wayland-1.sock\n\
            0000000000000000: 00000002 00000000 00010000 0001 01 99 /run/user/1000/wayland-1\n";
        let sock = "/run/user/1000/umbriel-wayland-1.sock";
        assert_eq!(listening_inode(net, sock), Some(4242));
        assert_eq!(listening_inode(net, "/run/user/1000/nope.sock"), None);
        // A pid holding no such socket is never the compositor.
        assert_eq!(listener_pid(sock, &[std::process::id()]), None);
    }

    /// A Hyprland signature names no pid: the inherited one counts while its socket answers.
    #[test]
    fn an_inherited_hyprland_signature_counts_while_its_socket_answers() {
        let rt = FakeRuntime::new("hypr", &[]);
        let dead = rt.dir.join("hypr/dead_sig");
        let live = rt.dir.join("hypr/live_sig");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(dead.join(".socket.sock"), b"").unwrap();
        backdate(&dead);
        let _listener = std::os::unix::net::UnixListener::bind(live.join(".socket.sock")).unwrap();
        for inherited in ["dead_sig", "live_sig"] {
            let env = EnvProbe {
                hyprland_signature: Some(inherited.into()),
                ..EnvProbe::default()
            };
            let got = find_hypr_signature(&env, rt.path(), rt.uid);
            assert_eq!(got.as_deref(), Some("live_sig"), "inherited {inherited}");
        }
    }

    /// A listening socket connects; a leftover file and a missing path do not.
    #[test]
    fn a_budgeted_connect_reaches_only_a_listener() {
        let rt = FakeRuntime::new("connect", &[]);
        let live = rt.dir.join("wayland-1");
        let _listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
        assert!(connect_within(&live).is_ok());
        let stale = rt.dir.join("wayland-0");
        std::fs::write(&stale, b"").unwrap();
        assert!(connect_within(&stale).is_err());
        assert!(connect_within(&rt.dir.join("wayland-9")).is_err());
    }

    /// Only the sway, scroll and niri name shapes carry a pid.
    #[test]
    fn only_a_known_ipc_socket_name_carries_a_pid() {
        assert_eq!(ipc_socket_pid("/run/niri.wayland-1.4242.sock"), Some(4242));
        assert_eq!(ipc_socket_pid("/run/sway-ipc.1000.77.sock"), Some(77));
        assert_eq!(ipc_socket_pid("/run/scroll-ipc.1000.77.sock"), Some(77));
        assert_eq!(ipc_socket_pid("/tmp/my.42.sock"), None);
        assert_eq!(ipc_socket_pid("/run/hypr/sig/.socket.sock"), None);
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

    /// Keys match whole, so `WAYLAND_DISPLAY` is not `DISPLAY` — a Wayland-only session must not
    /// be mistaken for an X11 one.
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
