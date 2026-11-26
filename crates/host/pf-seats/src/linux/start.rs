//! How a row's unit is started and stopped: the account made ready, the unit's files written, the
//! unit raised, and the wait for the seat's host.
//!
//! [`LinuxBackend::start_row`] is the start. It readies the row's account, raises what runs
//! outside the unit, starts the unit, raises what runs inside it, then waits for the host
//! ([`LinuxBackend::wait_ready`]). [`LinuxBackend::bring_up`] writes the seat's environment file
//! and a fresh management token before it starts the unit. A stop asked for while the start
//! runs ends that start ([`LinuxBackend::stopped_since`]). [`stop_unit`] is the reverse, and
//! [`LinuxBackend::remove_files`] takes away what a row made.
//!
//! A person's cold start opens their session through the login helper when it holds their
//! password ([`login_with`]), and through systemd's own login otherwise. A helper that ends
//! before a session falls back to the plain start ([`LOGIN_FALLBACK`], [`LOGIN_TRIPPED`]).

use super::copy_lock::CopyGuard;
use super::owner_host::{door_reader, owner_at_desk, user_host};
use super::resources::{Order, UnitChange};
use super::{accounts, layout, libraries, login, login_password, session, shortcuts, steam_home};
use super::{err, run, Inner, LinuxBackend, SeatAccount, START_TIMEOUT};
use crate::backend::BackendError;
use crate::backend::PlatformBackend as _;
use crate::model::{RuntimeStatus, Seat, SeatId};
use crate::secret::Secret;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

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
    pub(super) fn start_row(
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
                layout::ensure_person_dirs(state, &seat.id, &passwd)?;
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
                let _ = std::fs::remove_dir_all(layout::trust_dir(state, &seat.id));
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
        layout::ensure_games(state, games)?;
        layout::ensure_seat_dirs(state, &seat.id, &passwd, self.owner_uid())?;
        if let Err(error) = steam_home::link_steam(&passwd) {
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

    pub(super) fn stop_unit(&self, seat: &Seat) -> Result<(), BackendError> {
        stop_unit(&self.inner, seat, UnitChange::Stopped, None)
    }

    /// Whether the seat's user, and nobody else, listens on the row's management port: its host,
    /// not a squatter on the loopback.
    pub(super) fn serving(&self, seat: &Seat) -> bool {
        let user = accounts::lookup(&seat.account).ok().flatten();
        user.is_some_and(|user| session::port_held_by(seat.mgmt_port, user.uid))
    }

    /// Whether the seat's host answers: the unit is active and the host's management port is up,
    /// and for the owner's row the runner has marked the headless host as its own.
    pub(super) fn is_ready(&self, seat: &Seat) -> bool {
        self.serving(seat) && (!seat.kind.is_owner() || session::ready_path(&seat.account).exists())
    }

    /// How many stops were asked of `id` so far.
    pub(super) fn stops(&self, id: &SeatId) -> u64 {
        let stops = self.inner.stops.lock().unwrap_or_else(|e| e.into_inner());
        stops.get(id).copied().unwrap_or(0)
    }

    /// Counts a stop of `id`: a start of it in flight gives up.
    pub(super) fn ask_stop(&self, id: &SeatId) {
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
    pub(super) fn wait_ready(
        &self,
        seat: &Seat,
        asked: u64,
        login: bool,
    ) -> Result<(), BackendError> {
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
    pub(super) fn remove_files(
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
        layout::remove_seat_dirs(&self.inner.state, id)?;
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
}

/// Stops `seat`'s unit if it is up, for a stop or a delete (`change`). First, what was started
/// after the unit is stopped ([`Order::AfterUnit`]), so the pad broker refuses the seat's user
/// while the unit ends. Then the unit stops. For a made seat every process of its user is ended,
/// so nothing it started under its own systemd manager survives. Last, what was started before
/// the unit is stopped. On the owner's or a person's account nothing but the unit is stopped and
/// no process of theirs is signalled. A unit that never started is fine. `held`: the copy lock
/// that a delete holds across the stop.
pub(super) fn stop_unit(
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
pub(super) fn login_with<'a>(
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
pub(super) fn login_dropin(context: Option<&str>) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.to_str()?;
    let path = Path::new(path.strip_suffix(" (deleted)").unwrap_or(path));
    session::render_login(path, context)
}

/// The error of a start through the login helper that ended before a session: the start goes
/// on without it. The gate already admitted the person, so the helper never refuses.
pub(super) const LOGIN_FALLBACK: &str = "login_fallback";
/// [`LOGIN_FALLBACK`] that also turns the helper off ([`login::Fallback::trips`]).
pub(super) const LOGIN_TRIPPED: &str = "login_tripped";

/// Why `seat`'s start failed once its unit ended in `state`. With `login`, a login helper that
/// ended before a session says [`LOGIN_FALLBACK`] or [`LOGIN_TRIPPED`], with the operator's
/// cause.
pub(super) fn unit_ended(seat: &Seat, state: &session::UnitState, login: bool) -> BackendError {
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
pub(super) fn rewrite_env(
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
