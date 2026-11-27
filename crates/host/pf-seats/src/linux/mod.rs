//! The Linux platform: one user, one logind session and one systemd unit per seat.
//!
//! The root daemon `punktfunk-seats` runs [`LinuxBackend`] behind [`socket`]. A seat is the unit
//! `punktfunk-seat@<user>.service`: it logs the user in through PAM and runs `seat-session`, a
//! headless compositor and an ordinary `punktfunk-host serve`. Everything the seat runs is a
//! child of the unit, so the unit's bind mounts reach it.
//!
//! What the daemon may do to a seat's account depends on whose account it is ([`SeatAccount`]).
//! It made a made seat's account and deletes it with the seat. The owner's account and a
//! person's account belong to someone who may be logged in at the machine: there the daemon
//! starts and stops the seat's unit and changes nothing else. A person's seat starts only after
//! [`pam`] accepts their password.
//!
//! What must be up while a unit runs (copied files, lent pads, the uinput stand-in, the pad
//! broker, present devices) is started and stopped in one function, [`Inner::apply_change`], from
//! the table in [`resources`]. Every writer of a seat's copied files takes
//! [`copy_lock::CopyLock`]. [`node_owner`] tells udev which user owns each pad's device nodes.
//!
//! The rest is in [`start`], [`sync`], [`checks`], [`presence`] and [`owner_host`]: a row's
//! start and stop, the sync thread, the prerequisites, a person's device list, the owner's host.

mod accounts;
mod acl;
mod checks;
mod copy_lock;
mod devices;
pub mod door_switch;
pub mod emulators;
#[cfg(test)]
mod fake;
mod firefox;
mod flatpak;
mod layout;
pub mod lent;
mod libraries;
mod library_copy;
pub mod login;
pub mod login_password;
mod logind;
pub mod loopback;
pub mod mount_share;
pub mod node_owner;
mod owner_host;
mod pam;
#[cfg(test)]
mod person_tests;
mod presence;
mod reach;
mod resources;
mod safe_open;
mod session;
mod shortcuts;
pub mod socket;
mod start;
#[cfg(test)]
mod start_tests;
mod steam_home;
mod sync;
mod trust_copy;
#[cfg(test)]
mod unlock_tests;

pub use layout::ledger_root;
pub use owner_host::owner_at_desk;
pub use pam::{run_check as pam_check, CHECK_VERB as PAM_CHECK_VERB};
pub use socket::SOCKET_PATH;

use crate::backend::{Adopted, BackendError, PlatformBackend, PresentPad};
use crate::ipc::{Diagnostic, DiagnosticLevel, SeatingStatus};
use crate::model::{Ledger, RowKind, RuntimeState, RuntimeStatus, Seat, SeatId};
use crate::persistence::{LedgerStore, StoreError};
use crate::secret::Secret;
use crate::service::SeatService;
use crate::unlock::DeviceList;
use checks::cached;
use copy_lock::{CopyGuard, CopyLock};
use owner_host::refresh_box_library;
use presence::{edit_devices, end_presence, person_trust, with_devices};
use resources::{Action, Order, Resource, UnitChange};
use start::{rewrite_env, stop_unit};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use sync::sync_loop;

/// The supervisor's own state: the ledger, every seat's home and trust copy, the shared games
/// folder. Only root can change it. The door has its own directory, `/var/lib/punktfunk`, and
/// only reads files here.
pub const STATE_DIR: &str = "/var/lib/punktfunk-seats";

/// A seat's save exchange: `out` holds what its host exported for the owner's plugins, `in`
/// what the owner's host hands back. The same path inside the seat's unit.
pub fn saves_dir(id: &SeatId) -> PathBuf {
    layout::seat_dir(Path::new(STATE_DIR), id).join("saves")
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

/// Whether an off's hold stands under [`STATE_DIR`]: an off that began and didn't finish. The
/// hold an on sets while it copies doesn't count ([`door_switch::off_held`]).
pub fn off_held() -> bool {
    door_switch::off_held(Path::new(STATE_DIR))
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
    /// Set once a login helper tripped ([`start::LOGIN_TRIPPED`]): no helper until the supervisor
    /// restarts.
    helper_off: AtomicBool,
    /// What the login socket carries, once the binary bound it ([`LinuxBackend::login_socket`]).
    login_label: OnceLock<login_password::Label>,
    /// The desk pads lent to rows, and the hardware they name.
    lent: lent::Lent,
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
    reader: trust_copy::Reader,
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
            (trust_copy::Reader::group(passwd.gid), None)
        } else {
            (
                trust_copy::Reader::person(passwd.uid, passwd.gid),
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
    let here = matches!(layout::seat_dir(state, id).try_exists(), Ok(true));
    let started = !matches!(layout::host_dir(state, id).try_exists(), Ok(false));
    here && !started
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
                trust_copy::refresh_trust(box_dir, state, id, reader)?;
                library_copy::refresh_seat_library(box_dir, state, id, reader)?;
                Ok(false)
            }
            SeatAccount::Person => {
                person_trust(self, guard, id, reader)?;
                library_copy::refresh_seat_library(box_dir, state, id, reader)?;
                Ok(false)
            }
            SeatAccount::Owner => {
                trust_copy::refresh_owner_trust(box_dir, state, id, reader)?;
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
        layout::write_host_token(
            &self.state,
            &seat.id,
            &session::token_line(token),
            layout::token_owner(door.as_ref()),
        )
    }
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
        layout::ensure_layout(&state)
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
        layout::make_dir(&root, 0o711)?;
        let service = SeatService::with_store(store, self)?;
        service.backend().reap_orphans(&service.ledger());
        service.backend().load_devices(&service.ledger());
        Ok(service)
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
    fn steam_source(&self) -> Option<steam_home::SteamSource> {
        if let Some(root) = &self.inner.steam_source {
            return steam_home::SteamSource::owned(root.clone());
        }
        match self.owner() {
            Some(owner) => steam_home::steam_source_of(&owner),
            None => steam_home::find_steam_source(&accounts::all().ok()?),
        }
    }
}

impl PlatformBackend for LinuxBackend {
    fn provision(&self, seat: &Seat) -> Result<(), BackendError> {
        accounts::check_seat_name(&seat.account)?;
        let state = &self.inner.state;
        let gid = accounts::ensure_group(accounts::GAMES_GROUP)?;
        layout::ensure_games(state, gid)?;
        let passwd = accounts::ensure(seat, state)?;
        accounts::close_home(&passwd.home)?;
        layout::ensure_seat_dirs(state, &seat.id, &passwd, self.owner_uid())?;
        steam_home::provision_steam(state, &seat.id, &passwd, self.steam_source().as_ref());
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
        let made = layout::ensure_person_dirs(&self.inner.state, &seat.id, &passwd);
        if made.is_err() {
            let _ = layout::remove_seat_dirs(&self.inner.state, &seat.id);
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

    /// A person's row, started with the password they proved the start with:
    /// [`start::login_with`] decides whether that password opens their session.
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
                library_copy::remove_box_library(&self.inner.state)
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
    /// never waits on what is there ([`trust_copy::copy_pin`]).
    fn host_config_dir(&self, seat: &Seat) -> Option<PathBuf> {
        let state = &self.inner.state;
        let dir = trust_copy::pin_dir(state, &seat.id);
        let copied = match SeatAccount::of(&seat.kind) {
            SeatAccount::Owner => {
                trust_copy::copy_pin(&accounts::require_owner(&seat.account).ok()?.home, &dir)
            }
            SeatAccount::Person => {
                accounts::require_person(seat).ok()?;
                trust_copy::copy_person_pin(state, &seat.id, &dir)
            }
            SeatAccount::Made => trust_copy::copy_pin(&accounts::require(seat).ok()?.home, &dir),
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

    fn install_emulator(&self, id: &str) -> Result<String, BackendError> {
        emulators::install(id)
    }

    fn remove_emulator(&self, id: &str) -> Result<(), BackendError> {
        emulators::remove(id)
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
            std::fs::create_dir_all(super::layout::seat_dir(state.path(), id)).unwrap();
        }
        std::fs::create_dir_all(super::layout::host_dir(state.path(), &started)).unwrap();
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

    use super::sync::tests::backend;
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
