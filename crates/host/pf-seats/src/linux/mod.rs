//! The Linux seat backend: a user, a logind session and a systemd unit per seat.
//!
//! The root daemon `punktfunk-seats` runs [`LinuxBackend`] behind [`socket`]. A seat is a
//! system-range user with its home under the box directory. Starting it writes the seat contract
//! and a fresh management token, then starts `punktfunk-seat@<user>.service`: that unit logs the
//! user in through PAM and runs `seat-session`, which brings up a headless compositor and a stock
//! `punktfunk-host serve`. Everything a seat runs descends from the unit, so the private
//! `compatdata` bind mounts reach it. A restart reads each unit's state back from systemd.
//!
//! The owner's row is the box owner's own account, adopted. Its unit runs the owner's host in a
//! headless `background` session, and never while the owner sits at the machine: a login of theirs
//! on a seat ends the unit, and the owner's own user host serves the row from then on. That
//! host writes the box's library: the daemon copies it for the door and every other seat.
//!
//! Another person's row adopts their account the same way, once [`pam`] accepts their password
//! in a child process: a headless `user` session, never while they sit at the machine, whose
//! host admits only the devices the row's list does ([`devices`]). The daemon changes nothing on
//! the account and ends nothing but its unit.
//!
//! A seat's pads are the daemon's too: its host asks the pad broker in the `punktfunk-seats`
//! binary for them ([`LinuxBackend::running_account`] is its gate), and [`fence`] gives udev the
//! user each one belongs to. The binary's uinput proxy, the seat's `/dev/uinput`, comes and goes
//! with the unit ([`LinuxBackend::on_unit`]).

mod accounts;
mod devices;
#[cfg(test)]
mod fake;
pub mod fence;
mod flatpak;
pub mod handover;
mod libraries;
pub mod login;
mod logind;
mod pam;
#[cfg(test)]
mod person_tests;
mod safe_open;
mod session;
mod shared;
pub mod socket;
#[cfg(test)]
mod unlock_tests;

pub use pam::{run_check as pam_check, CHECK_VERB as PAM_CHECK_VERB};
pub use shared::ledger_root;
pub use socket::SOCKET_PATH;

use crate::backend::{BackendError, PlatformBackend};
use crate::ipc::{Diagnostic, DiagnosticLevel, SeatingStatus};
use crate::model::{Ledger, RuntimeState, RuntimeStatus, Seat, SeatId};
use crate::persistence::{LedgerStore, StoreError};
use crate::secret::Secret;
use crate::service::SeatService;
use crate::unlock::DeviceList;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// The supervisor's own state: the ledger, every seat's home and trust copy, the shared games
/// folder. Root's alone; the box directory beside it is the door's, which reads its files only.
pub const STATE_DIR: &str = "/var/lib/punktfunk-seats";

/// A seat's save exchange: `out` holds what its host exported for the owner's plugins, `in`
/// what the owner's host hands back. The same path inside the seat's unit.
pub fn saves_dir(id: &SeatId) -> PathBuf {
    shared::seat_dir(Path::new(STATE_DIR), id).join("saves")
}

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

/// Runs `program` to its end with no stdin. A test installs [`fake`], which records the call and
/// answers without running anything.
fn output(program: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    #[cfg(test)]
    if let Some(answer) = fake::answer(program, args) {
        return Ok(answer);
    }
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
}

/// Runs `program`, returning its stdout. A failure names the program, its first argument and its
/// stderr under `code`.
fn run(code: &str, program: &str, args: &[&str]) -> Result<String, BackendError> {
    let output =
        output(program, args).map_err(|error| err(code, format!("run {program}: {error}")))?;
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
    /// Each person's row's device list, loaded on first use. Held across each change, its save
    /// and the trust copy it rewrites, so two writers never cross. Taken after `level`.
    devices: Mutex<HashMap<SeatId, devices::Held>>,
    /// The password of a start through the login helper, while it waits for the helper.
    handover: handover::Handover,
    /// Set once a login helper tripped ([`LOGIN_TRIPPED`]): no helper until the supervisor
    /// restarts.
    helper_off: AtomicBool,
    /// What the login socket carries, once the binary bound it ([`LinuxBackend::login_socket`]).
    login_label: OnceLock<handover::Label>,
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

/// Whose account a row runs on, which decides what the supervisor may do to it. A made seat's
/// account is the supervisor's and ends with the row. The box owner's and another person's
/// belong to someone who may be logged in at the machine: the row is a guest there, and its
/// unit is all that is ever stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Own {
    Seat,
    Owner,
    Person,
}

impl Own {
    fn of(seat: &Seat) -> Self {
        if seat.owner {
            Self::Owner
        } else if seat.adopted_uid.is_some() {
            Self::Person
        } else {
            Self::Seat
        }
    }

    /// Whether the account is the supervisor's to make, close, delete and clear of every
    /// process it runs. Only a made seat's is.
    fn ours(self) -> bool {
        self == Self::Seat
    }

    /// Whether the row's unit ends when its user sits down at the machine.
    fn yields_to_the_desk(self) -> bool {
        self != Self::Seat
    }
}

/// What the keep-level thread and the pad broker need of a running seat.
#[derive(Clone, Debug)]
struct Tracked {
    /// Who reads its trust copy.
    reader: shared::Reader,
    /// The row and account, for a row on a person's own account: its physical login ends the
    /// unit, and its environment file follows the box's Seat defaults.
    guest: Option<(Seat, accounts::Passwd)>,
    /// The seat's user: what the pad broker admits, and what its pads are stamped with.
    uid: u32,
    account: String,
}

impl Tracked {
    fn of(seat: &Seat, passwd: &accounts::Passwd) -> Self {
        let (reader, guest) = if Own::of(seat).ours() {
            (shared::Reader::group(passwd.gid), None)
        } else {
            (
                shared::Reader::person(passwd.uid, passwd.gid),
                Some((seat.clone(), passwd.clone())),
            )
        };
        Self {
            reader,
            guest,
            uid: passwd.uid,
            account: seat.account.clone(),
        }
    }
}

/// The wall clock in unix seconds, as presence is kept.
fn unix_now() -> i64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    now.map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `f` over `id`'s held device list, loaded on first use, with the devices lock held throughout.
fn with_devices<T>(
    inner: &Inner,
    id: &SeatId,
    f: impl FnOnce(&mut devices::Held, i64) -> Result<T, BackendError>,
) -> Result<T, BackendError> {
    let now = unix_now();
    let mut all = inner.devices.lock().unwrap_or_else(|e| e.into_inner());
    let held = all
        .entry(id.clone())
        .or_insert_with(|| devices::load(&inner.state, id, now));
    f(held, now)
}

/// `id`'s list after `edit`, from the list with its expired presence dropped. A change is saved,
/// then the trust copy's pairing list follows who is admitted.
fn edit_devices(
    inner: &Inner,
    id: &SeatId,
    edit: &mut dyn FnMut(&mut DeviceList, i64),
) -> Result<DeviceList, BackendError> {
    with_devices(inner, id, |held, now| {
        let mut next = held.list.clone();
        next.expire(now);
        edit(&mut next, now);
        if next != held.list {
            held.commit(&inner.state, id, next.clone())?;
            shared::rewrite_admitted(&inner.box_dir, &inner.state, id, &next.admitted())?;
        }
        Ok(next)
    })
}

/// A person's session ended: so has every presence on its row.
fn end_presence(inner: &Inner, id: &SeatId) {
    if let Err(error) = edit_devices(inner, id, &mut |list, _| list.end_presence()) {
        tracing::warn!(seat = %id, %error, "presence not ended");
    }
}

/// A person's trust copy for `reader`, its pairing list holding only the devices admitted now.
fn person_trust(inner: &Inner, id: &SeatId, reader: shared::Reader) -> Result<bool, BackendError> {
    with_devices(inner, id, |held, now| {
        let mut next = held.list.clone();
        if next.expire(now) {
            held.commit(&inner.state, id, next.clone())?;
        }
        let admitted = next.admitted();
        shared::refresh_person_trust(&inner.box_dir, &inner.state, id, reader, &admitted)
    })
}

/// Stops `seat`'s unit if it is up. A made seat's user ends with it, so nothing it started under
/// its own manager survives, and then the hook takes back what runs beside it. A row on a
/// person's own account ends nothing else, signals none of their processes and leaves the hook
/// alone: their own login may hold pads; every presence on it ends. The owner's row stays
/// tracked, since their own host reads its trust copy; a person's stops being served. A unit
/// that was never started is fine.
fn stop_unit(inner: &Inner, seat: &Seat) -> Result<(), BackendError> {
    let (id, account) = (&seat.id, &seat.account);
    let unit = session::unit(account);
    if session::state(account)?.active != "inactive" {
        run("systemctl_failed", "systemctl", &["stop", &unit])?;
    }
    // A failed unit keeps its state until reset, and would read as failed after a stop.
    let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
    inner.acl_dirty.store(true, Ordering::SeqCst);
    let kind = Own::of(seat);
    if kind == Own::Owner {
        return Ok(());
    }
    // Untracked first, so the pad broker refuses the user while it ends.
    inner
        .running
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(id);
    if kind == Own::Person {
        end_presence(inner, id);
    }
    if !kind.ours() {
        return Ok(());
    }
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
            devices: Mutex::new(HashMap::new()),
            handover: handover::Handover::default(),
            helper_off: AtomicBool::new(false),
            login_label: OnceLock::new(),
        });
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("seats-level".into())
            .spawn(move || keep_level(weak))
            .map_err(|e| err("thread", format!("start the trust thread: {e}")))?;
        Ok(Self { inner })
    }

    /// A backend over `box_dir` and `state`, with no thread keeping it level.
    #[cfg(test)]
    fn detached(box_dir: &Path, state: &Path) -> Self {
        Self {
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
                devices: Mutex::new(HashMap::new()),
                handover: handover::Handover::default(),
                helper_off: AtomicBool::new(false),
                login_label: OnceLock::new(),
            }),
        }
    }

    /// Records what the login socket the binary bound carries ([`handover::bind`]), which
    /// decides whether a start may go through the login helper. The first call wins.
    pub fn login_socket(&self, label: handover::Label) {
        let _ = self.inner.login_label.set(label);
    }

    /// Sets what runs beside each seat's unit: the binary's uinput proxy. The first call wins.
    pub fn on_unit(self, hook: UnitHook) -> Self {
        let _ = self.inner.hook.set(hook);
        self
    }

    /// Opens the ledger in `seats` under [`STATE_DIR`] and gives the service this backend.
    /// The ledger store closes its directory to root; seat users walk through it, so it reopens
    /// before the ledger loads, and a ledger that won't load leaves running seats their homes.
    /// Each person's device list loads now, its expired presence dropped.
    pub fn open_service(self) -> Result<SeatService<Self>, StoreError> {
        let root = ledger_root(&self.inner.state);
        let store = LedgerStore::open(&root)?;
        shared::make_dir(&root, 0o711)?;
        let service = SeatService::with_store(store, self)?;
        service.backend().reap_orphans(&service.ledger());
        service.backend().load_devices(&service.ledger());
        Ok(service)
    }

    /// Loads each person's row's device list and writes its trust copy's pairing list from it,
    /// so a presence that ran out while the supervisor was down admits nobody.
    fn load_devices(&self, ledger: &Ledger) {
        for seat in ledger.seats.iter().filter(|s| Own::of(s) == Own::Person) {
            let written = with_devices(&self.inner, &seat.id, |held, _| {
                let admitted = held.list.admitted();
                shared::rewrite_admitted(
                    &self.inner.box_dir,
                    &self.inner.state,
                    &seat.id,
                    &admitted,
                )
            });
            if let Err(error) = written {
                tracing::warn!(seat = %seat.id, %error, "pairing list not written");
            }
        }
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
            match self.remove_files(account, &id, true) {
                Ok(()) => {
                    tracing::warn!(%account, seat = %id, "seat account without a ledger row removed")
                }
                Err(error) => {
                    tracing::warn!(%account, seat = %id, %error, "seat account not removed")
                }
            }
        }
    }

    /// The box owner's uid once their row is prepared, for the account `door-helper` recorded:
    /// the one peer besides root and the door the socket serves, for Flatpak changes only.
    pub fn owner_uid(&self) -> Option<u32> {
        self.owner().map(|owner| owner.uid)
    }

    fn owner(&self) -> Option<accounts::Passwd> {
        self.inner
            .owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
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
        match self.owner() {
            Some(owner) => shared::steam_source_of(&owner),
            None => shared::find_steam_source(&accounts::all().ok()?),
        }
    }

    fn stop_unit(&self, seat: &Seat) -> Result<(), BackendError> {
        stop_unit(&self.inner, seat)
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
        if let Some(changed) = rewrite_env(&self.inner, seat, passwd)? {
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
    /// ends first, a stop or a timeout stops the seat and fails the start. `login`: the unit runs
    /// the login helper, whose end says why ([`unit_ended`]).
    fn wait_ready(&self, seat: &Seat, asked: u64, login: bool) -> Result<(), BackendError> {
        let deadline = Instant::now() + START_TIMEOUT;
        let failure = loop {
            if let Err(stopped) = self.stopped_since(asked) {
                break stopped;
            }
            let state = session::state(&seat.account)?;
            match state.active.as_str() {
                "active" if self.is_ready(seat) => return Ok(()),
                "failed" | "inactive" => break unit_ended(seat, &state, login),
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

    /// Removes what a row made under the state directory and `/run`. `delete_account`: the
    /// account is a made seat's and goes with its home and its folders on the owner's library
    /// drives, its user manager's drop-in first, while its uid can still be read. A person's
    /// account, the owner's included, stays, and nothing of it is touched.
    fn remove_files(
        &self,
        account: &str,
        id: &SeatId,
        delete_account: bool,
    ) -> Result<(), BackendError> {
        let mut reload = false;
        if delete_account {
            if let Some(source) = self.steam_source() {
                libraries::forget(id, &source.root);
            }
            if let Ok(Some(user)) = accounts::lookup(account) {
                let manager = session::manager_dropin(user.uid);
                reload |= std::fs::remove_file(&manager).is_ok();
                if let Some(dir) = manager.parent() {
                    let _ = std::fs::remove_dir(dir);
                }
            }
            accounts::delete(account, id)?;
        }
        shared::remove_seat_dirs(&self.inner.state, id)?;
        let _ = std::fs::remove_file(session::env_path(account));
        let dropin = session::dropin_dir(account);
        if dropin.exists() {
            let _ = std::fs::remove_dir_all(dropin);
            reload = true;
        }
        if reload {
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
        shared::ensure_seat_dirs(state, &seat.id, &passwd, self.owner_uid())?;
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

    fn account_uid(&self, account: &str, ledger: &Ledger) -> Result<u32, BackendError> {
        accounts::adoptable(account, ledger, &self.inner.state).map(|found| found.uid)
    }

    /// The account still resolves to the row's uid, then [`pam::check`] runs the login stack in a
    /// process of its own. The password is kept nowhere.
    fn check_password(&self, seat: &Seat, password: &Secret) -> Result<(), BackendError> {
        accounts::require_person(seat)?;
        pam::check(&seat.account, password)
    }

    /// A person's row, whose password [`Self::check_password`] passed: the account still resolves
    /// to the recorded uid, then the row's directory is made. Nothing is made in the account, and
    /// a refusal leaves nothing behind.
    fn adopt_account(&self, seat: &Seat, _password: &Secret) -> Result<bool, BackendError> {
        let passwd = accounts::require_person(seat)?;
        let made = shared::ensure_person_dirs(&self.inner.state, &seat.id, &passwd);
        if made.is_err() {
            let _ = shared::remove_seat_dirs(&self.inner.state, &seat.id);
        }
        made.map(|()| false)
    }

    fn account_owned(&self, seat: &Seat) -> Result<(), BackendError> {
        accounts::require_person(seat).map(drop)
    }

    fn devices(
        &self,
        seat: &Seat,
        edit: &mut dyn FnMut(&mut DeviceList, i64),
    ) -> Result<DeviceList, BackendError> {
        if Own::of(seat) != Own::Person {
            return Err(err("not_a_person", "only a person's row lists devices"));
        }
        edit_devices(&self.inner, &seat.id, edit)
    }

    fn box_fingerprint(&self) -> Result<[u8; 32], BackendError> {
        devices::box_fingerprint(&self.inner.box_dir, &self.inner.state)
    }

    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        let asked = self.inner.stops.load(Ordering::SeqCst);
        match Own::of(seat) {
            Own::Owner => return self.start_owner(seat, asked),
            Own::Person => return self.start_person(seat, asked, None),
            Own::Seat => {}
        }
        let (box_dir, state) = (&self.inner.box_dir, &self.inner.state);
        let passwd = accounts::require(seat)?;
        accounts::close_home(&passwd.home)?;
        let games = accounts::ensure_group(accounts::GAMES_GROUP)?;
        shared::ensure_games(state, games)?;
        shared::ensure_seat_dirs(state, &seat.id, &passwd, self.owner_uid())?;
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
            let owner = self.owner();
            let shares = libraries::share(box_dir, state, &seat.id, &passwd, root, owner.as_ref());
            let shown = shares.as_ref().map_or(&[][..], |s| s.libraries.as_slice());
            libraries::register(state, &seat.id, &passwd, shown);
            let browser = session::browser(box_dir);
            session::write_unit_files(
                seat,
                &passwd.home,
                state,
                &token,
                Some((passwd.uid, passwd.gid)),
                shares.as_ref(),
                &browser,
            )?;
            self.stopped_since(asked)?;
            let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
            run("systemctl_failed", "systemctl", &["start", &unit])?;
        }
        self.track(&seat.id, Tracked::of(seat, &passwd));
        self.wait_ready(seat, asked, false)?;
        Ok(RuntimeStatus::running())
    }

    /// A person's row, started with the password they proved the start with: [`login_with`]
    /// decides whether that password opens their session.
    fn start_with_password(
        &self,
        seat: &Seat,
        password: &Secret,
    ) -> Result<RuntimeStatus, BackendError> {
        if Own::of(seat) != Own::Person {
            return self.start(seat);
        }
        let asked = self.inner.stops.load(Ordering::SeqCst);
        self.start_person(seat, asked, Some(password))
    }

    /// Ends the headless session. A host in the owner's own login stays: it isn't the unit's.
    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.inner.stops.fetch_add(1, Ordering::SeqCst);
        self.stop_unit(seat)?;
        Ok(RuntimeStatus::stopped())
    }

    /// The owner's row takes the box's library with it: it was the door's, and the door goes too.
    /// A row the owner's new row has replaced leaves the library, which is the new one's. A
    /// person's row releases the account: its unit stops and what the row made goes, its device
    /// list included, and nothing of the account does.
    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        let _level = self.inner.level.lock().unwrap_or_else(|e| e.into_inner());
        let kind = Own::of(seat);
        // Before the unit is stopped: ending a stranger's sessions is not removing a seat.
        if kind.ours() {
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
        let removed = self.remove_files(&seat.account, &seat.id, kind.ours());
        let mut devices = self.inner.devices.lock().unwrap_or_else(|e| e.into_inner());
        devices.remove(&seat.id);
        removed
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
    /// The owner's row stays tracked from its adoption. A person's row is tracked while its unit
    /// is, and has no proxy: its pads come from the broker, and the hook is a made seat's. A
    /// person's unit found down ends every presence on its row.
    fn reconcile(&self, seat: &Seat, status: &RuntimeStatus) {
        let kind = Own::of(seat);
        if kind == Own::Owner {
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
                let found = if kind.ours() {
                    accounts::require(seat)
                } else {
                    accounts::require_person(seat)
                };
                if !known && let Ok(passwd) = found {
                    self.track(&seat.id, Tracked::of(seat, &passwd));
                    if kind.ours() {
                        unit_hook(&self.inner, &seat.account, Some(passwd.uid));
                    }
                }
            }
            RuntimeState::Stopping => {}
            _ => {
                self.untrack(&seat.id);
                if kind.ours() {
                    unit_hook(&self.inner, &seat.account, None);
                }
                if kind == Own::Person {
                    end_presence(&self.inner, &seat.id);
                }
            }
        }
    }

    /// A root-owned copy of the certificate the seat's host serves, for the service to pin: from
    /// `~/.config/punktfunk`, or for a person's row from the row's own config directory. Either
    /// is a user's, which root doesn't own, so the copy is taken through directory handles and
    /// never waits on what is there ([`shared::copy_pin`]).
    fn host_config_dir(&self, seat: &Seat) -> Option<PathBuf> {
        let state = &self.inner.state;
        let dir = shared::pin_dir(state, &seat.id);
        let copied = match Own::of(seat) {
            Own::Owner => {
                shared::copy_pin(&accounts::require_owner(&seat.account).ok()?.home, &dir)
            }
            Own::Person => {
                accounts::require_person(seat).ok()?;
                shared::copy_person_pin(state, &seat.id, &dir)
            }
            Own::Seat => shared::copy_pin(&accounts::require(seat).ok()?.home, &dir),
        };
        if let Err(error) = copied {
            tracing::debug!(seat = %seat.id, %error, "seat host's certificate not read");
        }
        Some(dir)
    }

    fn doctor(&self, ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        let mut out = self.recheck();
        out.extend(fence::diagnostics());
        for seat in &ledger.seats {
            let found = accounts::lookup(&seat.account)?;
            let kind = Own::of(seat);
            let owned = found.as_ref().is_some_and(|found| match kind {
                Own::Owner => accounts::is_ordinary(found),
                Own::Person => seat.adopted_uid == Some(found.uid),
                Own::Seat => accounts::owned_by(found, &seat.id),
            });
            let (good, bad) = match kind {
                Own::Owner => (
                    "the owner's account exists and is an ordinary user",
                    "the owner's account is gone or isn't an ordinary user",
                ),
                Own::Person => (
                    "the account is the user this profile adopted",
                    "the account is gone or its name now belongs to another user",
                ),
                Own::Seat => (
                    "account marker matches the exact seat ID",
                    "account is absent or its marker does not match",
                ),
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
            if kind == Own::Person {
                let unreadable =
                    with_devices(&self.inner, &seat.id, |held, _| Ok(held.unreadable.clone()))?;
                if let Some(cause) = unreadable {
                    out.push(devices::unreadable_diagnostic(&seat.id, &cause));
                }
            }
        }
        out.extend(devices::pin_diagnostic(
            &self.inner.box_dir,
            &self.inner.state,
        ));
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

    fn install_flatpak(&self, id: &str) -> Result<Option<String>, BackendError> {
        flatpak::install(id)
    }

    fn remove_flatpak(&self, id: &str, purge: bool) -> Result<(), BackendError> {
        flatpak::remove(id, purge)
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
        self.wait_ready(seat, asked, false)?;
        Ok(RuntimeStatus::running())
    }

    /// A person's row up, on their own account. With them at the machine nothing starts: their
    /// login is theirs, and the headless session would sit beside it uninvited. Otherwise the
    /// unit brings up the headless session and a contract seat's host in it, which admits only
    /// the devices the row's list does. The account is checked first, and what is made is the
    /// row's own, never in the account. A cold start with `password` runs the login helper when
    /// [`login_with`] says so, and hands it the password through [`handover`] while this waits;
    /// a helper that ended before a session falls back to the plain start.
    fn start_person(
        &self,
        seat: &Seat,
        asked: u64,
        password: Option<&Secret>,
    ) -> Result<RuntimeStatus, BackendError> {
        let passwd = accounts::require_person(seat)?;
        if owner_at_desk(&seat.account) {
            return Err(err(
                "account_at_desk",
                format!(
                    "{} is signed in at the machine. Try again once they have signed out.",
                    seat.account
                ),
            ));
        }
        let (box_dir, state) = (&self.inner.box_dir, &self.inner.state);
        shared::ensure_person_dirs(state, &seat.id, &passwd)?;
        let reader = shared::Reader::person(passwd.uid, passwd.gid);
        person_trust(&self.inner, &seat.id, reader)?;
        shared::refresh_seat_library(state, &seat.id, reader)?;
        let mut written = None;
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
            let browser = session::browser(box_dir);
            session::write_unit_files(seat, &passwd.home, state, &token, None, None, &browser)?;
            let label =
                (self.inner.login_label.get().cloned()).unwrap_or_else(handover::Label::unbound);
            if label == handover::Label::Missing && password.is_some() {
                tracing::warn!(account = %seat.account,
                    cause = "the login socket doesn't carry PID 1's SELinux context",
                    "seat started without the wallet login");
            }
            let off = self.inner.helper_off.load(Ordering::SeqCst);
            let login = login_with(password, &label, off);
            let dropin = login
                .as_ref()
                .and_then(|(_, context)| login_dropin(context.as_deref()));
            let password = login.map(|(password, _)| password);
            if password.is_some() && dropin.is_none() {
                tracing::warn!(account = %seat.account, cause = "this binary's path can't go in a unit file",
                    "seat started without the wallet login");
            }
            written = dropin.is_some().then(|| LoginDropin(&seat.account));
            session::set_login(&seat.account, dropin.as_deref())?;
            self.stopped_since(asked)?;
            let unit = session::unit(&seat.account);
            let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
            let handover = &self.inner.handover;
            let offer = password
                .filter(|_| written.is_some())
                .map(|password| handover.offer(&seat.account, password, Instant::now()));
            run("systemctl_failed", "systemctl", &["start", &unit])?;
            if let Some(offer) = offer {
                offer.wait(|| session::state(&seat.account).is_ok_and(|s| s.live()));
            }
        }
        self.track(&seat.id, Tracked::of(seat, &passwd));
        let ready = self.wait_ready(seat, asked, written.is_some());
        drop(written);
        match ready {
            Err(error) if [LOGIN_FALLBACK, LOGIN_TRIPPED].contains(&error.code.as_str()) => {
                let tripped = error.code == LOGIN_TRIPPED;
                if tripped && !self.inner.helper_off.swap(true, Ordering::SeqCst) {
                    tracing::warn!("login helper off until the seat supervisor restarts");
                }
                tracing::warn!(account = %seat.account, cause = %error.message,
                    "seat started without the wallet login");
                self.start_plain(seat, &passwd, asked)
            }
            ready => ready.map(|()| RuntimeStatus::running()),
        }
    }

    /// `seat`'s unit started again as systemd's own login opens it, after the login helper ended
    /// before a session.
    fn start_plain(
        &self,
        seat: &Seat,
        passwd: &accounts::Passwd,
        asked: u64,
    ) -> Result<RuntimeStatus, BackendError> {
        self.stopped_since(asked)?;
        let unit = session::unit(&seat.account);
        let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
        run("systemctl_failed", "systemctl", &["start", &unit])?;
        self.track(&seat.id, Tracked::of(seat, passwd));
        self.wait_ready(seat, asked, false)?;
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

/// While a seat runs: refreshes its trust copy when the box's files change, the environment file
/// of a row on a person's own account, and, for a seat other than the owner's, the box's library,
/// which follows the owner's own first. A person's pairing list follows who its row admits, as
/// presence runs out. Ends a person's headless session when they sit down at the machine, and
/// repairs the games folder's ACL mask after a made seat stops and every [`ACL_EVERY`]. The
/// owner's row stays listed while its host is the owner's own, which reads the same copy. Ends
/// when the backend is dropped.
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
            let trust = match &tracked.guest {
                Some((seat, _)) if !seat.owner => person_trust(&inner, id, tracked.reader),
                _ => shared::refresh_trust(&inner.box_dir, &inner.state, id, tracked.reader),
            };
            let refreshed = trust
                .and_then(|_| match &tracked.guest {
                    Some((seat, passwd)) => rewrite_env(&inner, seat, passwd).map(|_| ()),
                    None => Ok(()),
                })
                .and_then(|()| match &tracked.guest {
                    Some((seat, _)) if seat.owner => Ok(false),
                    _ => shared::refresh_seat_library(&inner.state, id, tracked.reader),
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
            if let Some((seat, _)) = &tracked.guest {
                yield_to_the_desk(&inner, seat);
            }
        }
        let seat_up = seats.iter().any(|(_, tracked)| tracked.guest.is_none());
        let due = seat_up && acl_at.elapsed() >= ACL_EVERY;
        if inner.acl_dirty.swap(false, Ordering::SeqCst) || due {
            acl_at = Instant::now();
            if let Err(error) = shared::fix_acl(&inner.state) {
                tracing::warn!(%error, "games ACL mask not repaired");
            }
        }
    }
}

/// The one choice of how a person's cold start opens their session: through the login helper,
/// with the password the person proved the start with, so what their own login unlocks with it
/// unlocks here too; without one, or once the helper is `off`, as systemd's `PAMName=` opens
/// it. With SELinux on, the helper runs in PID 1's context, the one systemd's own PAM child
/// has: pam_selinux computes the session's context from the caller's, so every user gets what
/// today's start gives them, a confined one included. That needs the login socket to carry the
/// context too ([`handover::Label::Init`]); without it, today's start. A switch for the wallet
/// login goes here.
fn login_with<'a>(
    password: Option<&'a Secret>,
    label: &handover::Label,
    off: bool,
) -> Option<(&'a Secret, Option<String>)> {
    let context = match label {
        handover::Label::Off => None,
        handover::Label::Init(context) => Some(context.clone()),
        handover::Label::Missing => return None,
    };
    password
        .filter(|_| !off)
        .map(|password| (password, context))
}

/// Removes `.0`'s login drop-in when it drops: every way out of a start that wrote one, so a
/// restart or the next start runs the plain unit.
struct LoginDropin<'a>(&'a str);

impl Drop for LoginDropin<'_> {
    fn drop(&mut self) {
        if let Err(error) = session::set_login(self.0, None) {
            tracing::warn!(account = %self.0, %error, "login drop-in not removed");
        }
    }
}

/// The login drop-in for this binary in SELinux `context`, `None` when its path can't go in a
/// unit file. After an upgrade replaced the file, the link names the new one with a
/// ` (deleted)` suffix.
fn login_dropin(context: Option<&str>) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.to_str()?;
    let path = Path::new(path.strip_suffix(" (deleted)").unwrap_or(path));
    session::render_login(path, context)
}

/// The error of a start through the login helper that ended before a session: the start goes
/// on without it. The gate already admitted the person, so the helper never refuses.
const LOGIN_FALLBACK: &str = "login_fallback";
/// [`LOGIN_FALLBACK`] that also turns the helper off ([`login::Fallback::trips`]).
const LOGIN_TRIPPED: &str = "login_tripped";

/// Why `seat`'s start failed once its unit ended in `state`. With `login`, a login helper that
/// ended before a session says [`LOGIN_FALLBACK`] or [`LOGIN_TRIPPED`], with the operator's
/// cause.
fn unit_ended(seat: &Seat, state: &session::UnitState, login: bool) -> BackendError {
    let account = &seat.account;
    match login.then(|| login::fallback(state.ended)).flatten() {
        Some(fallback) if fallback.trips => err(LOGIN_TRIPPED, fallback.cause),
        Some(fallback) => err(LOGIN_FALLBACK, fallback.cause),
        None => err(
            "seat_exited",
            format!(
                "seat unit ended ({}); journalctl -u {} says why",
                state.result,
                session::unit(account)
            ),
        ),
    }
}

/// Writes the environment file of a row on a person's own account again with the token it holds,
/// as the box's Seat defaults stand now: Browser streaming reaches its host at the next start.
/// The owner's is handed to the owner, whose own host reads it; another person's stays root's.
/// `None` when the file holds no token, `Some(true)` when it changed.
fn rewrite_env(
    inner: &Inner,
    seat: &Seat,
    passwd: &accounts::Passwd,
) -> Result<Option<bool>, BackendError> {
    let Some(token) = session::env_token(&seat.account) else {
        return Ok(None);
    };
    let browser = session::browser(&inner.box_dir);
    let handed = seat.owner.then_some((passwd.uid, passwd.gid));
    let changed = session::write_env(seat, &passwd.home, &inner.state, &token, handed, &browser)?;
    if changed {
        tracing::info!(account = %seat.account, browser = browser.on,
            "environment file rewritten for its host's next start");
    }
    Ok(Some(changed))
}

/// The physical login wins: when the row's user has a session at the machine and the row's
/// headless unit is up, the unit ends. The owner's own host then takes the row; another person's
/// row stays down. Their session is never touched, so what they have on the monitor stays as it
/// is.
fn yield_to_the_desk(inner: &Inner, seat: &Seat) {
    let account = &seat.account;
    let live = session::state(account).is_ok_and(|s| s.live());
    if !Own::of(seat).yields_to_the_desk() || !live || !owner_at_desk(account) {
        return;
    }
    tracing::info!(%account, "user at the machine: ending the headless session");
    match stop_unit(inner, seat) {
        Ok(()) if seat.owner => user_host(account, "start"),
        Ok(()) => {}
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

    fn backend(box_dir: &Path, state: &Path) -> LinuxBackend {
        LinuxBackend::detached(box_dir, state)
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
            adopted_uid: None,
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
