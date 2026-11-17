//! The live graphical session as the host last detected it.
//!
//! pf-vdisplay's `apply_session_env` stores it. The host reads the four session variables from
//! here and hands them to each desktop-facing child through [`export_to`]; it never writes them
//! into its own environment. Until the first [`set`], each reader falls back to the environment
//! this process started with.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// Env that points a backend at the detected session. [`export_to`] hands a child only the four
/// session variables. `hyprland_signature`, `sway_socket` and `niri_socket` record what detection
/// found; `hyprctl`, `swaymsg` and the niri backend resolve theirs per call in pf-vdisplay's
/// `session` module.
#[derive(Clone, Debug, Default)]
pub struct SessionEnv {
    /// `WAYLAND_DISPLAY` of the live compositor. Detection leaves it `None` for Gaming-attach
    /// and Mutter, which never talk Wayland to us; [`set`] then keeps the last one.
    pub wayland_display: Option<String>,
    /// Per-user runtime dir (PipeWire + session bus). Never a world-writable `/tmp`.
    pub xdg_runtime_dir: String,
    pub dbus_session_bus_address: String,
    /// `XDG_CURRENT_DESKTOP`, named by detection per compositor kind; `None` when nothing is
    /// live. Drives portal/EIS routing; xdph keys Hyprland-specific behavior off `Hyprland`.
    pub xdg_current_desktop: Option<String>,
    /// `HYPRLAND_INSTANCE_SIGNATURE` (`Some` only on Hyprland). `hyprctl` gets its own at each
    /// spawn from pf-vdisplay's `hypr_signature`, so a systemd `--user` host need not inherit it.
    pub hyprland_signature: Option<String>,
    /// `SWAYSOCK` (`Some` only on sway). `swaymsg` resolves its own, as for `hyprland_signature`.
    /// `None` on river.
    pub sway_socket: Option<String>,
    /// `NIRI_SOCKET` (`Some` only on niri). The niri backend resolves its own per call.
    pub niri_socket: Option<String>,
}

impl SessionEnv {
    /// Where the compositor listens: the name under the runtime dir, or the name itself when it
    /// is absolute.
    pub fn wayland_socket(&self) -> Option<PathBuf> {
        let name = self.wayland_display.as_deref()?;
        Some(Path::new(&self.xdg_runtime_dir).join(name))
    }
}

static LIVE: RwLock<Option<Arc<SessionEnv>>> = RwLock::new(None);

/// The stored session. `None` until the first [`set`].
pub fn get() -> Option<Arc<SessionEnv>> {
    LIVE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Store the detected session. `live` is false when detection found no compositor.
pub fn set(env: SessionEnv, live: bool) {
    let mut slot = LIVE.write().unwrap_or_else(|e| e.into_inner());
    let last = match slot.as_deref() {
        Some(prev) => prev.wayland_display.clone(),
        None => inherited(),
    };
    *slot = Some(Arc::new(carry(env, live, last)));
}

/// A live session that names no socket keeps the last one; no live session clears it.
fn carry(mut env: SessionEnv, live: bool, last: Option<String>) -> SessionEnv {
    if !live {
        env.wayland_display = None;
    } else if env.wayland_display.is_none() {
        env.wayland_display = last;
    }
    env
}

fn inherited() -> Option<String> {
    inherited_var("WAYLAND_DISPLAY")
}

fn inherited_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// `WAYLAND_DISPLAY` as the host sees it: the stored session's, or the one this process
/// started with while nothing is stored.
pub fn wayland_display() -> Option<String> {
    match get() {
        Some(env) => env.wayland_display.clone(),
        None => inherited(),
    }
}

/// `XDG_RUNTIME_DIR` as the host sees it, on the same terms as [`wayland_display`].
pub fn runtime_dir() -> Option<String> {
    match get() {
        Some(env) => Some(env.xdg_runtime_dir.clone()).filter(|d| !d.is_empty()),
        None => inherited_var("XDG_RUNTIME_DIR"),
    }
}

/// The session bus address, on the same terms. `None`: the caller lets zbus pick its default.
pub fn dbus_address() -> Option<String> {
    match get() {
        Some(env) => Some(env.dbus_session_bus_address.clone()).filter(|a| !a.is_empty()),
        None => inherited_var("DBUS_SESSION_BUS_ADDRESS"),
    }
}

/// `XDG_CURRENT_DESKTOP`, on the same terms. A stored session that is not live names none, so
/// a stale desktop name cannot route into a dead session.
pub fn current_desktop() -> Option<String> {
    match get() {
        Some(env) => env.xdg_current_desktop.clone(),
        None => inherited_var("XDG_CURRENT_DESKTOP"),
    }
}

/// The stored session's PipeWire socket, for libpipewire's `remote.name`. `None` while nothing
/// is stored, or when the operator points `PIPEWIRE_REMOTE` / `PIPEWIRE_RUNTIME_DIR` elsewhere:
/// libpipewire then resolves the socket from the environment.
pub fn pipewire_remote() -> Option<String> {
    if inherited_var("PIPEWIRE_REMOTE").is_some() || inherited_var("PIPEWIRE_RUNTIME_DIR").is_some()
    {
        return None;
    }
    let env = get()?;
    (!env.xdg_runtime_dir.is_empty()).then(|| format!("{}/pipewire-0", env.xdg_runtime_dir))
}

/// The live compositor's socket, connected. `None` while nothing is stored: the caller then
/// connects from the environment. A stored session with no socket is `NotFound`.
#[cfg(unix)]
pub fn wayland_stream() -> Option<std::io::Result<std::os::unix::net::UnixStream>> {
    let env = get()?;
    Some(match env.wayland_socket() {
        Some(path) => std::os::unix::net::UnixStream::connect(path),
        None => Err(std::io::ErrorKind::NotFound.into()),
    })
}

/// Hand `cmd` the stored session: `WAYLAND_DISPLAY` and `XDG_CURRENT_DESKTOP` set or unset,
/// `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` set. A variable `cmd` already sets or
/// removes stays as its caller decided, so a spawn helper may call this for every child.
/// Nothing stored: the child inherits the process environment, which is then the same thing.
pub fn export_to(cmd: &mut std::process::Command) -> &mut std::process::Command {
    let Some(env) = get() else {
        return cmd;
    };
    let decided: Vec<std::ffi::OsString> = cmd.get_envs().map(|(k, _)| k.to_owned()).collect();
    let vars = [
        ("WAYLAND_DISPLAY", env.wayland_display.as_deref()),
        ("XDG_CURRENT_DESKTOP", env.xdg_current_desktop.as_deref()),
        ("XDG_RUNTIME_DIR", Some(env.xdg_runtime_dir.as_str())),
        (
            "DBUS_SESSION_BUS_ADDRESS",
            Some(env.dbus_session_bus_address.as_str()),
        ),
    ];
    for (name, value) in vars {
        if decided.iter().any(|k| k == name) {
            continue;
        }
        match value {
            Some("") => {}
            Some(value) => {
                cmd.env(name, value);
            }
            None => {
                cmd.env_remove(name);
            }
        }
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: Option<&str>) -> SessionEnv {
        SessionEnv {
            wayland_display: name.map(str::to_string),
            xdg_runtime_dir: "/run/user/1000".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_socketless_live_session_keeps_the_last_socket() {
        let last = Some("wayland-0".to_string());
        let kept = carry(named(None), true, last.clone());
        assert_eq!(kept.wayland_display.as_deref(), Some("wayland-0"));
        let replaced = carry(named(Some("wayland-1")), true, last.clone());
        assert_eq!(replaced.wayland_display.as_deref(), Some("wayland-1"));
        assert_eq!(carry(named(None), false, last).wayland_display, None);
    }

    #[test]
    fn the_socket_is_the_name_under_the_runtime_dir_unless_absolute() {
        let socket = |name| named(name).wayland_socket();
        assert_eq!(
            socket(Some("wayland-0")),
            Some("/run/user/1000/wayland-0".into())
        );
        assert_eq!(
            socket(Some("/tmp/kwin.sock")),
            Some("/tmp/kwin.sock".into())
        );
        assert_eq!(socket(None), None);
    }
}
