//! The sync thread: what keeps the seats' copies level with the box while seats run.
//!
//! [`sync_loop`] wakes every [`TRUST_TICK`] and does its jobs one after another: the box
//! library, the console's certificate in Firefox, each running seat's copies, a log of the
//! libraries that a made seat's Steam couldn't open, and the ACL mask of the games folder.
//!
//! A seat's copies are written under the copy lock ([`super::copy_lock`]), so a delete never sees
//! its seat's files written back ([`sync_seat`]). A job that keeps failing is logged once per
//! outage, not at every tick ([`once_per_outage`]). A row on the owner's or a person's own
//! account also yields to its user at the machine ([`yield_to_the_desk`]). The thread ends when
//! the backend is dropped.

use super::copy_lock::CopyGuard;
use super::owner_host::{owner_at_desk, refresh_box_library, user_host};
use super::presence::person_trust;
use super::resources::UnitChange;
use super::start::{rewrite_env, stop_unit};
use super::{accounts, acl, firefox, library_copy, reach, session, trust_copy};
use super::{Inner, SeatAccount, Tracked, ACL_EVERY, REACH_EVERY, TRUST_TICK};
use crate::backend::BackendError;
use crate::model::{Seat, SeatId};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Weak;
use std::time::{Duration, Instant};

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
pub(super) fn sync_loop(inner: Weak<Inner>) {
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
        SeatAccount::Owner => trust_copy::refresh_owner_trust(box_dir, state, id, reader)?,
        SeatAccount::Made => trust_copy::refresh_trust(box_dir, state, id, reader)?,
    };
    if let Some((seat, passwd)) = &tracked.own_account {
        rewrite_env(inner, seat, tracked.kind, passwd)?;
    }
    if tracked.kind != SeatAccount::Owner {
        library_copy::refresh_seat_library(box_dir, state, id, reader)?;
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
        if let Err(error) = acl::fix_acl(&inner.state) {
            tracing::warn!(%error, "games ACL mask not repaired");
        }
    }
}

/// The ACL repair is due once a unit stopped since the last (`dirty`), and while a made seat is
/// up once [`ACL_EVERY`] has passed `since` the last.
fn acl_due(dirty: bool, seat_up: bool, since: Duration) -> bool {
    dirty || (seat_up && since >= ACL_EVERY)
}

/// The physical login wins: when the row's user has a session at the machine and the row's
/// headless unit is up, the unit ends. The owner's own host then takes the row; another person's
/// row stays down. Their session is never touched, so what they have on the monitor stays as it
/// is.
pub(super) fn yield_to_the_desk(inner: &Inner, seat: &Seat) {
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::linux::{layout, LinuxBackend};
    use crate::model::{RowKind, RuntimeStatus};
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    pub(in crate::linux) fn backend(box_dir: &Path, state: &Path) -> LinuxBackend {
        LinuxBackend::detached(box_dir, state)
    }

    /// A seat deleted since the sync thread listed the running ones gets none of its copies
    /// written back; one still running gets them.
    #[test]
    fn a_removed_seat_s_copies_are_not_written_back() {
        let (box_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let backend = backend(box_dir.path(), state.path());
        let (id, tracked) = made_tracked();
        let trust = layout::trust_dir(state.path(), &id);
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
        assert!(layout::trust_dir(state.path(), &seat.id).is_dir());
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
        let trust = layout::trust_dir(state.path(), &id);
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
}
