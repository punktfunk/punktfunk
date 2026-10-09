//! Blocking sleep / reboot / shutdown for `power.*` (`design/host-actions.md`),
//! the host's own restart for `host.restart`, and the per-verb probe the
//! discovery route reports.
//!
//! Linux drives logind over zbus, authorized for group `punktfunk` by
//! `packaging/linux/49-punktfunk-power.rules`. Calls omit `-ignore-inhibit`
//! and `-multiple-sessions`: a foreign inhibitor or a second local user makes
//! logind refuse, and that is a `409 blocked`.
//! Windows uses the interactive token's `SeShutdownPrivilege`. Other platforms
//! have no executor; the mgmt route answers `501`.
//!
//! [`probe`] and [`act`] block on a D-Bus round trip or a Win32 call. Call them
//! from `spawn_blocking`. The dedicated thread matches
//! [`crate::sleep_inhibit::acquire`]: zbus's blocking API cannot run on a tokio
//! worker.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerVerb {
    Sleep,
    Reboot,
    Shutdown,
    /// Restart the Punktfunk host process, not the machine.
    Restart,
}

/// Exit status that asks the Windows service supervisor for an immediate relaunch.
#[cfg_attr(not(windows), allow(dead_code, reason = "Windows service reads it"))]
pub const RESTART_EXIT_CODE: u32 = 75;

/// The `punktfunk-host*.service` user unit a `/proc/self/cgroup` places this process in.
/// `None` outside one: a terminal, a system unit, or another app's scope.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn host_unit(cgroup: &str) -> Option<String> {
    cgroup.lines().find_map(|line| {
        let path = line.rsplit(':').next()?;
        let unit = path.rsplit('/').next()?;
        (path.contains("/user@")
            && unit.starts_with("punktfunk-host")
            && unit.ends_with(".service"))
        .then(|| unit.to_string())
    })
}

#[cfg(target_os = "linux")]
fn restart_probe() -> Availability {
    match std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .as_deref()
        .and_then(host_unit)
    {
        Some(_) => Availability::yes(),
        None => Availability::no(
            "the host is not running as its user service, so nothing would start it again",
        ),
    }
}

#[cfg(target_os = "linux")]
fn restart_act() -> Result<(), String> {
    let unit = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .as_deref()
        .and_then(host_unit)
        .ok_or("the host is not running as its user service")?;
    // `--no-block`: this process is the one systemd stops.
    let status = std::process::Command::new("systemctl")
        .args(["--user", "--no-block", "restart", &unit])
        .status()
        .map_err(|e| format!("systemctl: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("systemctl restart {unit}: {status}"))
    }
}

/// Discovery publishes `reason` as `unavailable_reason` when `available` is
/// false, so the client does not offer an action that would fail.
pub struct Availability {
    pub available: bool,
    pub reason: Option<String>,
}

impl Availability {
    #[cfg_attr(target_os = "macos", allow(dead_code, reason = "no power on macOS"))]
    fn yes() -> Availability {
        Availability {
            available: true,
            reason: None,
        }
    }

    fn no(reason: impl Into<String>) -> Availability {
        Availability {
            available: false,
            reason: Some(reason.into()),
        }
    }
}

/// What `verb` can do on this host. A seat host never does any of it: sleeping or restarting
/// would take every other seat down, so the box's own host is the one executor.
pub fn probe(verb: PowerVerb) -> Availability {
    if pf_paths::seat::is_seat_host() {
        return Availability::no("A seat can't sleep, restart or shut down the host.");
    }
    os_probe(verb)
}

/// The invoke route answers `501` when this is false.
pub fn supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "windows"))
}

/// One logind `Manager` call on a dedicated thread (zbus blocking cannot run on a
/// tokio worker). The D-Bus error text is the reason the caller surfaces.
#[cfg(target_os = "linux")]
fn logind_call<T, A>(method: &'static str, args: A) -> Result<T, String>
where
    T: for<'de> serde::Deserialize<'de> + ashpd::zbus::zvariant::Type + Send + 'static,
    A: serde::Serialize + ashpd::zbus::zvariant::DynamicType + Send + Sync + 'static,
{
    std::thread::spawn(move || -> Result<T, String> {
        use ashpd::zbus;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let conn = zbus::Connection::system()
                .await
                .map_err(|e| e.to_string())?;
            let reply = conn
                .call_method(
                    Some("org.freedesktop.login1"),
                    "/org/freedesktop/login1",
                    Some("org.freedesktop.login1.Manager"),
                    method,
                    &args,
                )
                .await
                .map_err(|e| e.to_string())?;
            reply.body().deserialize().map_err(|e| e.to_string())
        })
    })
    .join()
    .map_err(|_| "logind call thread panicked".to_string())?
}

/// Only logind `"yes"` is available. `"challenge"` means polkit would prompt
/// (host user not in group `punktfunk`, or a second local session).
#[cfg(target_os = "linux")]
fn os_probe(verb: PowerVerb) -> Availability {
    let method = match verb {
        PowerVerb::Sleep => "CanSuspend",
        PowerVerb::Reboot => "CanReboot",
        PowerVerb::Shutdown => "CanPowerOff",
        PowerVerb::Restart => return restart_probe(),
    };
    match logind_call::<String, ()>(method, ()) {
        Ok(ans) if ans == "yes" => Availability::yes(),
        Ok(ans) if ans == "challenge" => Availability::no(
            "the host would need interactive authorization — is the host user in group \
             'punktfunk' (and no second local user logged in)?",
        ),
        Ok(ans) if ans == "na" => Availability::no("this machine does not support it"),
        Ok(ans) => Availability::no(format!("logind answered {ans:?}")),
        Err(e) => Availability::no(format!("no logind: {e}")),
    }
}

/// `interactive = false`: a polkit challenge fails instead of prompting. The
/// caller must already have released our inhibitor; a foreign one still standing
/// makes logind refuse, and the error names the inhibitor.
#[cfg(target_os = "linux")]
pub fn act(verb: PowerVerb) -> Result<(), String> {
    let method = match verb {
        PowerVerb::Sleep => "Suspend",
        PowerVerb::Reboot => "Reboot",
        PowerVerb::Shutdown => "PowerOff",
        PowerVerb::Restart => return restart_act(),
    };
    logind_call::<(), (bool,)>(method, (false,))
}

/// Reboot/shutdown skip a capability check: the interactive token holds
/// `SeShutdownPrivilege` by default, and [`act`] enables it.
#[cfg(target_os = "windows")]
fn os_probe(verb: PowerVerb) -> Availability {
    match verb {
        PowerVerb::Sleep => {
            // SAFETY: no arguments, no aliasing — a capability query.
            if unsafe { windows::Win32::System::Power::IsPwrSuspendAllowed() } {
                Availability::yes()
            } else {
                Availability::no("this machine does not support sleep")
            }
        }
        PowerVerb::Reboot | PowerVerb::Shutdown => Availability::yes(),
        PowerVerb::Restart if std::env::var_os("PUNKTFUNK_SERVICE_CHILD").is_some() => {
            Availability::yes()
        }
        PowerVerb::Restart => Availability::no(
            "the host is not running under the Punktfunk service, so nothing would start it again",
        ),
    }
}

/// `SeShutdownPrivilege` is present but disabled on an interactive token; a token
/// without it fails here, before the OS call. The reason string is what the system
/// event log records.
#[cfg(target_os = "windows")]
pub fn act(verb: PowerVerb) -> Result<(), String> {
    use windows::Win32::System::Power::SetSuspendState;
    use windows::Win32::System::Shutdown::{
        InitiateSystemShutdownExW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_OTHER,
        SHTDN_REASON_MINOR_OTHER,
    };

    if verb == PowerVerb::Restart {
        // The service supervisor relaunches on this code without a crash backoff.
        std::process::exit(RESTART_EXIT_CODE as i32);
    }

    pf_frame::privilege::enable("SeShutdownPrivilege")
        .map_err(|e| format!("enabling SeShutdownPrivilege: {e}"))?;

    match verb {
        PowerVerb::Sleep => {
            // SAFETY: SetSuspendState(hibernate, force, disable_wake) all false:
            // suspend, honor other apps' wake locks, do not force.
            if unsafe { SetSuspendState(false, false, false) } {
                Ok(())
            } else {
                Err(format!(
                    "SetSuspendState failed: {}",
                    windows::core::Error::from_thread()
                ))
            }
        }
        PowerVerb::Restart => unreachable!("handled above"),
        PowerVerb::Reboot | PowerVerb::Shutdown => {
            let reason = windows::core::HSTRING::from(
                "Requested from a Punktfunk client (host power action)",
            );
            // SAFETY: local machine (None); `reason` lives across the call.
            unsafe {
                InitiateSystemShutdownExW(
                    None,
                    &reason,
                    0,    // no countdown; sessions were already ended
                    true, // force-close apps; nobody is at the console
                    verb == PowerVerb::Reboot,
                    SHTDN_REASON_MAJOR_OTHER | SHTDN_REASON_MINOR_OTHER | SHTDN_REASON_FLAG_PLANNED,
                )
            }
            .map_err(|e| format!("InitiateSystemShutdownExW: {e}"))
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn os_probe(_verb: PowerVerb) -> Availability {
    Availability::no("not supported on this host platform")
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn act(_verb: PowerVerb) -> Result<(), String> {
    Err("not supported on this host platform".into())
}

/// Host-wide power-close flag. One-shot per action; [`set_closing`] resets it
/// after sleep so the woken host types later closes as ordinary.
static POWER_CLOSING: std::sync::OnceLock<tokio::sync::watch::Sender<bool>> =
    std::sync::OnceLock::new();

fn power_closing() -> &'static tokio::sync::watch::Sender<bool> {
    POWER_CLOSING.get_or_init(|| tokio::sync::watch::channel(false).0)
}

/// One receiver per session lifecycle task; a true flag closes with
/// `RejectReason::HostPower`.
pub fn closing_rx() -> tokio::sync::watch::Receiver<bool> {
    power_closing().subscribe()
}

pub fn set_closing(closing: bool) {
    let _ = power_closing().send(closing);
}

#[cfg(test)]
mod tests {
    use super::host_unit;

    #[test]
    fn restart_needs_the_host_user_unit() {
        let unit =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/punktfunk-host.service\n";
        assert_eq!(host_unit(unit).as_deref(), Some("punktfunk-host.service"));
        // A terminal, a system unit, or a session unit that merely starts the host.
        for other in [
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-foot.scope",
            "0::/system.slice/punktfunk-host.service",
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/punktfunk-kde-session.service",
            "",
        ] {
            assert_eq!(host_unit(other), None, "{other}");
        }
    }
}
