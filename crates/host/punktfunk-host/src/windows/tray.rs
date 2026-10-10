//! Tray lifecycle: find, start, stop, check, and supervise `punktfunk-tray.exe`.
//! Shared by the `tray` CLI and by [`supervise`], which the host service runs
//! for its whole lifetime.
//!
//! The tray is a per-user, per-session GUI with no recovery of its own. HKLM
//! `Run` fires at sign-in only; [`supervise`] restarts a tray that dies after.
//!
//! [`start`] tries the user-token path first, then a plain spawn:
//!
//! * **From a streaming host (SYSTEM)** — launch as the signed-in user of that
//!   host's WTS session via
//!   [`crate::interactive::spawn_as_current_session_user`] (`WTSQueryUserToken`
//!   + `CreateProcessAsUserW`). Only SYSTEM holds the required `SE_TCB`.
//! * **From an interactive shell** — the caller already has the right user and
//!   session token, so `WTSQueryUserToken` fails and a plain spawn is correct.
//!
//! Only that second caller may fall back. A tray spawned under a SYSTEM token owns
//! the session's mutex with no icon a user can reach, so SYSTEM waits for a
//! signed-in user instead.
//!
//! Seat hosts do not supervise a tray: the seat manager is their control surface.
//! An explicit seat-local start still uses that session's user and mutex.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;

pub const TRAY_EXE: &str = "punktfunk-tray.exe";

pub fn main(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("start") => {
            let (pid, how) = start()?;
            match pid {
                Some(pid) => println!("status tray started (pid {pid}, {how})"),
                None => println!("status tray is already running"),
            }
            Ok(())
        }
        Some("stop") => {
            if stop() {
                println!("status tray stopped");
            } else {
                println!("no status tray was running");
            }
            Ok(())
        }
        Some("status") => {
            let path = tray_exe();
            println!(
                "status tray: {}",
                match (&path, is_running()) {
                    (None, _) => "not installed".to_string(),
                    (Some(_), true) => "running".to_string(),
                    (Some(_), false) => "not running".to_string(),
                }
            );
            if let Some(p) = path {
                println!("executable:  {}", p.display());
            }
            Ok(())
        }
        _ => bail!("usage: punktfunk-host tray <start|stop|status>"),
    }
}

/// `punktfunk-tray.exe` next to this executable. The `trayicon` task is optional, so absence is not an error.
pub fn tray_exe() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(TRAY_EXE)))
        .filter(|p| p.exists())
}

/// THIS session: the tray holds a `Local\` mutex, which resolves per logon session — a process
/// scan would see another user's tray and suppress the console one. Best-effort: a failed open
/// reads as not running — a hint, never proof for a kill.
pub fn is_running() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_ACCESS_RIGHTS};
    // SYNCHRONIZE: the least access an open needs; existence is all that is asked.
    const SYNCHRONIZE: SYNCHRONIZATION_ACCESS_RIGHTS = SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0000);
    // SAFETY: a static NUL-terminated name; the handle, when one comes back, is closed here.
    unsafe {
        match OpenMutexW(SYNCHRONIZE, false, w!("Local\\PunktfunkTray")) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// Two misses restart the tray, so this is half the grace window.
const WATCH_TICK: std::time::Duration = std::time::Duration::from_secs(30);

/// The `tray_autostart` setting, read each tick so a console change stops or resumes the watch.
fn wanted() -> bool {
    pf_host_config::row_bool("PUNKTFUNK_TRAY_AUTOSTART")
}

/// Keeps the console status tray alive while the ordinary host runs and Tray autostart is on.
///
/// Seat hosts return before spawning the watcher because their manager is the
/// control surface. On the console, two misses trigger [`ensure`]; one miss is
/// the tray's own Exit racing the service shutdown.
pub fn supervise() {
    if crate::seat::is_seat_host() {
        tracing::debug!("seat host: status-tray supervision disabled");
        return;
    }
    std::thread::spawn(|| {
        if wanted() {
            ensure();
        }
        let mut missed = false;
        loop {
            std::thread::sleep(WATCH_TICK);
            let absent = wanted() && !is_running();
            if absent && missed {
                ensure();
            }
            missed = absent;
        }
    });
}

/// Best-effort: a miss is usually "nobody signed in yet", so this stays at `debug`/`info`.
fn ensure() {
    match start() {
        Ok((Some(pid), how)) => tracing::info!(pid, how, "status tray started"),
        Ok((None, _)) => tracing::trace!("status tray is already running"),
        Err(e) => tracing::debug!(error = %e, "status tray not started"),
    }
}

/// Starts a tray for the caller's WTS session.
///
/// Console callers avoid a duplicate visible in the process snapshot. Seat
/// callers launch and let the tray's session-local mutex resolve duplicates;
/// a console tray must not suppress theirs. SYSTEM drops to the session user,
/// while an interactive caller keeps its token.
pub fn start() -> Result<(Option<u32>, &'static str)> {
    let Some(exe) = tray_exe() else {
        bail!("{TRAY_EXE} is not installed next to this executable");
    };
    if !crate::seat::is_seat_host() && is_running() {
        return Ok((None, "already running"));
    }
    // Quoting preserves an operator-chosen install path that contains spaces.
    let quoted = format!("\"{}\"", exe.display());
    let declined = match crate::interactive::spawn_as_current_session_user(&quoted, None) {
        Ok(pid) => return Ok((Some(pid), "as this session's user")),
        Err(e) => e,
    };
    // The fallback below runs the tray under THIS token, which is only ever right for an
    // interactive caller. As SYSTEM it plants a tray with no usable icon that holds the
    // session's `Local\PunktfunkTray` for good, and the user's own tray then has to fight it
    // for the name. Nothing to do but wait for a signed-in user; the watcher retries.
    if crate::hooks::running_as_system() {
        return Err(declined).context("no signed-in user in this session yet");
    }
    tracing::debug!(error = %format!("{declined:#}"), "status tray: user-token spawn declined — falling back to this token");
    // WTSQueryUserToken is privileged; an interactive caller's plain spawn preserves its seat.
    let child = std::process::Command::new(&exe)
        .spawn()
        .with_context(|| format!("spawn {}", exe.display()))?;
    Ok((Some(child.id()), "in this session"))
}

/// Stops the tray in this session; the console path can reap stale peers.
///
/// `--quit` posts `WM_CLOSE` through the session-local tray mutex so the icon is
/// removed cleanly. A seat host never uses global `taskkill`, which would kill
/// other seats. Console supervision restarts its tray while the HKLM opt-in
/// remains present.
pub fn stop() -> bool {
    let was_running = is_running();
    if !was_running && !crate::seat::is_seat_host() {
        return false;
    }
    if let Some(exe) = tray_exe() {
        if let Ok(mut child) = std::process::Command::new(&exe).arg("--quit").spawn() {
            let _ = child.wait();
        }
        if crate::seat::is_seat_host() {
            return true;
        }
        for _ in 0..8 {
            if !is_running() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    let _ = std::process::Command::new(crate::install::resolve_tool("taskkill"))
        .args(["/F", "/IM", TRAY_EXE])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    true
}
