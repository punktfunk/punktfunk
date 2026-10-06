//! The Linux seat backend: a user, a logind session and a systemd unit per seat.
//!
//! The root daemon `punktfunk-seats` runs [`LinuxBackend`] behind [`socket`]. A seat is a
//! system-range user with its home under the box directory. Starting it writes the seat contract
//! and a fresh management token, then starts `punktfunk-seat@<user>.service`: that unit logs the
//! user in through PAM and runs `seat-session`, which brings up a headless compositor and a stock
//! `punktfunk-host serve`. Everything a seat runs descends from the unit, so the private
//! `compatdata` bind mounts reach it. The daemon holds no seat state of its own: a restart reads
//! each unit's state back from systemd.

mod accounts;
mod session;
mod shared;
pub mod socket;

pub use shared::ledger_root;
pub use socket::SOCKET_PATH;

use crate::backend::{BackendError, PlatformBackend};
use crate::ipc::{Diagnostic, DiagnosticLevel, SeatingStatus};
use crate::model::{Ledger, RuntimeState, RuntimeStatus, Seat, SeatId};
use crate::persistence::StoreError;
use crate::service::SeatService;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// A seat has this long to bring up its compositor and open the host's management port.
const START_TIMEOUT: Duration = Duration::from_secs(90);
/// How often a running seat's trust copy is compared with the box's.
const TRUST_TICK: Duration = Duration::from_secs(2);
/// How often the games folder's ACL mask is repaired while a seat runs.
const ACL_EVERY: Duration = Duration::from_secs(600);

fn err(code: &str, message: impl Into<String>) -> BackendError {
    BackendError::new(code, message)
}

/// Runs `program`, returning its stdout. A failure names the program, its first argument and its
/// stderr under `code`.
fn run(code: &str, program: &str, args: &[&str]) -> Result<String, BackendError> {
    let output = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| err(code, format!("run {program}: {error}")))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let cause = match stderr.trim() {
        "" => output.status.to_string(),
        text => text.to_owned(),
    };
    Err(err(
        code,
        format!("{program} {}: {cause}", args.first().unwrap_or(&"")),
    ))
}

pub struct LinuxBackend {
    inner: Arc<Inner>,
}

struct Inner {
    box_dir: PathBuf,
    /// The Steam to clone into a new seat; `None` looks for the owner's at provision time.
    steam_source: Option<PathBuf>,
    /// Seats whose unit is up, with the group their trust copy belongs to.
    running: Mutex<HashMap<SeatId, u32>>,
    /// A seat stopped since the ACL mask was last repaired.
    acl_dirty: AtomicBool,
}

impl LinuxBackend {
    /// `box_dir` is the box host's config directory, `/var/lib/punktfunk`. Creates the directories
    /// seats live in and starts the thread that keeps trust copies and the games ACL current.
    pub fn open(box_dir: PathBuf, steam_source: Option<PathBuf>) -> Result<Self, BackendError> {
        shared::ensure_layout(&box_dir)
            .map_err(|e| err("box_dir", format!("prepare {}: {e}", box_dir.display())))?;
        std::fs::create_dir_all(session::ENV_DIR)
            .map_err(|e| err("run_dir", format!("create {}: {e}", session::ENV_DIR)))?;
        let inner = Arc::new(Inner {
            box_dir,
            steam_source,
            running: Mutex::new(HashMap::new()),
            acl_dirty: AtomicBool::new(false),
        });
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("seats-level".into())
            .spawn(move || keep_level(weak))
            .map_err(|e| err("thread", format!("start the trust thread: {e}")))?;
        Ok(Self { inner })
    }

    /// Opens the ledger in `seats` under the box directory and gives the service this backend.
    /// The ledger store closes its directory to root; seat users walk through it, so it reopens.
    pub fn open_service(self) -> Result<SeatService<Self>, StoreError> {
        let root = ledger_root(&self.inner.box_dir);
        let service = SeatService::open(&root, self)?;
        shared::make_dir(&root, 0o711)?;
        Ok(service)
    }

    fn track(&self, id: &SeatId, gid: u32) {
        self.inner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), gid);
    }

    fn untrack(&self, id: &SeatId) {
        self.inner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    fn steam_source(&self) -> Option<PathBuf> {
        self.inner.steam_source.clone().or_else(|| {
            let users = accounts::all().ok()?;
            shared::find_steam_source(&users)
        })
    }

    /// Stops the seat's unit if it is up. A unit that was never started is fine.
    fn stop_unit(&self, seat: &Seat) -> Result<(), BackendError> {
        let unit = session::unit(&seat.account);
        if session::state(&seat.account)?.active != "inactive" {
            run("systemctl_failed", "systemctl", &["stop", &unit])?;
        }
        // A failed unit keeps its state until reset, and would read as failed after a stop.
        let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
        self.untrack(&seat.id);
        self.inner.acl_dirty.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Waits for the unit to be active and the host's management port to answer. A unit that
    /// ends first, or a timeout, stops the seat and fails the start.
    fn wait_ready(&self, seat: &Seat) -> Result<(), BackendError> {
        let deadline = Instant::now() + START_TIMEOUT;
        let failure = loop {
            let state = session::state(&seat.account)?;
            match state.active.as_str() {
                "active" if session::port_open(seat.mgmt_port) => return Ok(()),
                "failed" | "inactive" => {
                    break err(
                        "seat_exited",
                        format!(
                            "seat unit ended ({}); journalctl -u {} says why",
                            state.result,
                            session::unit(&seat.account)
                        ),
                    );
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                break err(
                    "start_timeout",
                    "seat did not open its management port within 90 seconds",
                );
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        let _ = self.stop_unit(seat);
        Err(failure)
    }

    fn remove_files(&self, seat: &Seat) -> Result<(), BackendError> {
        accounts::delete(seat)?;
        shared::remove_seat_dirs(&self.inner.box_dir, &seat.id)?;
        let _ = std::fs::remove_file(session::env_path(&seat.account));
        let dropin = session::dropin_dir(&seat.account);
        if dropin.exists() {
            let _ = std::fs::remove_dir_all(dropin);
            session::daemon_reload()?;
        }
        Ok(())
    }

    fn prerequisites(&self) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        let mut check = |ok: bool, code: &str, good: &str, bad: &str| {
            out.push(if ok {
                Diagnostic::info(code, good)
            } else {
                Diagnostic::error(code, bad)
            });
        };
        check(
            Path::new("/run/systemd/system").is_dir(),
            "systemd",
            "systemd is the init system",
            "This machine doesn't run systemd, which seats need.",
        );
        let logind = std::process::Command::new("systemctl")
            .args(["is-active", "--quiet", "systemd-logind"])
            .status()
            .is_ok_and(|s| s.success());
        check(
            logind,
            "logind",
            "systemd-logind is running",
            "systemd-logind isn't running, so a seat can't get a session.",
        );
        let group = |name: &str| accounts::group_gid(name).ok().flatten().is_some();
        check(
            group("punktfunk"),
            "punktfunk_group",
            "the punktfunk group exists",
            "The punktfunk group doesn't exist; reinstall the package that creates it.",
        );
        check(
            group("render"),
            "render_group",
            "the render group exists",
            "The render group doesn't exist, so a seat can't use the graphics card.",
        );
        let render_node = std::fs::read_dir("/dev/dri").is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        });
        check(
            render_node,
            "render_node",
            "a GPU render node is present",
            "No graphics card render node was found, so seats can't stream.",
        );
        check(
            which("kwin_wayland").is_some() || which("gamescope").is_some(),
            "compositor",
            "a seat compositor is installed",
            "Neither kwin_wayland nor gamescope is installed.",
        );
        let missing: Vec<&str> = [
            "useradd",
            "userdel",
            "setfacl",
            "loginctl",
            "systemctl",
            "cp",
        ]
        .into_iter()
        .filter(|tool| which(tool).is_none())
        .collect();
        check(
            missing.is_empty(),
            "tools",
            "the account and ACL tools are installed",
            &format!("These tools are missing: {}.", missing.join(", ")),
        );
        let installed = [
            "/usr/lib/systemd/system/punktfunk-seat@.service",
            "/etc/systemd/system/punktfunk-seat@.service",
        ]
        .iter()
        .any(|p| Path::new(p).is_file())
            && Path::new("/usr/libexec/punktfunk/seat-session").is_file()
            && Path::new("/usr/libexec/punktfunk/seat-reap").is_file();
        check(
            installed,
            "seat_unit",
            "the seat unit and its scripts are installed",
            "The seat unit or its scripts are missing; reinstall the host package.",
        );
        let box_dir = &self.inner.box_dir;
        check(
            box_dir.join("native-cert.pem").is_file() && box_dir.join("native-key.pem").is_file(),
            "box_identity",
            "the box has a host identity",
            "The box has no host identity yet; start its host once.",
        );
        out
    }
}

impl PlatformBackend for LinuxBackend {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError> {
        accounts::check_name(&seat.account)?;
        let box_dir = &self.inner.box_dir;
        let gid = accounts::group_gid("punktfunk")?.ok_or_else(|| {
            err(
                "group_missing",
                "the punktfunk group doesn't exist; the package creates it",
            )
        })?;
        shared::ensure_games(box_dir, gid)?;
        let (passwd, created) = accounts::ensure(seat, box_dir)?;
        let finished = shared::ensure_seat_dirs(box_dir, &seat.id, &passwd);
        if let Err(error) = finished {
            if created {
                let _ = self.remove_files(seat);
            }
            return Err(error);
        }
        shared::provision_steam(box_dir, &passwd, self.steam_source().as_deref());
        Ok(())
    }

    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        let box_dir = &self.inner.box_dir;
        let passwd = accounts::require(seat)?;
        shared::ensure_seat_dirs(box_dir, &seat.id, &passwd)?;
        shared::refresh_trust(box_dir, &seat.id, passwd.gid)?;
        let unit = session::unit(&seat.account);
        // A unit already up keeps its token: the host read it when it started.
        if !session::state(&seat.account)?.live() {
            if session::port_open(seat.mgmt_port) {
                return Err(err(
                    "port_in_use",
                    format!("management port {} already answers", seat.mgmt_port),
                ));
            }
            let token = session::mint_token();
            let door = accounts::lookup("punktfunk")?;
            shared::write_host_token(
                box_dir,
                &seat.id,
                &session::token_line(&token),
                shared::token_owner(door.as_ref()),
            )?;
            session::write_unit_files(seat, &passwd.home, box_dir, &token)?;
            let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
            run("systemctl_failed", "systemctl", &["start", &unit])?;
        }
        self.track(&seat.id, passwd.gid);
        self.wait_ready(seat)?;
        Ok(RuntimeStatus::running())
    }

    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.stop_unit(seat)?;
        Ok(RuntimeStatus::stopped())
    }

    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        self.stop_unit(seat)?;
        self.remove_files(seat)
    }

    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        let state = session::state(&seat.account)?;
        if state.live() {
            let known = self
                .inner
                .running
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&seat.id);
            if !known && let Some(passwd) = accounts::lookup(&seat.account)? {
                self.track(&seat.id, passwd.gid);
            }
        } else {
            self.untrack(&seat.id);
        }
        Ok(state.runtime())
    }

    fn doctor(&self, ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        let mut out = self.prerequisites();
        for seat in &ledger.seats {
            let owned = accounts::lookup(&seat.account)?
                .is_some_and(|found| accounts::owned_by(&found, &seat.id));
            out.push(Diagnostic {
                level: if owned {
                    DiagnosticLevel::Info
                } else {
                    DiagnosticLevel::Error
                },
                code: "account_marker".into(),
                message: if owned {
                    "account marker matches the exact seat ID".into()
                } else {
                    "account is absent or its marker does not match".into()
                },
                seat_id: Some(seat.id.clone()),
            });
            let state = session::state(&seat.account)?;
            out.push(Diagnostic {
                level: if state.runtime().state == RuntimeState::Failed {
                    DiagnosticLevel::Error
                } else {
                    DiagnosticLevel::Info
                },
                code: "unit_state".into(),
                message: format!(
                    "unit is {} ({}), last result {}",
                    state.active, state.sub, state.result
                ),
                seat_id: Some(seat.id.clone()),
            });
        }
        Ok(out)
    }

    fn seating(&self) -> Result<SeatingStatus, BackendError> {
        Ok(SeatingStatus {
            enabled: true,
            checks: self.prerequisites(),
        })
    }

    /// Seats are on whenever the daemon runs.
    fn enable(&self, _allow_rdp_from_network: bool) -> Result<SeatingStatus, BackendError> {
        self.seating()
    }

    fn disable(&self, _keep_accounts: bool) -> Result<SeatingStatus, BackendError> {
        Err(err(
            "not_supported",
            "seats stay on while the supervisor runs; stop its service to turn them off",
        ))
    }
}

/// While a seat runs: refreshes its trust copy when the box's files change, and repairs the games
/// folder's ACL mask after a seat stops and every [`ACL_EVERY`]. Ends when the backend is dropped.
fn keep_level(inner: Weak<Inner>) {
    let mut acl_at = Instant::now();
    let mut failing: HashSet<SeatId> = HashSet::new();
    loop {
        std::thread::sleep(TRUST_TICK);
        let Some(inner) = inner.upgrade() else { return };
        let seats: Vec<(SeatId, u32)> = inner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, gid)| (id.clone(), *gid))
            .collect();
        for (id, gid) in &seats {
            match shared::refresh_trust(&inner.box_dir, id, *gid) {
                Ok(_) => {
                    failing.remove(id);
                }
                // Logged once per outage, not every tick.
                Err(error) if failing.insert(id.clone()) => {
                    tracing::warn!(seat = %id, %error, "trust copy not refreshed");
                }
                Err(_) => {}
            }
        }
        let due = !seats.is_empty() && acl_at.elapsed() >= ACL_EVERY;
        if inner.acl_dirty.swap(false, Ordering::SeqCst) || due {
            acl_at = Instant::now();
            if let Err(error) = shared::fix_acl(&inner.box_dir) {
                tracing::warn!(%error, "games ACL mask not repaired");
            }
        }
    }
}

/// `name` on `PATH`, or in the system directories a service's `PATH` may omit.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/usr/bin", "/sbin", "/bin"].map(PathBuf::from))
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tool_is_found_on_the_path_and_a_missing_one_is_not() {
        assert!(which("sh").is_some());
        assert!(which("no-such-tool-punktfunk").is_none());
    }

    #[test]
    fn a_failing_command_names_itself_and_its_stderr() {
        let error = run("x_failed", "sh", &["-c", "echo boom >&2; exit 3"]).unwrap_err();
        assert_eq!(error.code, "x_failed");
        assert_eq!(error.message, "sh -c: boom");
        let missing = run("x_failed", "no-such-tool-punktfunk", &[]).unwrap_err();
        assert!(missing.message.starts_with("run no-such-tool-punktfunk"));
        assert_eq!(run("x", "echo", &["hi"]).unwrap(), "hi\n");
    }
}
