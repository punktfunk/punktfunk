//! The Linux platform: one user, one logind session and one systemd unit per seat.
//!
//! The root daemon `punktfunk-seats` runs [`LinuxBackend`] behind [`socket`]. To start a seat it
//! writes the seat's environment file and a fresh management token, then starts
//! `punktfunk-seat@<user>.service`. That unit logs the user in through PAM and runs
//! `seat-session`: a headless compositor and an ordinary `punktfunk-host serve`. Everything the
//! seat runs is a child of the unit, so the unit's bind mounts reach it. A restarted daemon reads
//! each unit's state back from systemd.
//!
//! What the daemon may do to a seat's account depends on whose account it is ([`SeatAccount`]).
//! It made a made seat's account and deletes it with the seat. The owner's account and a
//! person's account belong to someone who may be logged in at the machine: there the daemon
//! starts and stops the seat's unit and changes nothing else. A person's seat starts only after
//! [`pam`] accepts their password. While the owner sits at the machine the owner's unit stays
//! down and the owner's own host serves the row.
//!
//! What must be up while a unit runs (copied files, lent pads, the uinput stand-in, the pad
//! broker, present devices) is started and stopped in one function, [`Inner::apply_change`], from
//! the table in [`resources`]. Every writer of a seat's copied files takes
//! [`copy_lock::CopyLock`]. [`node_owner`] tells udev which user owns each pad's device nodes.

mod accounts;
mod copy_lock;
mod devices;
pub mod door_switch;
#[cfg(test)]
mod fake;
mod firefox;
mod flatpak;
pub mod lent;
mod libraries;
pub mod login;
pub mod login_password;
mod logind;
pub mod loopback;
pub mod mount_share;
pub mod node_owner;
mod pam;
#[cfg(test)]
mod person_tests;
mod reach;
mod resources;
mod safe_open;
mod session;
mod shared;
mod shortcuts;
pub mod socket;
#[cfg(test)]
mod start_tests;
#[cfg(test)]
mod unlock_tests;

pub use pam::{run_check as pam_check, CHECK_VERB as PAM_CHECK_VERB};
pub use shared::ledger_root;
pub use socket::SOCKET_PATH;

use crate::backend::{Adopted, BackendError, PlatformBackend, PresentPad};
use crate::ipc::{Diagnostic, DiagnosticLevel, SeatingStatus};
use crate::model::{Ledger, RowKind, RuntimeState, RuntimeStatus, Seat, SeatId};
use crate::persistence::{LedgerStore, StoreError};
use crate::secret::Secret;
use crate::service::SeatService;
use crate::unlock::DeviceList;
use copy_lock::{CopyGuard, CopyLock};
use resources::{Action, Order, Resource, UnitChange};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// The supervisor's own state: the ledger, every seat's home and trust copy, the shared games
/// folder. Only root can change it. The door has its own directory, `/var/lib/punktfunk`, and
/// only reads files here.
pub const STATE_DIR: &str = "/var/lib/punktfunk-seats";

/// A seat's save exchange: `out` holds what its host exported for the owner's plugins, `in`
/// what the owner's host hands back. The same path inside the seat's unit.
pub fn saves_dir(id: &SeatId) -> PathBuf {
    shared::seat_dir(Path::new(STATE_DIR), id).join("saves")
}

/// The systemd unit that runs `account`'s seat. `*` names every seat's.
pub fn seat_unit(account: &str) -> String {
    session::unit(account)
}

/// The box owner `door-helper` recorded under [`STATE_DIR`]: the account the door is on for.
/// Readable by the door, written only by root.
pub fn recorded_owner() -> Option<String> {
    accounts::recorded_owner(Path::new(STATE_DIR))
}

/// Whether an off's hold stands under [`STATE_DIR`]: an off that began and didn't finish. An on
/// removes it before anything else and never sets it.
pub fn off_held() -> bool {
    Path::new(STATE_DIR).join(door_switch::HOLD).exists()
}

/// Whether `account` may switch the door `on` or off now, by the record root keeps
/// ([`door_switch::admit`]). The owner's host asks before it starts a switch; the error's code says
/// which rule refused it.
pub fn check_owner(account: &str, on: bool) -> Result<(), BackendError> {
    let recorded = recorded_owner();
    door_switch::admit(on, account, recorded.as_deref(), accounts::switchable).map(drop)
}

/// A seat has this long to bring up its compositor and open the host's management port.
const START_TIMEOUT: Duration = Duration::from_secs(90);
/// How often a running seat's trust copy is compared with the box's.
const TRUST_TICK: Duration = Duration::from_secs(2);
/// How long a request to sync the box's library now waits for the copy lock. The door gives up
/// on its request after two seconds (`socket.rs`), and a longer wait holds a socket connection.
const LIBRARY_SYNC_WAIT: Duration = Duration::from_millis(1500);
/// How often the games folder's ACL mask is repaired while a seat runs.
const ACL_EVERY: Duration = Duration::from_secs(600);
/// How often a made seat's Steam log is read for library folders Steam couldn't open.
const REACH_EVERY: Duration = Duration::from_secs(10);
/// How long `Seating` reuses the prerequisite checks it ran.
const CHECKS_FRESH: Duration = Duration::from_secs(10);

fn err(code: &str, message: impl Into<String>) -> BackendError {
    BackendError::new(code, message)
}

/// Runs `program` to its end with no stdin. A test installs the stand-in in `fake.rs`, which
/// records the call and answers without running anything.
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
    /// The door group's gid ([`socket::DOOR_GROUP`]), once looked up: the door reads the box's
    /// library through it.
    door_gid: OnceLock<u32>,
    /// A seat stopped since the ACL mask was last repaired.
    acl_dirty: AtomicBool,
    /// Stops asked of each seat so far. A start that sees its seat's count move gives up.
    stops: Mutex<HashMap<SeatId, u64>>,
    /// The hook the daemon binary set for its uinput stand-in ([`LinuxBackend::on_unit`]).
    hook: OnceLock<UnitHook>,
    /// The prerequisite checks `Seating` last answered with, and when they ran.
    checks: Mutex<Option<(Instant, Vec<Diagnostic>)>>,
    /// The lock every writer of a seat's copied files takes, and the device lists it guards.
    copy_lock: CopyLock,
    /// The password of a start through the login helper, while it waits for the helper.
    login_passwords: login_password::LoginPasswords,
    /// Set once a login helper tripped ([`LOGIN_TRIPPED`]): no helper until the supervisor
    /// restarts.
    helper_off: AtomicBool,
    /// What the login socket carries, once the binary bound it ([`LinuxBackend::login_socket`]).
    login_label: OnceLock<login_password::Label>,
    /// The desk pads lent to rows, and the hardware they name.
    lent: lent::Lent,
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

/// Called around a made seat's unit, for the daemon binary's uinput stand-in: with `Some(uid)`
/// before the unit starts and for a seat found running, with `None` once it stopped. Never called
/// for the owner's row or a person's row. Every call must be idempotent.
pub type UnitHook = Box<dyn Fn(&str, Option<u32>) + Send + Sync>;

fn unit_hook(inner: &Inner, account: &str, uid: Option<u32>) {
    if let Some(hook) = inner.hook.get() {
        hook(account, uid);
    }
}

/// Whose account a seat runs on, which decides what the supervisor may do to that account. The
/// supervisor made a made seat's account and deletes it with the seat. The owner's account and a
/// person's account belong to someone who may be logged in at the machine: there the supervisor
/// stops the seat's unit and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SeatAccount {
    Made,
    Owner,
    Person,
}

impl SeatAccount {
    /// The account kind of a ledger row. A Windows person's row counts as a person's here;
    /// [`accounts::require_person`] refuses it by name.
    fn of(kind: &RowKind) -> Self {
        if kind.is_owner() {
            Self::Owner
        } else if kind.is_person() {
            Self::Person
        } else {
            Self::Made
        }
    }

    /// Whether the supervisor made the account, and so may close it, delete it and end every
    /// process it runs.
    fn made(self) -> bool {
        self == Self::Made
    }
}

/// What the sync thread and the pad broker need to know about a running seat.
#[derive(Clone, Debug)]
struct Tracked {
    kind: SeatAccount,
    /// Who reads the seat's trust copy.
    reader: shared::Reader,
    /// The row and the account, for a seat on the owner's or a person's own account. The sync
    /// thread stops such a seat's unit when its user logs in at the machine, and keeps its
    /// environment file in step with the box's Seat defaults.
    own_account: Option<(Seat, accounts::Passwd)>,
    /// The seat's user: whom the pad broker serves, and whose name its pads are stamped with.
    uid: u32,
    account: String,
}

impl Tracked {
    fn of(seat: &Seat, passwd: &accounts::Passwd) -> Self {
        let kind = SeatAccount::of(&seat.kind);
        let (reader, own_account) = if kind.made() {
            (shared::Reader::group(passwd.gid), None)
        } else {
            (
                shared::Reader::person(passwd.uid, passwd.gid),
                Some((seat.clone(), passwd.clone())),
            )
        };
        Self {
            kind,
            reader,
            own_account,
            uid: passwd.uid,
            account: seat.account.clone(),
        }
    }
}

/// Whether an account that no ledger row names was left by a create that stopped halfway: the
/// account's seat directory exists under `state`, and its seat never started.
///
/// A create makes the seat directory before the account. So an account with no seat directory
/// belongs to a state directory that isn't mounted, or was emptied, where every seat would look
/// abandoned. Such an account is kept: it is the last pointer to that seat's files.
///
/// A seat that has started has a token directory. One that can't be read counts as there.
fn reapable(state: &Path, id: &SeatId) -> bool {
    let here = matches!(shared::seat_dir(state, id).try_exists(), Ok(true));
    let started = !matches!(shared::host_dir(state, id).try_exists(), Ok(false));
    here && !started
}

/// The wall clock in unix seconds, as presence is kept.
fn unix_now() -> i64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    now.map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// `f` over `id`'s held device list, loaded on first use, with the lists' lock held throughout.
fn with_devices<T>(
    inner: &Inner,
    guard: &mut CopyGuard<'_>,
    id: &SeatId,
    f: impl FnOnce(&mut devices::Held, i64) -> Result<T, BackendError>,
) -> Result<T, BackendError> {
    let now = unix_now();
    let mut all = guard.devices();
    let held = all
        .entry(id.clone())
        .or_insert_with(|| devices::load(&inner.state, id, now));
    f(held, now)
}

/// `id`'s list after `edit`, from the list with its expired presence dropped. A change is saved,
/// then the trust copy's pairing list follows who is admitted.
fn edit_devices(
    inner: &Inner,
    guard: &mut CopyGuard<'_>,
    id: &SeatId,
    edit: &mut dyn FnMut(&mut DeviceList, i64),
) -> Result<DeviceList, BackendError> {
    with_devices(inner, guard, id, |held, now| {
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
fn end_presence(inner: &Inner, guard: &mut CopyGuard<'_>, id: &SeatId) {
    if let Err(error) = edit_devices(inner, guard, id, &mut |list, _| list.end_presence()) {
        tracing::warn!(seat = %id, %error, "presence not ended");
    }
}

/// A person's trust copy for `reader`, its pairing list holding only the devices admitted now.
fn person_trust(
    inner: &Inner,
    guard: &mut CopyGuard<'_>,
    id: &SeatId,
    reader: shared::Reader,
) -> Result<bool, BackendError> {
    with_devices(inner, guard, id, |held, now| {
        let mut next = held.list.clone();
        if next.expire(now) {
            held.commit(&inner.state, id, next.clone())?;
        }
        let admitted = next.admitted();
        shared::refresh_person_trust(&inner.box_dir, &inner.state, id, reader, &admitted)
    })
}

/// Stops `seat`'s unit if it is up, for a stop or a delete (`change`). First, what was started
/// after the unit is stopped ([`Order::AfterUnit`]), so the pad broker refuses the seat's user
/// while the unit ends. Then the unit stops. For a made seat every process of its user is ended,
/// so nothing it started under its own systemd manager survives. Last, what was started before
/// the unit is stopped. On the owner's or a person's account nothing but the unit is stopped and
/// no process of theirs is signalled. A unit that never started is fine. `held`: the copy lock
/// that a delete holds across the stop.
fn stop_unit(
    inner: &Inner,
    seat: &Seat,
    change: UnitChange,
    mut held: Option<&mut CopyGuard<'_>>,
) -> Result<(), BackendError> {
    let account = &seat.account;
    let unit = session::unit(account);
    let up = session::state(account)?.active != "inactive";
    inner.apply_change(seat, None, change, Order::AfterUnit, held.as_deref_mut())?;
    if up {
        run("systemctl_failed", "systemctl", &["stop", &unit])?;
    }
    // A failed unit keeps its state until reset, and would read as failed after a stop.
    let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
    inner.acl_dirty.store(true, Ordering::SeqCst);
    // Only the seat's own user is ended; a stranger of the same name keeps its sessions.
    let ended = match SeatAccount::of(&seat.kind) {
        SeatAccount::Made => {
            accounts::refuse_foreign(account, &seat.id).and_then(|()| accounts::quiesce(account))
        }
        SeatAccount::Owner | SeatAccount::Person => Ok(()),
    };
    inner.apply_change(seat, None, change, Order::BeforeUnit, held)?;
    ended
}

impl Inner {
    /// Starts or stops the resources of one `order` around `seat`'s unit, as `change` asks
    /// ([`resources::actions`]). This is the only place such a resource starts or stops. `user` is
    /// the seat's account, which every start needs. An action that needs the copy lock runs under
    /// `held`, or takes the lock itself. Returns `true` when the owner's environment file changed.
    fn apply_change(
        &self,
        seat: &Seat,
        user: Option<&accounts::Passwd>,
        change: UnitChange,
        order: Order,
        mut held: Option<&mut CopyGuard<'_>>,
    ) -> Result<bool, BackendError> {
        let kind = SeatAccount::of(&seat.kind);
        let user = || {
            user.ok_or_else(|| err("account_missing", format!("change {}'s unit", seat.account)))
        };
        let mut fresh = false;
        for (resource, action) in resources::actions(kind, order, change) {
            match (resource, action) {
                (Resource::Copies, Action::Start) => {
                    fresh |= self.copies(seat, kind, user()?, held.as_deref_mut())?;
                }
                (Resource::LentPads, Action::SetPadsLive(live)) => {
                    self.lent
                        .set_live(seat, live, change == UnitChange::Starting);
                }
                (Resource::LentPads, Action::Stop) => self.lent.release(seat),
                (Resource::UinputProxy, Action::Start) => {
                    unit_hook(self, &seat.account, Some(user()?.uid))
                }
                (Resource::UinputProxy, Action::Stop) => unit_hook(self, &seat.account, None),
                (Resource::PadBroker, Action::Start) => {
                    let tracked = Tracked::of(seat, user()?);
                    let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
                    running.insert(seat.id.clone(), tracked);
                }
                (Resource::PadBroker, Action::Stop) => {
                    let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
                    running.remove(&seat.id);
                }
                (Resource::Presence, Action::Stop) => {
                    self.copy_lock.with(held.as_deref_mut(), |guard| {
                        end_presence(self, guard, &seat.id);
                    });
                }
                // The test `the_action_table` holds that the table asks for no other pair.
                _ => {}
            }
        }
        Ok(fresh)
    }

    /// Writes `seat`'s copies for the reader of `passwd`, under the copy lock. Returns `true` when
    /// the owner's environment file changed.
    ///
    /// The owner's row is written only for the recorded owner account. It gets its trust copy, the
    /// box's library copied from the owner's home, and its environment file. An environment file
    /// with no token gets a fresh one. A host that is up keeps the token it started with.
    fn copies(
        &self,
        seat: &Seat,
        kind: SeatAccount,
        passwd: &accounts::Passwd,
        held: Option<&mut CopyGuard<'_>>,
    ) -> Result<bool, BackendError> {
        let (box_dir, state, id) = (&self.box_dir, &self.state, &seat.id);
        let reader = Tracked::of(seat, passwd).reader;
        if kind == SeatAccount::Owner {
            accounts::require_recorded(state, &seat.account)?;
        }
        self.copy_lock.with(held, |guard| match kind {
            SeatAccount::Made => {
                shared::refresh_trust(box_dir, state, id, reader)?;
                shared::refresh_seat_library(box_dir, state, id, reader)?;
                Ok(false)
            }
            SeatAccount::Person => {
                person_trust(self, guard, id, reader)?;
                shared::refresh_seat_library(box_dir, state, id, reader)?;
                Ok(false)
            }
            SeatAccount::Owner => {
                shared::refresh_owner_trust(box_dir, state, id, reader)?;
                *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = Some(passwd.clone());
                if let Err(error) = refresh_box_library(self) {
                    tracing::warn!(account = %seat.account, %error, "box library not copied");
                }
                if let Some(changed) = rewrite_env(self, seat, SeatAccount::Owner, passwd)? {
                    return Ok(changed);
                }
                self.write_owner_files(seat, passwd, &session::mint_token())?;
                Ok(true)
            }
        })
    }

    fn write_owner_files(
        &self,
        seat: &Seat,
        passwd: &accounts::Passwd,
        token: &str,
    ) -> Result<(), BackendError> {
        self.write_token(seat, token)?;
        let browser = session::browser(&self.box_dir);
        session::write_unit_files(
            seat,
            SeatAccount::Owner,
            passwd,
            &self.state,
            token,
            None,
            &browser,
        )
    }

    /// Writes `token` as the seat's token file, for the box's client to present: the `punktfunk`
    /// user's when there is one.
    fn write_token(&self, seat: &Seat, token: &str) -> Result<(), BackendError> {
        let door = accounts::lookup("punktfunk")?;
        shared::write_host_token(
            &self.state,
            &seat.id,
            &session::token_line(token),
            shared::token_owner(door.as_ref()),
        )
    }
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

/// Copies the owner's library into the box's library copy, once the owner's row is known.
/// Returns whether anything changed.
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
            stops: Mutex::default(),
            hook: OnceLock::new(),
            checks: Mutex::new(None),
            copy_lock: CopyLock::default(),
            login_passwords: login_password::LoginPasswords::default(),
            helper_off: AtomicBool::new(false),
            login_label: OnceLock::new(),
            lent: lent::Lent::under(Path::new("/")),
        });
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("seats-sync".into())
            .spawn(move || sync_loop(weak))
            .map_err(|e| err("thread", format!("start the trust thread: {e}")))?;
        Ok(Self { inner })
    }

    /// A backend over `box_dir` and `state`, with no sync thread. Its sysfs, udev
    /// database and lend records lie under `state`.
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
                stops: Mutex::default(),
                hook: OnceLock::new(),
                checks: Mutex::new(None),
                copy_lock: CopyLock::default(),
                login_passwords: login_password::LoginPasswords::default(),
                helper_off: AtomicBool::new(false),
                login_label: OnceLock::new(),
                lent: lent::Lent::under(state),
            }),
        }
    }

    /// Records what the login socket the binary bound carries ([`login_password::bind`]), which
    /// decides whether a start may go through the login helper. The first call wins.
    pub fn login_socket(&self, label: login_password::Label) {
        let _ = self.inner.login_label.set(label);
    }

    /// Sets the hook that drives the daemon binary's uinput stand-in around each made seat's
    /// unit. The first call wins.
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
        for seat in ledger
            .seats
            .iter()
            .filter(|s| SeatAccount::of(&s.kind) == SeatAccount::Person)
        {
            let mut guard = self.inner.copy_lock.lock();
            let written = with_devices(&self.inner, &mut guard, &seat.id, |held, _| {
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
    /// stopped after `useradd` left, which refuses every later create of its name. An account
    /// that isn't [`reapable`] is kept, and so is one whose unit is up or can't be read.
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
            let idle = session::state(account).is_ok_and(|state| !state.live());
            if !reapable(&self.inner.state, &id) || !idle {
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
        stop_unit(&self.inner, seat, UnitChange::Stopped, None)
    }

    /// Writes the owner's files again at supervisor start: `/run` is empty after a boot, and the
    /// owner's own host would otherwise start on the box's ports, in the door's way. A host
    /// already up restarts onto files that changed; at boot none is up yet. The row is tracked, as
    /// [`PlatformBackend::adopt`] tracks it, for as long as it exists.
    pub fn prepare_owners(&self, ledger: &Ledger) {
        for seat in ledger.seats.iter().filter(|s| s.kind.is_owner()) {
            let prepared = accounts::require_owner(&seat.account).and_then(|passwd| {
                self.inner.apply_change(
                    seat,
                    Some(&passwd),
                    UnitChange::OwnerKnown,
                    Order::BeforeUnit,
                    None,
                )
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
        self.serving(seat) && (!seat.kind.is_owner() || session::ready_path(&seat.account).exists())
    }

    /// How many stops were asked of `id` so far.
    fn stops(&self, id: &SeatId) -> u64 {
        let stops = self.inner.stops.lock().unwrap_or_else(|e| e.into_inner());
        stops.get(id).copied().unwrap_or(0)
    }

    /// Counts a stop of `id`: a start of it in flight gives up.
    fn ask_stop(&self, id: &SeatId) {
        let mut stops = self.inner.stops.lock().unwrap_or_else(|e| e.into_inner());
        *stops.entry(id.clone()).or_default() += 1;
    }

    /// Fails when a stop of `id` has been asked since `asked`, the count its start began at.
    fn stopped_since(&self, id: &SeatId, asked: u64) -> Result<(), BackendError> {
        if self.stops(id) == asked {
            return Ok(());
        }
        Err(err("seat_stopped", "the seat was stopped while it started"))
    }

    /// Waits for the unit to be active and the host's management port to answer. A unit that
    /// ends first, a state that can't be read, a stop or a timeout stops the seat and fails the
    /// start. `login`: the unit runs the login helper, whose end says why ([`unit_ended`]).
    fn wait_ready(&self, seat: &Seat, asked: u64, login: bool) -> Result<(), BackendError> {
        let deadline = Instant::now() + START_TIMEOUT;
        let failure = loop {
            if let Err(stopped) = self.stopped_since(&seat.id, asked) {
                break stopped;
            }
            let state = match session::state(&seat.account) {
                Ok(state) => state,
                Err(error) => break error,
            };
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
        // `ids` are the message ids of `good` and `bad`.
        let mut check = |ok: bool, code: &str, ids: [&str; 2], good: &str, bad: &str| {
            out.push(if ok {
                Diagnostic::info(code, good).worded(ids[0])
            } else {
                Diagnostic::error(code, bad).worded(ids[1])
            });
        };
        check(
            Path::new("/run/systemd/system").is_dir(),
            "systemd",
            ["seatcheck_systemd_ok", "seatcheck_systemd_missing"],
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
            ["seatcheck_logind_ok", "seatcheck_logind_missing"],
            "systemd-logind is running",
            "systemd-logind isn't running, so a seat can't get a session.",
        );
        let group = |name: &str| accounts::group_gid(name).ok().flatten().is_some();
        check(
            group(socket::DOOR_GROUP),
            "door_group",
            ["seatcheck_door_group_ok", "seatcheck_door_group_missing"],
            "The punktfunk-door group exists",
            "The punktfunk-door group doesn't exist; reinstall the package that creates it.",
        );
        check(
            group("render"),
            "render_group",
            [
                "seatcheck_render_group_ok",
                "seatcheck_render_group_missing",
            ],
            "The render group exists",
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
            ["seatcheck_render_node_ok", "seatcheck_render_node_missing"],
            "A GPU render node is present",
            "No graphics card render node was found, so seats can't stream.",
        );
        check(
            which("kwin_wayland").is_some() || which("gamescope").is_some(),
            "compositor",
            ["seatcheck_compositor_ok", "seatcheck_compositor_missing"],
            "A seat compositor is installed",
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
            ["seatcheck_tools_ok", "seatcheck_tools_missing"],
            "The account and ACL tools are installed",
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
            ["seatcheck_seat_unit_ok", "seatcheck_seat_unit_missing"],
            "The seat unit and its scripts are installed",
            "The seat unit or its scripts are missing; reinstall the host package.",
        );
        let box_dir = &self.inner.box_dir;
        check(
            box_dir.join("native-cert.pem").is_file() && box_dir.join("native-key.pem").is_file(),
            "box_identity",
            [
                "seatcheck_box_identity_ok",
                "seatcheck_box_identity_missing",
            ],
            "The box has a host identity",
            "The box has no host identity yet; start its host once.",
        );
        if let Some(row) = out.iter_mut().find(|row| row.code == "tools") {
            row.params.insert("tools".into(), missing.join(", "));
        }
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
        self.inner.apply_change(
            seat,
            Some(&passwd),
            UnitChange::OwnerKnown,
            Order::BeforeUnit,
            None,
        )?;
        Ok(())
    }

    /// The box owner's uid, known once the owner's row is prepared. It is the uid of the account
    /// that `door-helper` recorded. The socket serves this uid as its one peer besides root and
    /// the door, for Flatpak changes only. The ledger keeps the same uid on the owner's row.
    fn owner_uid(&self) -> Option<u32> {
        self.owner().map(|owner| owner.uid)
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
    /// to the recorded uid, then the row's directory is made. Nothing is made in the account, so
    /// release has nothing of it to take back, and a refusal leaves nothing behind.
    fn adopt_account(&self, seat: &Seat, _password: &Secret) -> Result<Adopted, BackendError> {
        let passwd = accounts::require_person(seat)?;
        let made = shared::ensure_person_dirs(&self.inner.state, &seat.id, &passwd);
        if made.is_err() {
            let _ = shared::remove_seat_dirs(&self.inner.state, &seat.id);
        }
        made.map(|()| Adopted::default())
    }

    fn account_owned(&self, seat: &Seat) -> Result<(), BackendError> {
        accounts::require_person(seat).map(drop)
    }

    fn devices(
        &self,
        seat: &Seat,
        edit: &mut dyn FnMut(&mut DeviceList, i64),
    ) -> Result<DeviceList, BackendError> {
        if SeatAccount::of(&seat.kind) != SeatAccount::Person {
            return Err(err("not_a_person", "only a person's row lists devices"));
        }
        let mut guard = self.inner.copy_lock.lock();
        edit_devices(&self.inner, &mut guard, &seat.id, edit)
    }

    fn box_fingerprint(&self) -> Result<[u8; 32], BackendError> {
        devices::box_fingerprint(&self.inner.box_dir, &self.inner.state)
    }

    fn start(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        let asked = self.stops(&seat.id);
        self.start_row(seat, asked, None)
    }

    /// A person's row, started with the password they proved the start with: [`login_with`]
    /// decides whether that password opens their session.
    fn start_with_password(
        &self,
        seat: &Seat,
        password: &Secret,
    ) -> Result<RuntimeStatus, BackendError> {
        if SeatAccount::of(&seat.kind) != SeatAccount::Person {
            return self.start(seat);
        }
        let asked = self.stops(&seat.id);
        self.start_row(seat, asked, Some(password))
    }

    /// Ends the headless session, then levels the row's lent pads with what still runs: parked,
    /// unless the owner's own login hosts the row. A host in that login stays: it isn't the
    /// unit's. The status is read for the pads alone.
    fn stop(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        self.ask_stop(&seat.id);
        self.stop_unit(seat)?;
        if !seat.lent.is_empty() {
            let live = self.status(seat).is_ok_and(|s| lent::runs(&s.state));
            self.inner.apply_change(
                seat,
                None,
                UnitChange::Observed { running: live },
                Order::BeforeUnit,
                None,
            )?;
        }
        Ok(RuntimeStatus::stopped())
    }

    /// The owner's row takes the box's library with it: it was the door's, and the door goes too.
    /// A row the owner's new row has replaced leaves the library, which is the new one's. A
    /// person's row releases the account: its unit stops and what the row made goes, its device
    /// list included, and nothing of the account does. Once its files are gone, the row's lent
    /// pads are the desk's again.
    fn remove(&self, seat: &Seat) -> Result<(), BackendError> {
        let mut guard = self.inner.copy_lock.lock();
        let kind = SeatAccount::of(&seat.kind);
        // Before the unit is stopped: ending a stranger's sessions is not removing a seat.
        if kind.made() {
            accounts::refuse_foreign(&seat.account, &seat.id)?;
        }
        stop_unit(&self.inner, seat, UnitChange::Deleting, Some(&mut guard))?;
        if seat.kind.is_owner() {
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
        let removed = self.remove_files(&seat.account, &seat.id, kind.made());
        guard.devices().remove(&seat.id);
        if removed.is_ok() {
            self.inner.apply_change(
                seat,
                None,
                UnitChange::Deleted,
                Order::BeforeUnit,
                Some(&mut guard),
            )?;
        }
        removed
    }

    fn status(&self, seat: &Seat) -> Result<RuntimeStatus, BackendError> {
        let state = session::state(&seat.account)?;
        if seat.kind.is_owner() {
            return Ok(self.owner_status(seat, &state));
        }
        Ok(state.runtime())
    }

    /// Brings a seat's resources in line with the unit state the service just read. Its lent
    /// pads go to it while its unit runs or starts, and are parked otherwise. A unit found up
    /// whose user the pad broker doesn't serve yet gets [`UnitChange::FoundUp`]. A unit found
    /// down gets [`UnitChange::Stopped`]. A unit that is stopping keeps its resources: a restart
    /// rebuilds its namespace on the same uinput stand-in. The owner's row is tracked from its
    /// adoption, so no account is looked up for it.
    fn reconcile(&self, seat: &Seat, status: &RuntimeStatus) {
        let live = lent::runs(&status.state);
        let _ = self.inner.apply_change(
            seat,
            None,
            UnitChange::Observed { running: live },
            Order::BeforeUnit,
            None,
        );
        let kind = SeatAccount::of(&seat.kind);
        if kind == SeatAccount::Owner {
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
                let found = if kind.made() {
                    accounts::require(seat)
                } else {
                    accounts::require_person(seat)
                };
                if !known && let Ok(passwd) = found {
                    for order in [Order::AfterUnit, Order::BeforeUnit] {
                        let _ = self.inner.apply_change(
                            seat,
                            Some(&passwd),
                            UnitChange::FoundUp,
                            order,
                            None,
                        );
                    }
                }
            }
            RuntimeState::Stopping => {}
            _ => {
                for order in [Order::AfterUnit, Order::BeforeUnit] {
                    let _ = self
                        .inner
                        .apply_change(seat, None, UnitChange::Stopped, order, None);
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
        let copied = match SeatAccount::of(&seat.kind) {
            SeatAccount::Owner => {
                shared::copy_pin(&accounts::require_owner(&seat.account).ok()?.home, &dir)
            }
            SeatAccount::Person => {
                accounts::require_person(seat).ok()?;
                shared::copy_person_pin(state, &seat.id, &dir)
            }
            SeatAccount::Made => shared::copy_pin(&accounts::require(seat).ok()?.home, &dir),
        };
        if let Err(error) = copied {
            tracing::debug!(seat = %seat.id, %error, "seat host's certificate not read");
        }
        Some(dir)
    }

    fn doctor(&self, ledger: &Ledger) -> Result<Vec<Diagnostic>, BackendError> {
        let mut out = self.recheck();
        out.extend(node_owner::diagnostics());
        for seat in &ledger.seats {
            let found = accounts::lookup(&seat.account)?;
            let kind = SeatAccount::of(&seat.kind);
            let owned = found.as_ref().is_some_and(|found| match kind {
                SeatAccount::Owner => accounts::is_ordinary(found),
                SeatAccount::Person => seat.kind.uid() == Some(found.uid),
                SeatAccount::Made => accounts::owned_by(found, &seat.id),
            });
            let (good, bad) = match kind {
                SeatAccount::Owner => (
                    "the owner's account exists and is an ordinary user",
                    "the owner's account is gone or isn't an ordinary user",
                ),
                SeatAccount::Person => (
                    "the account is the user this profile adopted",
                    "the account is gone or its name now belongs to another user",
                ),
                SeatAccount::Made => (
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
                seat_id: Some(seat.id.clone()),
                ..Diagnostic::info("account_marker", if owned { good } else { bad })
            });
            let state = session::state(&seat.account)?;
            tracing::debug!(
                seat = %seat.id,
                active = %state.active,
                sub = %state.sub,
                result = %state.result,
                "seat unit state"
            );
            let runtime = state.runtime().state;
            let (id, message) = match runtime {
                RuntimeState::Running => ("seatcheck_child_state_running", "Running."),
                RuntimeState::Starting => ("seatcheck_child_state_starting", "Starting…"),
                RuntimeState::Stopping => ("seatcheck_child_state_stopping", "Stopping…"),
                RuntimeState::Failed => ("seatline_not_started", "This seat didn't start."),
                RuntimeState::Stopped | RuntimeState::Unknown => (
                    "seatcheck_child_state_stopped",
                    "Stopped. It starts when its player connects.",
                ),
            };
            out.push(Diagnostic {
                level: if runtime == RuntimeState::Failed {
                    DiagnosticLevel::Error
                } else {
                    DiagnosticLevel::Info
                },
                seat_id: Some(seat.id.clone()),
                ..Diagnostic::info("unit_state", message).worded(id)
            });
            let running = kind == SeatAccount::Made && runtime == RuntimeState::Running;
            let missed = (found.as_ref().filter(|_| running))
                .and_then(|passwd| reach::missed(&passwd.home))
                .filter(|missed| reach::this_run(missed, &session::unit(&seat.account)));
            if let Some(missed) = missed {
                let (id, message) = if missed.listed() {
                    (
                        "seatcheck_steam_libraries_unreachable",
                        "Its Steam can't open the host's game libraries, so their games are \
                         missing here. Restart the seat, and report a bug if they're still missing.",
                    )
                } else {
                    (
                        "seatcheck_steam_sandboxed",
                        "Its Steam is a Flatpak or Snap app, which doesn't see the host's game \
                         libraries, so their games are missing here. Install the distribution's \
                         Steam package on the host to share them.",
                    )
                };
                out.push(Diagnostic {
                    seat_id: Some(seat.id.clone()),
                    ..Diagnostic::warning("steam_libraries", message).worded(id)
                });
            }
            if kind == SeatAccount::Person {
                let mut guard = self.inner.copy_lock.lock();
                let unreadable = with_devices(&self.inner, &mut guard, &seat.id, |held, _| {
                    Ok(held.unreadable.clone())
                })?;
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

    /// The checks are the last ones run within [`CHECKS_FRESH`], so a frequent caller doesn't fork
    /// `systemctl` and walk `PATH` each time. `enabled` is constant. Doctor and Enable run them
    /// afresh.
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

    fn desk_pads(&self, ledger: &Ledger) -> Result<Vec<PresentPad>, BackendError> {
        Ok(self.inner.lent.desk_pads(ledger))
    }

    fn lendable_pad(&self, key: &str, ledger: &Ledger) -> Result<PresentPad, BackendError> {
        self.inner.lent.lendable_pad(key, ledger)
    }

    fn settle_pads(&self, ledger: &Ledger, keys: &[String]) -> Result<(), BackendError> {
        self.inner.lent.settle(ledger, keys)
    }

    fn sync_library(&self) -> Result<bool, BackendError> {
        let Some(_guard) = self.inner.copy_lock.lock_within(LIBRARY_SYNC_WAIT) else {
            return Err(err(
                "busy",
                "another change holds the copies; the next tick levels the library",
            ));
        };
        refresh_box_library(&self.inner)
    }
}

impl LinuxBackend {
    /// A row's unit up: its account ready, what runs outside the unit raised, the unit, then what
    /// runs inside it, then the wait for its host. `asked` is the stop count the start began at.
    /// What is made for a person's row is the row's own, never in the account. The login drop-in
    /// [`Self::bring_up`] wrote stays while the start waits; a helper that ended before a session
    /// falls back to the plain start.
    ///
    /// When the row's user sits at the machine, before the unit starts or once the wait failed,
    /// [`Self::start_at_desk`] ends the start. The sync thread stops the unit for that login
    /// without counting a stop, so the wait reads the unit's end as a failure.
    fn start_row(
        &self,
        seat: &Seat,
        asked: u64,
        password: Option<&Secret>,
    ) -> Result<RuntimeStatus, BackendError> {
        let kind = SeatAccount::of(&seat.kind);
        let state = &self.inner.state;
        let passwd = match kind {
            SeatAccount::Made => self.ready_made_user(seat)?,
            SeatAccount::Owner => accounts::require_owner(&seat.account)?,
            SeatAccount::Person => {
                let passwd = accounts::require_person(seat)?;
                if owner_at_desk(&seat.account) {
                    return self.start_at_desk(seat, asked);
                }
                shared::ensure_person_dirs(state, &seat.id, &passwd)?;
                passwd
            }
        };
        self.inner.apply_change(
            seat,
            Some(&passwd),
            UnitChange::Starting,
            Order::BeforeUnit,
            None,
        )?;
        if kind == SeatAccount::Owner && owner_at_desk(&seat.account) {
            return self.start_at_desk(seat, asked);
        }
        let written = self.bring_up(seat, kind, &passwd, password, asked)?;
        self.inner.apply_change(
            seat,
            Some(&passwd),
            UnitChange::Starting,
            Order::AfterUnit,
            None,
        )?;
        let ready = self.wait_ready(seat, asked, written.is_some());
        drop(written);
        let started = match ready {
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
        };
        match started {
            Err(_) if seat.kind.yields_to_the_desk() && owner_at_desk(&seat.account) => {
                self.start_at_desk(seat, asked)
            }
            started => started,
        }
    }

    /// A start while the row's user sits at the machine. The owner's own login hosts the owner's
    /// row: the headless unit stays down and the owner's user host is started. A person is
    /// refused: their login is theirs, and a headless session would sit beside it uninvited.
    fn start_at_desk(&self, seat: &Seat, asked: u64) -> Result<RuntimeStatus, BackendError> {
        if !seat.kind.is_owner() {
            return Err(err(
                "account_at_desk",
                format!(
                    "{} is signed in at the machine. Try again once they have signed out.",
                    seat.account
                ),
            )
            .said("account_at_desk")
            .param("account", &seat.account));
        }
        user_host(&seat.account, "start");
        self.wait_port(seat, asked)?;
        Ok(RuntimeStatus::running())
    }

    /// A made seat's user ready for its unit: a group of its own, its home closed, its groups,
    /// the games folder, its seat folders and Steam links.
    fn ready_made_user(&self, seat: &Seat) -> Result<accounts::Passwd, BackendError> {
        let state = &self.inner.state;
        let mut passwd = accounts::require(seat)?;
        // The trust copy goes and is made again for the new group.
        match accounts::own_group(&passwd) {
            Ok(false) => {}
            Ok(true) => {
                tracing::info!(account = %passwd.name, "seat user moved to a group of its own");
                let _ = std::fs::remove_dir_all(shared::trust_dir(state, &seat.id));
                passwd = accounts::require(seat)?;
            }
            Err(error) => {
                tracing::warn!(account = %passwd.name, %error, "seat user's own group not made");
            }
        }
        accounts::close_home(&passwd.home)?;
        let games = accounts::ensure_group(accounts::GAMES_GROUP)?;
        if let Err(error) = accounts::join_groups(&passwd.name) {
            tracing::warn!(account = %passwd.name, %error, "seat user's groups not restored");
        }
        shared::ensure_games(state, games)?;
        shared::ensure_seat_dirs(state, &seat.id, &passwd, self.owner_uid())?;
        if let Err(error) = shared::link_steam(&passwd) {
            tracing::warn!(account = %passwd.name, %error, "seat's steam links not made");
        }
        Ok(passwd)
    }

    /// `seat`'s unit up from cold, a row of `kind` on `passwd`'s account: a fresh token, the
    /// unit's files under the copy lock, then the unit. A unit already up keeps its token, which its
    /// host read when it started, and nothing here runs. `password` is a person's, for
    /// [`Self::person_login`]; the login drop-in's guard comes back for the caller to hold while
    /// it waits.
    fn bring_up<'s>(
        &self,
        seat: &'s Seat,
        kind: SeatAccount,
        passwd: &accounts::Passwd,
        password: Option<&Secret>,
        asked: u64,
    ) -> Result<Option<LoginDropin<'s>>, BackendError> {
        if session::state(&seat.account)?.live() {
            return Ok(None);
        }
        // The owner's own host may hold the row's port until the unit's runner stands it down.
        if kind != SeatAccount::Owner && session::port_taken(seat.mgmt_port) {
            return Err(err(
                "port_in_use",
                format!("management port {} is already in use", seat.mgmt_port),
            ));
        }
        let token = session::mint_token();
        self.inner.write_token(seat, &token)?;
        let shares = if kind.made() {
            self.share_libraries(seat, passwd)
        } else {
            None
        };
        let (state, browser) = (&self.inner.state, session::browser(&self.inner.box_dir));
        let shown = shares.as_ref();
        let guard = self.inner.copy_lock.lock();
        session::write_unit_files(seat, kind, passwd, state, &token, shown, &browser)?;
        drop(guard);
        let (written, password) = match kind {
            SeatAccount::Person => self.person_login(seat, password)?,
            SeatAccount::Owner | SeatAccount::Made => (None, None),
        };
        self.stopped_since(&seat.id, asked)?;
        let unit = session::unit(&seat.account);
        if kind.made() {
            // A manager already up (the last run's, or a linger's from boot) lacks this start's
            // groups and drop-in; the new session would join it and hand its apps neither.
            let manager = format!("user@{}.service", passwd.uid);
            let _ = run("systemctl_failed", "systemctl", &["stop", &manager]);
        }
        let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
        let passwords = &self.inner.login_passwords;
        let offer =
            password.map(|password| passwords.offer(&seat.account, password, Instant::now()));
        run("systemctl_failed", "systemctl", &["start", &unit])?;
        if let Some(offer) = offer {
            offer.wait(|| session::state(&seat.account).is_ok_and(|s| s.live()));
        }
        // The mount's own error is in the unit's journal.
        for stage in shares
            .iter()
            .flat_map(|s| libraries::unmounted(&seat.id, s))
        {
            let stage = stage.display();
            tracing::warn!(seat = %seat.id, %stage, %unit, "seat library stage not mounted");
        }
        Ok(written)
    }

    /// The owner's libraries a made seat is shown, listed in its Steam's library list, and the
    /// owner's shortcuts put into that Steam ([`shortcuts`]).
    fn share_libraries(&self, seat: &Seat, passwd: &accounts::Passwd) -> Option<libraries::Shares> {
        let (box_dir, state) = (&self.inner.box_dir, &self.inner.state);
        let source = self.steam_source();
        let root = source.as_ref().map(|s| s.root.as_path());
        let owner = self.owner();
        let mut shares = libraries::share(box_dir, state, &seat.id, passwd, root, owner.as_ref());
        let shown = shares.as_ref().map_or(&[][..], |s| s.libraries.as_slice());
        libraries::register(state, &seat.id, passwd, shown);
        if let Some(shares) = shares.as_mut() {
            let door = door_reader(&self.inner);
            let (owner, libraries) = (owner.as_ref(), &shares.libraries);
            shares.overlays =
                shortcuts::share(state, &seat.id, passwd, owner, root, libraries, door);
        }
        shares
    }

    /// How a person's cold start opens their session ([`login_with`]): the login drop-in written,
    /// or removed, and its guard, made first so a failed write still removes it. The password
    /// comes back only when the drop-in is written, for the helper to take through [`login_password`].
    fn person_login<'s, 'p>(
        &self,
        seat: &'s Seat,
        password: Option<&'p Secret>,
    ) -> Result<(Option<LoginDropin<'s>>, Option<&'p Secret>), BackendError> {
        let label =
            (self.inner.login_label.get().cloned()).unwrap_or_else(login_password::Label::unbound);
        if label == login_password::Label::Missing && password.is_some() {
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
        let written = dropin.is_some().then(|| LoginDropin(&seat.account));
        session::set_login(&seat.account, dropin.as_deref())?;
        let password = password.filter(|_| written.is_some());
        Ok((written, password))
    }

    /// `seat`'s unit started again as systemd's own login opens it, after the login helper ended
    /// before a session, then what runs inside it raised again.
    fn start_plain(
        &self,
        seat: &Seat,
        passwd: &accounts::Passwd,
        asked: u64,
    ) -> Result<RuntimeStatus, BackendError> {
        self.stopped_since(&seat.id, asked)?;
        let unit = session::unit(&seat.account);
        let _ = run("systemctl_failed", "systemctl", &["reset-failed", &unit]);
        run("systemctl_failed", "systemctl", &["start", &unit])?;
        self.inner.apply_change(
            seat,
            Some(passwd),
            UnitChange::Starting,
            Order::AfterUnit,
            None,
        )?;
        self.wait_ready(seat, asked, false)?;
        Ok(RuntimeStatus::running())
    }

    fn wait_port(&self, seat: &Seat, asked: u64) -> Result<(), BackendError> {
        let deadline = Instant::now() + START_TIMEOUT;
        while !self.serving(seat) {
            self.stopped_since(&seat.id, asked)?;
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
                    ..RuntimeStatus::default()
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

/// The sync thread. Every [`TRUST_TICK`] it brings what the seats read up to date with the box,
/// one job after another:
///
/// 1. the box's library copy, under the copy lock
/// 2. the console's certificate in Firefox
/// 3. each running seat's copies, and the desk yield of a row on the owner's or a person's own
///    account
/// 4. every [`REACH_EVERY`], the libraries that a made seat's Steam missed
/// 5. the ACL mask of the games folder
///
/// What a job remembers between runs is a local variable here, handed to the job. The owner's
/// row stays listed while its host is the owner's own host, which reads the same copy. The
/// thread ends when the backend is dropped.
fn sync_loop(inner: Weak<Inner>) {
    let mut acl_at = Instant::now();
    let mut reach_at = Instant::now();
    let mut failing: HashSet<SeatId> = HashSet::new();
    let mut explained: HashMap<SeatId, String> = HashMap::new();
    let mut library_failing = false;
    let mut firefox_failing = false;
    loop {
        std::thread::sleep(TRUST_TICK);
        let Some(inner) = inner.upgrade() else { return };
        let guard = inner.copy_lock.lock();
        let library = refresh_box_library(&inner);
        drop(guard);
        once_per_outage(library, &mut library_failing, "box library not copied");
        once_per_outage(
            firefox::sync(&inner.box_dir),
            &mut firefox_failing,
            "console certificate not installed for Firefox",
        );
        let seats: Vec<(SeatId, Tracked)> = inner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, tracked)| (id.clone(), tracked.clone()))
            .collect();
        for (id, tracked) in &seats {
            if !sync_seat(&inner, id, tracked, &mut failing) {
                continue;
            }
            if let Some((seat, _)) = &tracked.own_account {
                yield_to_the_desk(&inner, seat);
            }
        }
        if reach_at.elapsed() >= REACH_EVERY {
            reach_at = Instant::now();
            explain_reach(&seats, &mut explained);
        }
        repair_acl(&inner, &seats, &mut acl_at);
    }
}

/// Logs `result`'s error as `what` once per outage: `failing` says whether the last one failed.
fn once_per_outage<T, E: std::fmt::Display>(result: Result<T, E>, failing: &mut bool, what: &str) {
    match result {
        Ok(_) => *failing = false,
        Err(error) if !*failing => {
            *failing = true;
            tracing::warn!(%error, "{what}");
        }
        Err(_) => {}
    }
}

/// Refreshes `id`'s copies ([`refresh_copies`]) under the copy lock, so a delete never sees
/// them written back. A seat removed since `tracked` was read is left alone, and this returns
/// `false`: its files are gone and stay gone. A failure is logged once per outage: `failing` holds
/// the seats whose last refresh failed.
fn sync_seat(inner: &Inner, id: &SeatId, tracked: &Tracked, failing: &mut HashSet<SeatId>) -> bool {
    let mut guard = inner.copy_lock.lock();
    let tracked_now = inner.running.lock().unwrap_or_else(|e| e.into_inner());
    if !tracked_now.contains_key(id) {
        return false;
    }
    drop(tracked_now);
    let refreshed = refresh_copies(inner, &mut guard, id, tracked);
    drop(guard);
    match refreshed {
        Ok(()) => {
            failing.remove(id);
        }
        Err(error) if failing.insert(id.clone()) => {
            tracing::warn!(seat = %id, %error, "trust copy not refreshed");
        }
        Err(_) => {}
    }
    true
}

/// Refreshes what one running seat reads. The first failure ends it.
///
/// 1. The seat's trust copy. For a person's row or a locked profile, its pairing list holds only
///    the devices that the row or the profile's lock still lets on, so it shrinks as presence
///    runs out.
/// 2. The environment file, for a row on the owner's or a person's own account.
/// 3. The box's library in the seat's trust copy, for every seat but the owner's.
fn refresh_copies(
    inner: &Inner,
    guard: &mut CopyGuard<'_>,
    id: &SeatId,
    tracked: &Tracked,
) -> Result<(), BackendError> {
    let (box_dir, state, reader) = (&inner.box_dir, &inner.state, tracked.reader);
    match tracked.kind {
        SeatAccount::Person => person_trust(inner, guard, id, reader)?,
        SeatAccount::Owner => shared::refresh_owner_trust(box_dir, state, id, reader)?,
        SeatAccount::Made => shared::refresh_trust(box_dir, state, id, reader)?,
    };
    if let Some((seat, passwd)) = &tracked.own_account {
        rewrite_env(inner, seat, tracked.kind, passwd)?;
    }
    if tracked.kind != SeatAccount::Owner {
        shared::refresh_seat_library(box_dir, state, id, reader)?;
    }
    Ok(())
}

/// Logs, once per library load of the unit's run, why a made seat's Steam couldn't open a library
/// folder; a load the log keeps from the last run is passed over. `explained` holds the load each
/// seat was last read at.
fn explain_reach(seats: &[(SeatId, Tracked)], explained: &mut HashMap<SeatId, String>) {
    for (id, tracked) in seats.iter().filter(|(_, tracked)| tracked.kind.made()) {
        let Ok(Some(passwd)) = accounts::lookup(&tracked.account) else {
            continue;
        };
        let Some(missed) = reach::missed(&passwd.home) else {
            continue;
        };
        if explained.get(id) == Some(&missed.stamp) {
            continue;
        }
        if reach::this_run(&missed, &session::unit(&tracked.account)) {
            reach::explain(id, &passwd, &missed);
        }
        explained.insert(id.clone(), missed.stamp);
    }
}

/// Repairs the games folder's ACL mask when [`acl_due`]; `acl_at` is when it last did.
fn repair_acl(inner: &Inner, seats: &[(SeatId, Tracked)], acl_at: &mut Instant) {
    let seat_up = seats.iter().any(|(_, tracked)| tracked.kind.made());
    let dirty = inner.acl_dirty.swap(false, Ordering::SeqCst);
    if acl_due(dirty, seat_up, acl_at.elapsed()) {
        *acl_at = Instant::now();
        if let Err(error) = shared::fix_acl(&inner.state) {
            tracing::warn!(%error, "games ACL mask not repaired");
        }
    }
}

/// The ACL repair is due once a unit stopped since the last (`dirty`), and while a made seat is
/// up once [`ACL_EVERY`] has passed `since` the last.
fn acl_due(dirty: bool, seat_up: bool, since: Duration) -> bool {
    dirty || (seat_up && since >= ACL_EVERY)
}

/// Decides how a person's cold start opens their session. Nothing else decides it.
///
/// - With the password that the person proved the start with: through the login helper. Then
///   whatever the person's own login unlocks with that password is unlocked here too.
/// - With no password, or once the helper is `off`: the plain start, where systemd's `PAMName=`
///   opens the session.
///
/// With SELinux on, the helper runs in the context of PID 1, which is the context that systemd's
/// own PAM child has. pam_selinux computes the session's context from its caller's context. So
/// every user, a confined one included, gets what the plain start gives them. For that the
/// login socket must carry the context too ([`login_password::Label::Init`]). Without it, the
/// start is the plain one. A switch for the wallet login goes here.
fn login_with<'a>(
    password: Option<&'a Secret>,
    label: &login_password::Label,
    off: bool,
) -> Option<(&'a Secret, Option<String>)> {
    let context = match label {
        login_password::Label::Off => None,
        login_password::Label::Init(context) => Some(context.clone()),
        login_password::Label::Missing => return None,
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
/// unit file. A binary an upgrade replaced reads as its path plus ` (deleted)`; the suffix is
/// dropped to name the file now at that path.
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

/// Writes the environment file again for a row of `kind` on the owner's or a person's own
/// account. It keeps the token the file holds and takes the box's Seat defaults as they stand
/// now. So a change to Browser streaming reaches the row's host at its next start.
///
/// Every row's file stays root's. The owner may read the file of the owner's row, which the
/// owner's own host reads too. Returns `None` when the file holds no token, and `Some(true)`
/// when the file changed.
fn rewrite_env(
    inner: &Inner,
    seat: &Seat,
    kind: SeatAccount,
    passwd: &accounts::Passwd,
) -> Result<Option<bool>, BackendError> {
    let Some(token) = session::env_token(&seat.account) else {
        return Ok(None);
    };
    let browser = session::browser(&inner.box_dir);
    let changed = session::write_env(seat, kind, passwd, &inner.state, &token, &browser)?;
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
    if !seat.kind.yields_to_the_desk() || !live || !owner_at_desk(account) {
        return;
    }
    tracing::info!(%account, "user at the machine: ending the headless session");
    match stop_unit(inner, seat, UnitChange::Stopped, None) {
        Ok(()) if seat.kind.is_owner() => user_host(account, "start"),
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
    /// Only a seat whose directory is here and that never started is reaped. On a state
    /// directory that is empty, as an unmounted one is, no account is.
    #[test]
    fn an_account_is_reaped_only_from_the_state_directory_that_made_it() {
        let state = tempfile::tempdir().unwrap();
        let (gone, stopped_create, started) =
            (SeatId::random(), SeatId::random(), SeatId::random());
        for id in [&stopped_create, &started] {
            std::fs::create_dir_all(super::shared::seat_dir(state.path(), id)).unwrap();
        }
        std::fs::create_dir_all(super::shared::host_dir(state.path(), &started)).unwrap();
        assert!(!super::reapable(state.path(), &gone), "no seat directory");
        assert!(super::reapable(state.path(), &stopped_create));
        assert!(!super::reapable(state.path(), &started), "it has run");
        let empty = tempfile::tempdir().unwrap();
        for id in [&gone, &stopped_create, &started] {
            assert!(
                !super::reapable(empty.path(), id),
                "an empty state directory"
            );
        }
    }

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

    /// What the tests set up or ask of a backend without performing a whole unit change.
    impl LinuxBackend {
        pub(super) fn track(&self, id: &SeatId, tracked: Tracked) {
            let mut running = self.inner.running.lock().unwrap();
            running.insert(id.clone(), tracked);
        }

        /// The owner's copies, as its adoption writes them.
        pub(super) fn prepare_owner(
            &self,
            seat: &Seat,
            passwd: &accounts::Passwd,
        ) -> Result<bool, BackendError> {
            self.inner.copies(seat, SeatAccount::Owner, passwd, None)
        }
    }

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
            kind: RowKind::owner(),
            fingerprint: None,
            lent: Vec::new(),
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

    /// A made seat as the sync thread holds it, its copies for this process's group.
    fn made_tracked() -> (SeatId, Tracked) {
        let seat = Seat {
            id: SeatId::parse("0123456789abcdef0123456789abcd0a").unwrap(),
            name: "Desk".into(),
            account: "pf-seat-9".into(),
            display_slot: 12,
            native_port: 9779,
            mgmt_port: 48_124,
            autostart: false,
            runtime: RuntimeStatus::stopped(),
            kind: RowKind::made(),
            fingerprint: None,
            lent: Vec::new(),
        };
        let passwd = accounts::Passwd {
            name: "pf-seat-9".into(),
            uid: 3_999_999_990,
            gid: rustix::process::getegid().as_raw(),
            gecos: String::new(),
            home: "/nonexistent".into(),
        };
        (seat.id.clone(), Tracked::of(&seat, &passwd))
    }

    /// What `f` logs on this thread.
    fn logged(f: impl FnOnce()) -> String {
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let lines = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || Sink(Arc::clone(&sink)))
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        String::from_utf8_lossy(&lines.lock().unwrap()).into_owned()
    }

    /// A seat deleted since the sync thread listed the running ones gets none of its copies
    /// written back; one still running gets them.
    #[test]
    fn a_removed_seat_s_copies_are_not_written_back() {
        let (box_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let backend = backend(box_dir.path(), state.path());
        let (id, tracked) = made_tracked();
        let trust = shared::trust_dir(state.path(), &id);
        let mut failing = HashSet::new();
        assert!(!sync_seat(&backend.inner, &id, &tracked, &mut failing));
        assert!(!trust.exists(), "a removed seat's copy stays gone");
        backend.track(&id, tracked.clone());
        assert!(sync_seat(&backend.inner, &id, &tracked, &mut failing));
        assert!(trust.is_dir());
        assert!(failing.is_empty());
    }

    /// A person's row refreshed by the sync thread reaches its device list through the copy lock
    /// the refresh holds, and its trust copy is written.
    #[test]
    fn a_person_s_copies_are_refreshed_under_the_held_lock() {
        let (box_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let backend = backend(box_dir.path(), state.path());
        let uid = rustix::process::getuid().as_raw();
        let seat = Seat {
            id: SeatId::parse("0123456789abcdef0123456789abcd0b").unwrap(),
            name: "Desk".into(),
            account: "pfperson".into(),
            display_slot: 12,
            native_port: 9779,
            mgmt_port: 48_125,
            autostart: false,
            runtime: RuntimeStatus::stopped(),
            kind: RowKind::linux_person(uid),
            fingerprint: None,
            lent: Vec::new(),
        };
        let passwd = accounts::Passwd {
            name: "pfperson".into(),
            uid,
            gid: rustix::process::getgid().as_raw(),
            gecos: String::new(),
            home: "/nonexistent".into(),
        };
        let tracked = Tracked::of(&seat, &passwd);
        backend.track(&seat.id, tracked.clone());
        let mut failing = HashSet::new();
        assert!(sync_seat(&backend.inner, &seat.id, &tracked, &mut failing));
        assert!(failing.is_empty());
        assert!(shared::trust_dir(state.path(), &seat.id).is_dir());
        assert!(backend
            .inner
            .copy_lock
            .lock()
            .devices()
            .contains_key(&seat.id));
    }

    /// A trust copy that keeps failing is logged when it starts to, not at every tick, and again
    /// when it fails after a refresh that worked.
    #[test]
    fn a_failing_trust_copy_is_logged_once_per_outage() {
        let (box_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let backend = backend(box_dir.path(), state.path());
        let (id, tracked) = made_tracked();
        backend.track(&id, tracked.clone());
        let trust = shared::trust_dir(state.path(), &id);
        std::fs::create_dir_all(trust.parent().unwrap()).unwrap();
        let mut failing = HashSet::new();
        let mut tick = |broken: bool| {
            let _ = std::fs::remove_dir_all(&trust);
            let _ = std::fs::remove_file(&trust);
            if broken {
                std::fs::write(&trust, "").unwrap();
            }
            let kept = || assert!(sync_seat(&backend.inner, &id, &tracked, &mut failing));
            let lines = logged(kept);
            (
                lines.matches("trust copy not refreshed").count(),
                failing.contains(&id),
            )
        };
        let ticks = [tick(true), tick(true), tick(true), tick(false), tick(true)];
        assert_eq!(
            ticks,
            [(1, true), (0, true), (0, true), (0, false), (1, true)]
        );
    }

    /// The games folder's ACL mask is repaired once a unit stopped, and while a made seat runs
    /// once `ACL_EVERY` has passed; with no stop and no made seat up, never.
    #[test]
    fn the_acl_repair_is_due_after_a_stop_or_every_acl_every() {
        let (soon, late) = (ACL_EVERY - Duration::from_secs(1), ACL_EVERY);
        assert!(acl_due(true, false, Duration::ZERO));
        assert!(acl_due(true, true, soon));
        assert!(!acl_due(false, true, soon));
        assert!(acl_due(false, true, late));
        assert!(!acl_due(false, false, late * 10));
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
