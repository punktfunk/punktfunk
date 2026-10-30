//! The Linux seat backend: a user, a logind session and a systemd unit per seat.
//!
//! The root daemon `punktfunk-seats` runs [`LinuxBackend`] behind [`socket`]. A seat is a
//! system-range user with its home under the box directory. Starting it writes the seat contract
//! and a fresh management token, then starts `punktfunk-seat@<user>.service`: that unit logs the
//! user in through PAM and runs `seat-session`, which brings up a headless compositor and a stock
//! `punktfunk-host serve`. Everything a seat runs descends from the unit, so the private
//! `compatdata` bind mounts reach it. The daemon holds no seat state of its own: a restart reads
//! each unit's state back from systemd.
//!
//! The owner's row is the box owner's own account, adopted. Its unit runs the owner's host in a
//! headless `background` session, and never while the owner sits at the machine: a login of theirs
//! on a seat ends the unit, and the owner's own user host serves the row from then on. That
//! host writes the box's library: the daemon copies it for the door and every other seat.
//!
//! A seat's pads are the daemon's too: its host asks the pad broker in the `punktfunk-seats`
//! binary for them ([`LinuxBackend::running_account`] is its gate), and [`fence`] gives udev the
//! user each one belongs to. The binary's uinput proxy, the seat's `/dev/uinput`, comes and goes
//! with the unit ([`LinuxBackend::on_unit`]).

mod accounts;
pub mod fence;
mod libraries;
mod logind;
mod safe_open;
mod session;
mod shared;
pub mod socket;

pub use shared::ledger_root;
pub use socket::SOCKET_PATH;

use crate::backend::{BackendError, PlatformBackend};
use crate::ipc::{Diagnostic, DiagnosticLevel, SeatingStatus};
use crate::model::{Ledger, RuntimeState, RuntimeStatus, Seat, SeatId};
use crate::persistence::{LedgerStore, StoreError};
use crate::service::SeatService;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// The supervisor's own state: the ledger, every seat's home and trust copy, the shared games
/// folder. Root's alone; the box directory beside it is the door's, which reads its files only.
pub const STATE_DIR: &str = "/var/lib/punktfunk-seats";

/// The box owner `door-helper` recorded under [`STATE_DIR`]: the account the door is on for.
/// Readable by the door, written only by root.
pub fn recorded_owner() -> Option<String> {
    accounts::recorded_owner(Path::new(STATE_DIR))
}

/// What `door-helper` may switch the door for `account` to: `on` or off. `<uid> <home>` for the
/// account, `gone` for an owner switched off whose account no longer exists. The error's code
/// says which rule refused it.
pub fn check_owner(account: &str, on: bool) -> Result<String, BackendError> {
    Ok(match accounts::switchable(account, on)? {
        accounts::Switchable::Account(p) => format!("{} {}", p.uid, p.home.display()),
        accounts::Switchable::Gone => "gone".into(),
    })
}

/// A seat has this long to bring up its compositor and open the host's management port.
const START_TIMEOUT: Duration = Duration::from_secs(90);
/// How often a running seat's trust copy is compared with the box's.
const TRUST_TICK: Duration = Duration::from_secs(2);
/// How often the games folder's ACL mask is repaired while a seat runs.
const ACL_EVERY: Duration = Duration::from_secs(600);
/// How long `Seating` reuses the prerequisite checks it ran.
const CHECKS_FRESH: Duration = Duration::from_secs(10);

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
    /// The door's directory: the trust copies follow its files.
    box_dir: PathBuf,
    /// [`STATE_DIR`].
    state: PathBuf,
    /// The Steam `--steam-source` names; `None` follows the owner row ([`LinuxBackend::steam_source`]).
    steam_source: Option<PathBuf>,
    /// Seats whose unit is up.
    running: Mutex<HashMap<SeatId, Tracked>>,
    /// The box owner's account, once their row is known: whose home the box's library comes
    /// from and whose Steam a seat clones.
    owner: Mutex<Option<accounts::Passwd>>,
    /// The `punktfunk` group's gid, once looked up: the door reads the box's library through it.
    door_gid: OnceLock<u32>,
    /// A seat stopped since the ACL mask was last repaired.
    acl_dirty: AtomicBool,
    /// Stops asked of any seat so far. A start that sees it move gives up.
    stops: AtomicU64,
    /// What the binary runs beside a seat's unit ([`LinuxBackend::on_unit`]).
    hook: OnceLock<UnitHook>,
    /// The prerequisite checks `Seating` last answered with, and when they ran.
    checks: Mutex<Option<(Instant, Vec<Diagnostic>)>>,
    /// Held by a removal throughout and by [`keep_level`] for each seat it refreshes, so a
    /// refresh never writes a removed seat's trust copy or environment file back.
    level: Mutex<()>,
}

/// `slot`'s checks when they ran less than [`CHECKS_FRESH`] before `now`, else what `run`
/// answers, kept in `slot`.
fn cached(
    slot: &mut Option<(Instant, Vec<Diagnostic>)>,
    now: Instant,
    run: impl FnOnce() -> Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    if let Some((at, checks)) = slot.as_ref()
        && now.duration_since(*at) < CHECKS_FRESH
    {
        return checks.clone();
    }
    let checks = run();
    *slot = Some((now, checks.clone()));
    checks
}

/// Runs beside a seat's unit: `Some(uid)` before the unit starts and for a seat found running,
/// `None` once it stopped; neither for the owner's row. Every call must be idempotent.
pub type UnitHook = Box<dyn Fn(&str, Option<u32>) + Send + Sync>;

fn unit_hook(inner: &Inner, account: &str, uid: Option<u32>) {
    if let Some(hook) = inner.hook.get() {
        hook(account, uid);
    }
}

/// What the keep-level thread and the pad broker need of a running seat.
#[derive(Clone, Debug)]
struct Tracked {
    /// Who reads its trust copy.
    reader: shared::Reader,
    /// The owner's row and account, for the owner's row: its physical login ends the unit, and
    /// its environment file follows the box's Seat defaults.
    owner: Option<(Seat, accounts::Passwd)>,
    /// The seat's user: what the pad broker admits, and what its pads are stamped with.
    uid: u32,
    account: String,
}

impl Tracked {
    fn of(seat: &Seat, passwd: &accounts::Passwd) -> Self {
        let (reader, owner) = if seat.owner {
            (
                shared::Reader::person(passwd.uid, passwd.gid),
                Some((seat.clone(), passwd.clone())),
            )
        } else {
            (shared::Reader::group(passwd.gid), None)
        };
        Self {
            reader,
            owner,
            uid: passwd.uid,
            account: seat.account.clone(),
        }
    }
}

/// Stops `account`'s unit if it is up. A seat's user ends with it, so nothing it started under
/// its own manager survives, and then the hook takes back what runs beside it. The owner's row
/// keeps both: its account is a person's, whose own login may hold pads. A unit that was never
/// started is fine.
fn stop_unit(inner: &Inner, id: &SeatId, account: &str, owner: bool) -> Result<(), BackendError> {
    let unit = session::unit(account);
    if session::state(account)?.active != "inactive" {
        run("systemctl_failed", "systemctl", &["stop", &unit])?;
    }
    // A failed unit keeps its state until reset, and would read as failed after a stop.
    let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
    inner.acl_dirty.store(true, Ordering::SeqCst);
    // The owner's row stays tracked: its own host reads the trust copy while the unit is down.
    if owner {
        return Ok(());
    }
    // Untracked first, so the pad broker refuses the user while it ends.
    inner
        .running
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
    // Only the seat's own user is ended; a stranger of the same name keeps its sessions.
    let ended = accounts::refuse_foreign(account, id).and_then(|()| accounts::quiesce(account));
    unit_hook(inner, account, None);
    ended
}

/// Runs `systemctl --user <verb>` on the owner's own `punktfunk-host`. The owner's user units
/// stand down while the row's unit runs, so `start` is what brings them back: the host, and the
/// headless KDE session when the owner enabled it. A user with no manager has no login to host,
/// and nothing runs. `setpriv` becomes the user without a PAM session.
fn user_host(account: &str, verb: &str) {
    const KDE_SESSION: &str = "punktfunk-kde-session.service";
    let Ok(Some(user)) = accounts::lookup(account) else {
        return;
    };
    let (uid, gid) = (user.uid.to_string(), user.gid.to_string());
    let runtime = format!("XDG_RUNTIME_DIR=/run/user/{uid}");
    let systemctl = |args: &[&str]| {
        let mut all = vec![
            "--reuid",
            uid.as_str(),
            "--regid",
            gid.as_str(),
            "--init-groups",
            "env",
            runtime.as_str(),
            "systemctl",
            "--user",
        ];
        all.extend_from_slice(args);
        run("systemctl_failed", "setpriv", &all)
    };
    let mut units = vec![verb, "punktfunk-host.service"];
    if verb == "start" && systemctl(&["is-enabled", "--quiet", KDE_SESSION]).is_ok() {
        units.push(KDE_SESSION);
    }
    if let Err(error) = systemctl(&units) {
        tracing::debug!(%account, verb, %error, "owner's own host not reached");
    }
}

/// The door's group, which reads the box's library. `None` while there is no such group.
fn door_reader(inner: &Inner) -> Option<shared::Reader> {
    let gid = match inner.door_gid.get() {
        Some(gid) => *gid,
        None => {
            let gid = accounts::group_gid(socket::DOOR_GROUP).ok().flatten()?;
            *inner.door_gid.get_or_init(|| gid)
        }
    };
    Some(shared::Reader::group(gid))
}

/// Brings the box's library level with the owner's, once the owner's row is known. Returns
/// whether anything changed.
fn refresh_box_library(inner: &Inner) -> Result<bool, BackendError> {
    let home = inner
        .owner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|owner| owner.home.clone());
    let Some(home) = home else { return Ok(false) };
    match door_reader(inner) {
        Some(reader) => shared::refresh_box_library(&home, &inner.state, reader),
        None => Ok(false),
    }
}

/// Whether the owner has a session at the machine. A `loginctl` that doesn't answer reads as no.
pub fn owner_at_desk(account: &str) -> bool {
    let own = logind::own_session(account);
    logind::sessions().is_ok_and(|all| logind::at_the_desk(&all, account, own.as_deref()))
}

impl LinuxBackend {
    /// `box_dir` is the door's directory, `/var/lib/punktfunk`; `state` is [`STATE_DIR`].
    /// Creates the directories seats live in and starts the thread that keeps trust copies and
    /// the games ACL current.
    pub fn open(
        box_dir: PathBuf,
        state: PathBuf,
        steam_source: Option<PathBuf>,
    ) -> Result<Self, BackendError> {
        shared::ensure_layout(&state)
            .map_err(|e| err("state_dir", format!("prepare {}: {e}", state.display())))?;
        std::fs::create_dir_all(session::ENV_DIR)
            .map_err(|e| err("run_dir", format!("create {}: {e}", session::ENV_DIR)))?;
        let inner = Arc::new(Inner {
            box_dir,
            state,
            steam_source,
            running: Mutex::new(HashMap::new()),
            owner: Mutex::new(None),
            door_gid: OnceLock::new(),
            acl_dirty: AtomicBool::new(false),
            stops: AtomicU64::new(0),
            hook: OnceLock::new(),
            checks: Mutex::new(None),
            level: Mutex::new(()),
        });
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("seats-level".into())
            .spawn(move || keep_level(weak))
            .map_err(|e| err("thread", format!("start the trust thread: {e}")))?;
        Ok(Self { inner })
    }

    /// Sets what runs beside each seat's unit: the binary's uinput proxy. The first call wins.
    pub fn on_unit(self, hook: UnitHook) -> Self {
        let _ = self.inner.hook.set(hook);
        self
    }

    /// Opens the ledger in `seats` under [`STATE_DIR`] and gives the service this backend.
    /// The ledger store closes its directory to root; seat users walk through it, so it reopens
    /// before the ledger loads, and a ledger that won't load leaves running seats their homes.
    pub fn open_service(self) -> Result<SeatService<Self>, StoreError> {
        let root = ledger_root(&self.inner.state);
        let store = LedgerStore::open(&root)?;
        shared::make_dir(&root, 0o711)?;
        let service = SeatService::with_store(store, self)?;
        service.backend().reap_orphans(&service.ledger());
        Ok(service)
    }

    /// Removes the seat accounts no ledger row names, with their directories: what a create that
    /// stopped after `useradd` left, which refuses every later create of its name. A seat that has
    /// been started keeps its account, since its files may outlive a lost ledger; so does one
    /// whose unit is up or can't be read.
    pub fn reap_orphans(&self, ledger: &Ledger) {
        let users = match accounts::all() {
            Ok(users) => users,
            Err(error) => {
                tracing::warn!(%error, "seat accounts not listed");
                return;
            }
        };
        for (user, id) in accounts::orphans(&users, ledger, &self.inner.state) {
            let account = &user.name;
            // The token directory exists once the seat has started; one that can't be read counts.
            let token_dir = shared::host_dir(&self.inner.state, &id);
            let started = !matches!(token_dir.try_exists(), Ok(false));
            let idle = session::state(account).is_ok_and(|state| !state.live());
            if started || !idle {
                tracing::warn!(%account, seat = %id, "seat account without a ledger row kept");
                continue;
            }
            match self.remove_files(account, &id, false) {
                Ok(()) => {
                    tracing::warn!(%account, seat = %id, "seat account without a ledger row removed")
                }
                Err(error) => {
                    tracing::warn!(%account, seat = %id, %error, "seat account not removed")
                }
            }
        }
    }

    /// The account of the running seat whose user is `uid`: whom the pad broker serves.
    pub fn running_account(&self, uid: u32) -> Option<String> {
        self.inner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|tracked| tracked.uid == uid)
            .map(|tracked| tracked.account.clone())
    }

    fn track(&self, id: &SeatId, tracked: Tracked) {
        self.inner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), tracked);
    }

    fn untrack(&self, id: &SeatId) {
        self.inner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    /// The Steam a seat clones and shows: `--steam-source` when given, else the box owner's, as
    /// their row records them. Only a box with no owner row looks for the lowest-uid user's.
    fn steam_source(&self) -> Option<shared::SteamSource> {
        if let Some(root) = &self.inner.steam_source {
            return shared::SteamSource::owned(root.clone());
        }
        let owner = self
            .inner
            .owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match owner {
            Some(owner) => shared::steam_source_of(&owner),
            None => shared::find_steam_source(&accounts::all().ok()?),
        }
    }

    fn stop_unit(&self, seat: &Seat) -> Result<(), BackendError> {
        stop_unit(&self.inner, &seat.id, &seat.account, seat.owner)
    }

    /// The owner's account and what its host reads, in place: the trust copy and the environment
    /// file, with a fresh token when the file holds none. A host that is up keeps the token it
    /// started with. The box's library follows their home from here on. `true` when the
    /// environment file changed. Only the account `door-helper` recorded gets any of it: a row
    /// left behind for another account writes nothing and never starts.
    fn prepare_owner(&self, seat: &Seat, passwd: &accounts::Passwd) -> Result<bool, BackendError> {
        accounts::require_recorded(&self.inner.state, &seat.account)?;
        let reader = Tracked::of(seat, passwd).reader;
        shared::refresh_trust(&self.inner.box_dir, &self.inner.state, &seat.id, reader)?;
        *self.inner.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(passwd.clone());
        if let Err(error) = refresh_box_library(&self.inner) {
            tracing::warn!(account = %seat.account, %error, "box library not copied");
        }
        if let Some(changed) = rewrite_owner_env(&self.inner, seat, passwd)? {
            return Ok(changed);
        }
        self.write_owner_files(seat, passwd, &session::mint_token())?;
        Ok(true)
    }

    fn write_owner_files(
        &self,
        seat: &Seat,
        passwd: &accounts::Passwd,
        token: &str,
    ) -> Result<(), BackendError> {
        let state = &self.inner.state;
        let door = accounts::lookup("punktfunk")?;
        shared::write_host_token(
            state,
            &seat.id,
            &session::token_line(token),
            shared::token_owner(door.as_ref()),
        )?;
        let owner = Some((passwd.uid, passwd.gid));
        let browser = session::browser(&self.inner.box_dir);
        session::write_unit_files(seat, &passwd.home, state, token, owner, None, &browser)
    }

    /// Writes the owner's files again at supervisor start: `/run` is empty after a boot, and the
    /// owner's own host would otherwise start on the box's ports, in the door's way. A host
    /// already up restarts onto files that changed; at boot none is up yet. The row is tracked, as
    /// [`PlatformBackend::adopt`] tracks it, for as long as it exists.
    pub fn prepare_owners(&self, ledger: &Ledger) {
        for seat in ledger.seats.iter().filter(|s| s.owner) {
            let prepared = accounts::require_owner(&seat.account).and_then(|passwd| {
                let fresh = self.prepare_owner(seat, &passwd)?;
                self.track(&seat.id, Tracked::of(seat, &passwd));
                Ok(fresh)
            });
            match prepared {
                Ok(true) => user_host(&seat.account, "try-restart"),
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(account = %seat.account, %error, "owner's host files not written")
                }
            }
        }
    }

    /// Whether the seat's user, and nobody else, listens on the row's management port: its host,
    /// not a squatter on the loopback.
    fn serving(&self, seat: &Seat) -> bool {
        let user = accounts::lookup(&seat.account).ok().flatten();
        user.is_some_and(|user| session::port_held_by(seat.mgmt_port, user.uid))
    }

    /// Whether the seat's host answers: the unit is active and the host's management port is up,
    /// and for the owner's row the runner has marked the headless host as its own.
    fn is_ready(&self, seat: &Seat) -> bool {
        self.serving(seat) && (!seat.owner || session::ready_path(&seat.account).exists())
    }

    /// Fails when a stop has been asked since `asked`, the count a start began at.
    fn stopped_since(&self, asked: u64) -> Result<(), BackendError> {
        if self.inner.stops.load(Ordering::SeqCst) == asked {
            return Ok(());
        }
        Err(err("seat_stopped", "the seat was stopped while it started"))
    }

    /// Waits for the unit to be active and the host's management port to answer. A unit that
    /// ends first, a stop or a timeout stops the seat and fails the start.
    fn wait_ready(&self, seat: &Seat, asked: u64) -> Result<(), BackendError> {
        let deadline = Instant::now() + START_TIMEOUT;
        let failure = loop {
            if let Err(stopped) = self.stopped_since(asked) {
                break stopped;
            }
            let state = session::state(&seat.account)?;
            match state.active.as_str() {
                "active" if self.is_ready(seat) => return Ok(()),
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

    /// Removes what a seat's row made. The owner's account and home are theirs and stay.
    fn remove_files(&self, account: &str, id: &SeatId, owner: bool) -> Result<(), BackendError> {
        if !owner {
            accounts::delete(account, id)?;
        }
        shared::remove_seat_dirs(&self.inner.state, id)?;
        let _ = std::fs::remove_file(session::env_path(account));
        let dropin = session::dropin_dir(account);
        if dropin.exists() {
            let _ = std::fs::remove_dir_all(dropin);
            session::daemon_reload()?;
        }
        Ok(())
    }

    /// Runs the prerequisite checks now and keeps them for `Seating` to reuse.
    fn recheck(&self) -> Vec<Diagnostic> {
        let mut slot = self.inner.checks.lock().unwrap_or_else(|e| e.into_inner());
        let checks = self.prerequisites();
        *slot = Some((Instant::now(), checks.clone()));
        checks
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
            group(socket::DOOR_GROUP),
            "door_group",
            "the punktfunk-door group exists",
            "The punktfunk-door group doesn't exist; reinstall the package that creates it.",
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
            "usermod",
            "setfacl",
            "loginctl",
            "systemctl",
            "setpriv",
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
        // What systemd would run, wherever the package put it.
        let installed = run(
            "seat_unit",
            "systemctl",
            &[
                "show",
                "punktfunk-seat@doctor.service",
                "-p",
                "LoadState",
                "-p",
                "ExecStart",
                "-p",
                "ExecStopPost",
            ],
        )
        .is_ok_and(|show| {
            let programs = exec_paths(&show);
            show.lines().any(|l| l == "LoadState=loaded")
                && programs.len() >= 2
                && programs.iter().all(|p| p.is_file())
        });
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
        accounts::check_seat_name(&seat.account)?;
        let state = &self.inner.state;
        let gid = accounts::ensure_group(accounts::GAMES_GROUP)?;
        shared::ensure_games(state, gid)?;
        let passwd = accounts::ensure(seat, state)?;
        accounts::close_home(&passwd.home)?;
        shared::ensure_seat_dirs(state, &seat.id, &passwd)?;
        shared::provision_steam(state, &seat.id, &passwd, self.steam_source().as_ref());
        Ok(())
    }

    /// The owner's row, for the account `door-helper` recorded and no other.
    fn adopt(&self, seat: &Seat) -> Result<(), BackendError> {
        let passwd = accounts::require_owner(&seat.account)?;
        self.prepare_owner(seat, &passwd)?;
        self.track(&seat.id, Tracked::of(seat, &passwd));
        Ok(())
    }

    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        let asked = self.inner.stops.load(Ordering::SeqCst);
        if seat.owner {
            return self.start_owner(seat, asked);
        }
        let (box_dir, state) = (&self.inner.box_dir, &self.inner.state);
        let passwd = accounts::require(seat)?;
        accounts::close_home(&passwd.home)?;
        shared::ensure_games(state, accounts::ensure_group(accounts::GAMES_GROUP)?)?;
        shared::ensure_seat_dirs(state, &seat.id, &passwd)?;
        let reader = shared::Reader::group(passwd.gid);
        shared::refresh_trust(box_dir, state, &seat.id, reader)?;
        shared::refresh_seat_library(state, &seat.id, reader)?;
        let unit = session::unit(&seat.account);
        unit_hook(&self.inner, &seat.account, Some(passwd.uid));
        // A unit already up keeps its token: the host read it when it started.
        if !session::state(&seat.account)?.live() {
            if session::port_taken(seat.mgmt_port) {
                return Err(err(
                    "port_in_use",
                    format!("management port {} is already in use", seat.mgmt_port),
                ));
            }
            let token = session::mint_token();
            let door = accounts::lookup("punktfunk")?;
            shared::write_host_token(
                state,
                &seat.id,
                &session::token_line(&token),
                shared::token_owner(door.as_ref()),
            )?;
            let source = self.steam_source();
            let root = source.as_ref().map(|s| s.root.as_path());
            let shares = libraries::share(box_dir, state, &seat.id, &passwd, root);
            let shown = shares.as_ref().map_or(&[][..], |s| s.libraries.as_slice());
            libraries::register(state, &seat.id, &passwd, shown);
            let browser = session::browser(box_dir);
            session::write_unit_files(
                seat,
                &passwd.home,
                state,
                &token,
                None,
                shares.as_ref(),
                &browser,
            )?;
            self.stopped_since(asked)?;
            let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
            run("systemctl_failed", "systemctl", &["start", &unit])?;
        }
        self.track(&seat.id, Tracked::of(seat, &passwd));
        self.wait_ready(seat, asked)?;
        Ok(RuntimeStatus::running())
    }

    /// Ends the headless session. A host in the owner's own login stays: it isn't the unit's.
    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.inner.stops.fetch_add(1, Ordering::SeqCst);
        self.stop_unit(seat)?;
        Ok(RuntimeStatus::stopped())
    }

    /// The owner's row takes the box's library with it: it was the door's, and the door goes too.
    /// A row the owner's new row has replaced leaves the library, which is the new one's.
    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        let _level = self.inner.level.lock().unwrap_or_else(|e| e.into_inner());
        // Before the unit is stopped: ending a stranger's sessions is not removing a seat.
        if !seat.owner {
            accounts::refuse_foreign(&seat.account, &seat.id)?;
        }
        self.stop_unit(seat)?;
        self.untrack(&seat.id);
        if seat.owner {
            let mut owner = self.inner.owner.lock().unwrap_or_else(|e| e.into_inner());
            if owner
                .as_ref()
                .is_none_or(|owner| owner.name == seat.account)
            {
                *owner = None;
                drop(owner);
                shared::remove_box_library(&self.inner.state)
                    .map_err(|e| err("box_library", format!("remove the box library: {e}")))?;
            }
        }
        self.remove_files(&seat.account, &seat.id, seat.owner)
    }

    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        let state = session::state(&seat.account)?;
        if seat.owner {
            return Ok(self.owner_status(seat, &state));
        }
        Ok(state.runtime())
    }

    /// A unit found up is tracked and gets its proxy; one that is down loses both. A unit between
    /// the two, stopping, keeps them: a restart rebuilds its namespace on the same proxy.
    /// The owner's row stays tracked from its adoption.
    fn reconcile(&self, seat: &Seat, status: &RuntimeStatus) {
        if seat.owner {
            return;
        }
        match status.state {
            RuntimeState::Running | RuntimeState::Starting => {
                let known = self
                    .inner
                    .running
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&seat.id);
                if !known && let Ok(passwd) = accounts::require(seat) {
                    self.track(&seat.id, Tracked::of(seat, &passwd));
                    unit_hook(&self.inner, &seat.account, Some(passwd.uid));
                }
            }
            RuntimeState::Stopping => {}
            _ => {
                self.untrack(&seat.id);
                unit_hook(&self.inner, &seat.account, None);
            }
        }
    }

    /// A root-owned copy of the certificate the seat's host serves from `~/.config/punktfunk`,
    /// for the service to pin. That home is an account's root doesn't own, so the copy is taken
    /// through directory handles and never waits on what is there ([`shared::copy_pin`]).
    fn host_config_dir(&self, seat: &Seat) -> Option<PathBuf> {
        let passwd = if seat.owner {
            accounts::require_owner(&seat.account)
        } else {
            accounts::require(seat)
        };
        let dir = shared::pin_dir(&self.inner.state, &seat.id);
        if let Err(error) = shared::copy_pin(&passwd.ok()?.home, &dir) {
            tracing::debug!(seat = %seat.id, %error, "seat host's certificate not read");
        }
        Some(dir)
    }

    fn doctor(&self, ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        let mut out = self.recheck();
        out.extend(fence::diagnostics());
        for seat in &ledger.seats {
            let found = accounts::lookup(&seat.account)?;
            let owned = found.as_ref().is_some_and(|found| {
                if seat.owner {
                    accounts::is_ordinary(found)
                } else {
                    accounts::owned_by(found, &seat.id)
                }
            });
            let (good, bad) = if seat.owner {
                (
                    "the owner's account exists and is an ordinary user",
                    "the owner's account is gone or isn't an ordinary user",
                )
            } else {
                (
                    "account marker matches the exact seat ID",
                    "account is absent or its marker does not match",
                )
            };
            out.push(Diagnostic {
                level: if owned {
                    DiagnosticLevel::Info
                } else {
                    DiagnosticLevel::Error
                },
                code: "account_marker".into(),
                message: if owned { good } else { bad }.into(),
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

    /// The checks are the last ones run within [`CHECKS_FRESH`]: the door asks every 2 s and
    /// reads `enabled`, which is constant. Doctor and Enable run them afresh.
    fn seating(&self) -> Result<SeatingStatus, BackendError> {
        let mut slot = self.inner.checks.lock().unwrap_or_else(|e| e.into_inner());
        let checks = cached(&mut slot, Instant::now(), || self.prerequisites());
        Ok(SeatingStatus {
            enabled: true,
            checks,
            allow_rdp_from_network: false,
        })
    }

    /// Seats are on whenever the daemon runs.
    fn enable(
        &self,
        _allow_rdp_from_network: bool,
        _prepared: Vec<Diagnostic>,
    ) -> Result<SeatingStatus, BackendError> {
        Ok(SeatingStatus {
            enabled: true,
            checks: self.recheck(),
            allow_rdp_from_network: false,
        })
    }

    fn disable(&self, _keep_accounts: bool) -> Result<SeatingStatus, BackendError> {
        Err(err(
            "not_supported",
            "seats stay on while the supervisor runs; stop its service to turn them off",
        ))
    }
}

impl LinuxBackend {
    /// The owner's row up. With the owner at the machine their own login hosts it: the headless
    /// unit stays down and the owner's user host is started if it isn't running. Otherwise the
    /// unit brings up the headless session and the host in it, whose runner stands the owner's
    /// user host down first.
    fn start_owner(&self, seat: &Seat, asked: u64) -> Result<RuntimeStatus, BackendError> {
        let passwd = accounts::require_owner(&seat.account)?;
        self.prepare_owner(seat, &passwd)?;
        self.track(&seat.id, Tracked::of(seat, &passwd));
        if owner_at_desk(&seat.account) {
            user_host(&seat.account, "start");
            self.wait_port(seat, asked)?;
            return Ok(RuntimeStatus::running());
        }
        let unit = session::unit(&seat.account);
        // A unit already up keeps its token: the host read it when it started.
        if !session::state(&seat.account)?.live() {
            self.write_owner_files(seat, &passwd, &session::mint_token())?;
            self.stopped_since(asked)?;
            let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
            run("systemctl_failed", "systemctl", &["start", &unit])?;
        }
        self.wait_ready(seat, asked)?;
        Ok(RuntimeStatus::running())
    }

    fn wait_port(&self, seat: &Seat, asked: u64) -> Result<(), BackendError> {
        let deadline = Instant::now() + START_TIMEOUT;
        while !self.serving(seat) {
            self.stopped_since(asked)?;
            if Instant::now() >= deadline {
                return Err(err(
                    "start_timeout",
                    "the owner's host did not open its management port within 90 seconds",
                ));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        Ok(())
    }

    /// The owner's row as it is: the headless unit's state while it runs, else running when the
    /// owner is at the machine and their own host answers on the row's ports, else stopped. A host
    /// the owner's manager keeps without a login has no desktop to stream, so it isn't the row.
    fn owner_status(&self, seat: &Seat, state: &session::UnitState) -> RuntimeStatus {
        if state.live() {
            if state.active == "active" && !self.is_ready(seat) {
                return RuntimeStatus {
                    state: RuntimeState::Starting,
                    detail: Some("starting".into()),
                };
            }
            return state.runtime();
        }
        if self.serving(seat) && owner_at_desk(&seat.account) {
            return RuntimeStatus::running();
        }
        state.runtime()
    }
}

/// While a seat runs: refreshes its trust copy when the box's files change, the owner's
/// environment file, and, for a seat other than the owner's, the box's library, which follows
/// the owner's own first. Ends the owner's headless session when the owner sits down at the
/// machine, and repairs the games folder's ACL mask after a seat stops and every [`ACL_EVERY`].
/// The owner's row stays listed while its host is the owner's own, which reads the same copy.
/// Ends when the backend is dropped.
fn keep_level(inner: Weak<Inner>) {
    let mut acl_at = Instant::now();
    let mut failing: HashSet<SeatId> = HashSet::new();
    let mut library_failing = false;
    loop {
        std::thread::sleep(TRUST_TICK);
        let Some(inner) = inner.upgrade() else { return };
        // Logged once per outage, like a trust copy below.
        match refresh_box_library(&inner) {
            Ok(_) => library_failing = false,
            Err(error) if !library_failing => {
                library_failing = true;
                tracing::warn!(%error, "box library not copied");
            }
            Err(_) => {}
        }
        let seats: Vec<(SeatId, Tracked)> = inner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, tracked)| (id.clone(), tracked.clone()))
            .collect();
        for (id, tracked) in &seats {
            let level = inner.level.lock().unwrap_or_else(|e| e.into_inner());
            // Removed since the list was taken: its files are gone and stay gone.
            let tracked_now = inner.running.lock().unwrap_or_else(|e| e.into_inner());
            if !tracked_now.contains_key(id) {
                continue;
            }
            drop(tracked_now);
            let refreshed = shared::refresh_trust(&inner.box_dir, &inner.state, id, tracked.reader)
                .and_then(|_| match &tracked.owner {
                    Some((seat, passwd)) => rewrite_owner_env(&inner, seat, passwd).map(|_| false),
                    None => shared::refresh_seat_library(&inner.state, id, tracked.reader),
                });
            drop(level);
            match refreshed {
                Ok(_) => {
                    failing.remove(id);
                }
                // Logged once per outage, not every tick.
                Err(error) if failing.insert(id.clone()) => {
                    tracing::warn!(seat = %id, %error, "trust copy not refreshed");
                }
                Err(_) => {}
            }
            if tracked.owner.is_some() {
                yield_to_the_desk(&inner, id, &tracked.account);
            }
        }
        let seat_up = seats.iter().any(|(_, tracked)| tracked.owner.is_none());
        let due = seat_up && acl_at.elapsed() >= ACL_EVERY;
        if inner.acl_dirty.swap(false, Ordering::SeqCst) || due {
            acl_at = Instant::now();
            if let Err(error) = shared::fix_acl(&inner.state) {
                tracing::warn!(%error, "games ACL mask not repaired");
            }
        }
    }
}

/// Writes the owner's environment file again with the token it holds, as the box's Seat defaults
/// stand now: Browser streaming reaches the owner's host at its next start. `None` when the file
/// holds no token, `Some(true)` when it changed.
fn rewrite_owner_env(
    inner: &Inner,
    seat: &Seat,
    passwd: &accounts::Passwd,
) -> Result<Option<bool>, BackendError> {
    let Some(token) = session::env_token(&seat.account) else {
        return Ok(None);
    };
    let browser = session::browser(&inner.box_dir);
    let owner = Some((passwd.uid, passwd.gid));
    let changed = session::write_env(seat, &passwd.home, &inner.state, &token, owner, &browser)?;
    if changed {
        tracing::info!(account = %seat.account, browser = browser.on,
            "owner's environment file rewritten for its host's next start");
    }
    Ok(Some(changed))
}

/// The physical login wins: when the owner has a session at the machine and the row's headless
/// unit is up, the unit ends and the owner's own host takes the row. The owner's session is never
/// touched, so what they have on the monitor stays as it is.
fn yield_to_the_desk(inner: &Inner, id: &SeatId, account: &str) {
    let live = session::state(account).is_ok_and(|s| s.live());
    if !live || !owner_at_desk(account) {
        return;
    }
    tracing::info!(%account, "owner at the machine: ending the headless session");
    match stop_unit(inner, id, account, true) {
        Ok(()) => user_host(account, "start"),
        Err(error) => tracing::warn!(%account, %error, "headless session not ended"),
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

/// The programs in `systemctl show`'s `Exec*` lines: each `{ path=<program> ; argv[]=… }`.
fn exec_paths(show: &str) -> Vec<PathBuf> {
    show.split("path=")
        .skip(1)
        .filter_map(|rest| rest.split_whitespace().next())
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn doctor_reads_the_programs_systemd_would_run() {
        let show = "LoadState=loaded\n\
            ExecStart={ path=/nix/store/abc-seats/libexec/punktfunk/seat-session ; argv[]=/nix/store/abc-seats/libexec/punktfunk/seat-session ; ignore_errors=no }\n\
            ExecStopPost={ path=/nix/store/abc-seats/libexec/punktfunk/seat-reap ; argv[]=x %i ; ignore_errors=no }\n";
        assert_eq!(
            super::exec_paths(show),
            [
                std::path::PathBuf::from("/nix/store/abc-seats/libexec/punktfunk/seat-session"),
                std::path::PathBuf::from("/nix/store/abc-seats/libexec/punktfunk/seat-reap"),
            ]
        );
        assert!(super::exec_paths("LoadState=not-found\nExecStart=\n").is_empty());
    }

    use super::*;

    /// The checks run once, then are reused until they are `CHECKS_FRESH` old.
    #[test]
    fn the_seating_checks_are_reused_while_they_are_fresh() {
        let (mut slot, start) = (None, Instant::now());
        let mut runs = 0;
        let mut ask = |slot: &mut _, at: Instant| {
            cached(slot, at, || {
                runs += 1;
                vec![Diagnostic::info("run", runs.to_string())]
            })
        };
        assert_eq!(ask(&mut slot, start)[0].message, "1");
        assert_eq!(ask(&mut slot, start + CHECKS_FRESH / 2)[0].message, "1");
        assert_eq!(ask(&mut slot, start + CHECKS_FRESH)[0].message, "2");
        assert_eq!(ask(&mut slot, start + CHECKS_FRESH * 3 / 2)[0].message, "2");
    }

    /// A backend over `box_dir` and `state`, with no thread keeping it level.
    fn backend(box_dir: &Path, state: &Path) -> LinuxBackend {
        LinuxBackend {
            inner: Arc::new(Inner {
                box_dir: box_dir.into(),
                state: state.into(),
                steam_source: None,
                running: Mutex::new(HashMap::new()),
                owner: Mutex::new(None),
                door_gid: OnceLock::new(),
                acl_dirty: AtomicBool::new(false),
                stops: AtomicU64::new(0),
                hook: OnceLock::new(),
                checks: Mutex::new(None),
                level: Mutex::new(()),
            }),
        }
    }

    /// An owner row `door-helper` no longer records, as a switch-off that missed the row leaves
    /// it, gets no files and no owner, at supervisor start or on demand.
    #[test]
    fn an_owner_row_for_an_unrecorded_account_writes_nothing() {
        let (box_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let backend = backend(box_dir.path(), state.path());
        let seat = Seat {
            id: SeatId::parse("0123456789abcdef0123456789abcdef").unwrap(),
            name: "bob".into(),
            account: "bob".into(),
            display_slot: 12,
            native_port: 9779,
            mgmt_port: 47995,
            autostart: false,
            runtime: crate::model::RuntimeStatus::stopped(),
            owner: true,
            fingerprint: None,
            adopted_sid: None,
            rdp_granted: false,
        };
        let passwd = accounts::Passwd {
            name: "bob".into(),
            uid: 1001,
            gid: 1001,
            gecos: String::new(),
            home: state.path().join("home"),
        };
        std::fs::write(state.path().join("owner"), "alice\n").unwrap();
        let refused = backend.prepare_owner(&seat, &passwd).unwrap_err();
        assert_eq!(refused.code, "owner_not_recorded");
        assert!(backend.inner.owner.lock().unwrap().is_none());
        assert_eq!(
            std::fs::read_dir(state.path()).unwrap().count(),
            1,
            "only the record"
        );
    }

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
