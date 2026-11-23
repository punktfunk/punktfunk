//! Blocking sleep / reboot / shutdown for `power.*` (`design/host-actions.md`),
//! the host's own restart for `host.restart`, and the per-verb probe the
//! discovery route reports.
//!
//! Linux drives logind over zbus, authorized for group `punktfunk` by
//! `packaging/linux/49-punktfunk-power.rules`. A foreign inhibitor or a second
//! local user makes logind refuse, and that is a `409 blocked`. The door has no
//! session, so `49-punktfunk-door.rules` grants it the second-user variants.
//! Windows uses the interactive token's `SeShutdownPrivilege`. macOS sleeps through IOKit and
//! has no unprivileged public reboot or shutdown; its host restarts through the login agent or
//! by reopening the app. Other platforms have no executor; the mgmt route answers `501`.
//!
//! [`probe`] and [`act`] block on a D-Bus round trip or a Win32 call. Call them
//! from `spawn_blocking`. The dedicated thread matches
//! [`crate::sleep_inhibit::acquire`]: a nested `block_on` panics on a tokio worker.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerVerb {
    Sleep,
    Reboot,
    Shutdown,
    /// Restart the Punktfunk host process, not the machine.
    Restart,
}

/// Exit status that asks the supervisor for an immediate relaunch: the Windows service, or the
/// Mac login agent, which restarts on any failed exit. No other exit of the host uses it.
#[cfg_attr(
    not(any(windows, target_os = "macos")),
    allow(dead_code, reason = "the Windows service and the Mac host read it")
)]
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

/// The door's unit restarts it (`Restart=always`), and a seat's runner starts its host over
/// whenever it exits: both restart by stopping.
#[cfg(target_os = "linux")]
fn restarts_by_stopping() -> bool {
    let role = pf_paths::seat::role();
    role.is_door() || role.is_seat_host()
}

#[cfg(target_os = "linux")]
fn restart_probe() -> Availability {
    if restarts_by_stopping() {
        return Availability::yes();
    }
    match std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .as_deref()
        .and_then(host_unit)
    {
        Some(_) => Availability::yes(),
        None => Availability::refused(
            "host_restart_no_user_service",
            "The host is not running as its user service, so nothing would start it again",
        ),
    }
}

#[cfg(target_os = "linux")]
fn restart_act() -> Result<(), String> {
    if restarts_by_stopping() {
        // Its own stop signal: the handler gives the box back, then exits.
        return crate::native::stop_self();
    }
    let unit = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .as_deref()
        .and_then(host_unit)
        .ok_or("the host is not running as its user service")?;
    // `--no-block`: this process is the one systemd stops.
    let status =
        pf_host_config::session::export_to(std::process::Command::new("systemctl").args([
            "--user",
            "--no-block",
            "restart",
            &unit,
        ]))
        .status()
        .map_err(|e| format!("systemctl: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("systemctl restart {unit}: {status}"))
    }
}

/// Discovery publishes `reason` as `unavailable_reason` when `available` is
/// false, so the client does not offer an action that would fail. `code` names the reason's
/// host message `apierr_<code>`; a reason without one is the system's own words.
pub struct Availability {
    pub available: bool,
    pub reason: Option<String>,
    pub code: Option<&'static str>,
}

impl Availability {
    pub(crate) fn yes() -> Availability {
        Availability {
            available: true,
            reason: None,
            code: None,
        }
    }

    fn no(reason: impl Into<String>) -> Availability {
        Availability {
            available: false,
            reason: Some(reason.into()),
            code: None,
        }
    }

    /// Unavailable for the sentence `reason`, the English of `apierr_<code>`.
    pub(crate) fn refused(code: &'static str, reason: &str) -> Availability {
        Availability {
            code: Some(code),
            ..Availability::no(reason)
        }
    }
}

/// What `verb` can do on this host. A seat host sleeps, reboots and shuts down nothing: that
/// would take every other seat down, so the box's own host is the one executor. It may restart
/// its own process.
pub fn probe(verb: PowerVerb) -> Availability {
    if pf_paths::seat::role().is_seat_host() && verb != PowerVerb::Restart {
        return Availability::refused(
            "host_power_seat",
            "A seat can't sleep or shut down the host.",
        );
    }
    os_probe(verb)
}

/// The invoke route answers `501` when this is false.
pub fn supported() -> bool {
    cfg!(any(
        target_os = "linux",
        target_os = "windows",
        target_os = "macos"
    ))
}

/// One logind `Manager` call on a dedicated thread (a nested `block_on` panics on a
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
        Ok(ans) if ans == "challenge" => Availability::refused(
            "host_power_needs_authorization",
            "The host would need interactive authorization — is the host user in group \
             'punktfunk' (and no second local user logged in)?",
        ),
        Ok(ans) if ans == "na" => Availability::refused(
            "host_power_not_supported_here",
            "This machine does not support it",
        ),
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
                Availability::refused(
                    "host_power_sleep_not_supported",
                    "This machine does not support sleep",
                )
            }
        }
        PowerVerb::Reboot | PowerVerb::Shutdown => Availability::yes(),
        PowerVerb::Restart if std::env::var_os("PUNKTFUNK_SERVICE_CHILD").is_some() => {
            Availability::yes()
        }
        PowerVerb::Restart => Availability::refused(
            "host_restart_no_service",
            "The host is not running under the Punktfunk service, so nothing would start it again",
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

/// How a Mac host comes back from `host.restart`.
#[cfg(target_os = "macos")]
#[derive(Debug, PartialEq)]
enum MacRestart {
    /// The login agent starts it again after a [`RESTART_EXIT_CODE`] exit.
    Agent,
    /// A relauncher opens this app again once the process is gone.
    Reopen(std::path::PathBuf),
    /// Nothing would start it again.
    Refused,
}

#[cfg(target_os = "macos")]
fn mac_restart(agent: bool, bundle: Option<&std::path::Path>) -> MacRestart {
    match (agent, bundle) {
        (true, _) => MacRestart::Agent,
        (false, Some(app)) => MacRestart::Reopen(app.to_path_buf()),
        (false, None) => MacRestart::Refused,
    }
}

#[cfg(target_os = "macos")]
fn mac_restart_here() -> MacRestart {
    let exe = std::env::current_exe().unwrap_or_default();
    let bundle = crate::hostsys::app_loop::bundle_of(&exe).and_then(|b| b.canonicalize().ok());
    mac_restart(crate::hostsys::app_loop::login_agent(), bundle.as_deref())
}

/// `sh` waits for `pid` to exit, then runs `opener -n` on `app` with `env` forwarded, so the
/// app comes back on the same config folder and ports. Its own process group outlives ours.
#[cfg(target_os = "macos")]
fn reopener(
    pid: u32,
    app: &std::path::Path,
    env: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    opener: &str,
) -> std::process::Command {
    use std::os::unix::process::CommandExt;
    let mut sh = std::process::Command::new("/bin/sh");
    sh.arg("-c")
        .arg("pid=$1; shift; while kill -0 \"$pid\" 2>/dev/null; do sleep 0.2; done; exec \"$@\"")
        .args(["sh", &pid.to_string(), opener, "-n"]);
    for (key, value) in env {
        let mut pair = key;
        pair.push("=");
        pair.push(value);
        sh.arg("--env").arg(pair);
    }
    sh.arg(app)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0);
    sh
}

/// Sleep, and restart when something will start the host again. The console user may sleep the
/// Mac, as `pmset sleepnow` does.
#[cfg(target_os = "macos")]
fn os_probe(verb: PowerVerb) -> Availability {
    match verb {
        PowerVerb::Sleep => Availability::yes(),
        PowerVerb::Restart if mac_restart_here() != MacRestart::Refused => Availability::yes(),
        PowerVerb::Restart => Availability::refused(
            "host_restart_no_user_service",
            "The host isn't running as its user service, so nothing would start it again",
        ),
        PowerVerb::Reboot | PowerVerb::Shutdown => unsupported(),
    }
}

/// Asks the power manager to sleep now. Our display-awake assertion is an idle one, which a
/// deliberate sleep overrides. A restart stops as the stop signal does, then comes back.
#[cfg(target_os = "macos")]
pub fn act(verb: PowerVerb) -> Result<(), String> {
    use objc2_io_kit::{kIOMainPortDefault, kIOReturnSuccess, IOPMFindPowerManagement};
    use objc2_io_kit::{IOPMSleepSystem, IOServiceClose};
    if verb == PowerVerb::Restart {
        return match mac_restart_here() {
            MacRestart::Agent => crate::native::stop_exiting(RESTART_EXIT_CODE as i32),
            MacRestart::Reopen(app) => {
                // What the operator set, not what `host.env` gave: the file is read again.
                let env = std::env::vars_os().filter(|(k, _)| {
                    k.to_string_lossy().starts_with("PUNKTFUNK_")
                        && !crate::hostsys::app_loop::from_host_env(k)
                });
                reopener(std::process::id(), &app, env, "/usr/bin/open")
                    .spawn()
                    .map_err(|e| format!("start the relauncher: {e}"))?;
                crate::native::stop_self()
            }
            MacRestart::Refused => Err("nothing would start the host again".into()),
        };
    }
    if verb != PowerVerb::Sleep {
        return Err("not supported on this host platform".into());
    }
    // SAFETY: `kIOMainPortDefault` is IOKit's constant default main port, read once.
    let port = IOPMFindPowerManagement(unsafe { kIOMainPortDefault });
    if port == 0 {
        return Err("IOPMFindPowerManagement: no power manager connection".into());
    }
    let rc = IOPMSleepSystem(port);
    IOServiceClose(port);
    if rc == kIOReturnSuccess {
        Ok(())
    } else {
        Err(format!("IOPMSleepSystem: {rc:#x}"))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn os_probe(_verb: PowerVerb) -> Availability {
    unsupported()
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn unsupported() -> Availability {
    Availability::refused(
        "host_power_unsupported",
        "Host power actions are not supported on this host platform",
    )
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
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

    #[cfg(target_os = "macos")]
    #[test]
    fn a_mac_host_restarts_through_the_agent_or_the_app() {
        use super::{mac_restart, MacRestart};
        let app = std::path::Path::new("/Applications/Punktfunk Host.app");
        assert_eq!(mac_restart(true, Some(app)), MacRestart::Agent);
        assert_eq!(mac_restart(true, None), MacRestart::Agent);
        assert_eq!(
            mac_restart(false, Some(app)),
            MacRestart::Reopen(app.into())
        );
        assert_eq!(mac_restart(false, None), MacRestart::Refused);
    }

    /// The app opens only once the old host is gone, with the host's environment.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_relauncher_opens_the_app_after_the_host_exits() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (opener, marker) = (dir.path().join("open"), dir.path().join("opened"));
        let script = format!("#!/bin/sh\necho \"$@\" > '{}'\n", marker.display());
        std::fs::write(&opener, script).unwrap();
        std::fs::set_permissions(&opener, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut host = std::process::Command::new("sleep")
            .arg("1")
            .spawn()
            .unwrap();
        let env = [("PUNKTFUNK_CONFIG_DIR".into(), "/tmp/pf a".into())].into_iter();
        let app = std::path::Path::new("/Apps/X.app");
        let mut relauncher = super::reopener(host.id(), app, env, opener.to_str().unwrap())
            .spawn()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(!marker.exists(), "opened while the host still ran");
        host.wait().unwrap();
        assert!(relauncher.wait().unwrap().success());
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "-n --env PUNKTFUNK_CONFIG_DIR=/tmp/pf a /Apps/X.app\n"
        );
    }

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
