//! Attempt limits for checks of a secret. The supervisor uses them for passwords. The host uses
//! them, per device and in memory only, for profile PINs. A profile's own cap is in its lock file
//! (`profile_locks`). Pure: the caller passes the time and does any file I/O.
//!
//! - **Per device.** A device is counted separately for each seat, account or profile it tries.
//!   After each failed check it waits `2 s × 2^failures`, and after the fifth, [`LOCKOUT`].
//! - **Per account.** Over all its devices, an account lets at most [`ACCOUNT_FAILURES`] failed
//!   checks through in any [`WINDOW`]. So a login stack that locks an account after three
//!   failures (`pam_faillock`) is never driven there from here. The window doubles with every
//!   [`ACCOUNT_STEP`] failures since the account's last pass, up to [`WINDOW_MAX`].
//! - **In flight.** A check counts as failed until it settles, so concurrent attempts can't
//!   share one allowance. A refusal by these limits is not a failure.
//! - **Forgetting.** A pass clears its device and its account. A device's entry is dropped once
//!   its last failure is one [`WINDOW`] old, an account's once its last failure is one
//!   [`WINDOW_MAX`] old. So waiting out one window doesn't reset the doubling.
//! - **Saved.** The account side, without checks in flight, converts to unix seconds and back
//!   ([`Saved`]), so the supervisor keeps it in a file across a restart. The device side doesn't.

use crate::backend::BackendError;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

const BACKOFF: Duration = Duration::from_secs(2);
/// Failures after which a device waits [`LOCKOUT`].
const DEVICE_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Failed checks an account takes in any window of its own ([`Account::window`]).
const ACCOUNT_FAILURES: usize = 2;
const WINDOW: Duration = Duration::from_secs(15 * 60);
/// The failures since an account's last pass that double its window. A flat window lets one
/// source try about 70,000 passwords a year; doubled to the cap, it is about 700.
const ACCOUNT_STEP: u32 = 10;
const WINDOW_MAX: Duration = Duration::from_secs(24 * 60 * 60);
/// The largest [`Saved`] in JSON: 64 accounts under sustained guessing fit in about 40 KiB.
pub const SAVED_BYTES: usize = 64 * 1024;
/// The most accounts a [`Saved`] holds. A box has a handful.
const SAVED_ACCOUNTS: usize = 64;

/// What a check is for: `device`, a device of a seat, an account or a profile, and `account`, what
/// the secret opens. A side that is `None` has no limit of its own.
#[derive(Clone, Copy, Debug)]
pub struct Target<'a> {
    pub device: Option<(&'a str, &'a str)>,
    pub account: Option<&'a str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The password opened the account.
    Passed,
    /// The password may be wrong.
    Failed,
    /// The check never judged the password.
    Unjudged,
}

/// An admitted check: when its charge was taken. [`Attempts::settle`] takes it back.
#[must_use]
#[derive(Debug)]
pub struct Ticket(Instant);

#[derive(Debug, Default)]
pub struct Attempts {
    devices: HashMap<(String, String), Device>,
    accounts: HashMap<String, Account>,
}

#[derive(Debug, Default)]
struct Device {
    failures: u32,
    /// The last failure, and the end of the wait it set.
    last: Option<(Instant, Instant)>,
    /// When the check in flight was admitted.
    pending: Option<Instant>,
}

#[derive(Debug, Default)]
struct Account {
    failed: Vec<Instant>,
    /// When each check in flight was admitted.
    pending: Vec<Instant>,
    /// The failures since the last pass.
    streak: u32,
}

/// The account side of an [`Attempts`] with its times as unix seconds on the wall clock.
#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    accounts: BTreeMap<String, SavedAccount>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SavedAccount {
    failed: Vec<i64>,
    streak: u32,
}

impl Saved {
    /// The state as JSON, `None` over [`SAVED_BYTES`] or [`SAVED_ACCOUNTS`].
    pub fn to_json(&self) -> Option<Vec<u8>> {
        let bytes = serde_json::to_vec(self).ok()?;
        self.fits(&bytes).then_some(bytes)
    }

    /// The state `bytes` hold, `None` for one [`Saved::to_json`] would not have written.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let saved: Self = serde_json::from_slice(bytes).ok()?;
        saved.fits(bytes).then_some(saved)
    }

    fn fits(&self, bytes: &[u8]) -> bool {
        bytes.len() <= SAVED_BYTES && self.accounts.len() <= SAVED_ACCOUNTS
    }
}

impl Attempts {
    /// The account side at `now`, which is `unix` on the wall clock. Times round to the second
    /// later, so an import waits at most a second longer.
    pub fn export(&self, now: Instant, unix: i64) -> Saved {
        let age = |at: &Instant| now.saturating_duration_since(*at).as_secs();
        let stamp = |at: &Instant| unix.saturating_sub_unsigned(age(at));
        let accounts = self.accounts.iter().filter(|(_, a)| !a.failed.is_empty());
        let saved = |(key, a): (&String, &Account)| {
            let failed = a.failed.iter().map(stamp).collect();
            let streak = a.streak;
            (key.clone(), SavedAccount { failed, streak })
        };
        Saved {
            accounts: accounts.map(saved).collect(),
        }
    }

    /// Takes in the accounts of `saved` at `now`, which is `unix` on the wall clock. A failure
    /// [`WINDOW_MAX`] old is dropped, and one stamped after `unix` counts as now. A failure from
    /// before the `Instant` clock's start (on Windows, the boot) counts as now while the account's
    /// window would still hold it, and is dropped after.
    pub fn import(&mut self, saved: Saved, now: Instant, unix: i64) {
        for (key, entry) in saved.accounts {
            let mut account = Account {
                streak: entry.streak,
                ..Account::default()
            };
            let window = account.window();
            for stamp in entry.failed {
                let age = u64::try_from(unix.saturating_sub(stamp)).unwrap_or(0);
                let age = Duration::from_secs(age);
                if age >= WINDOW_MAX {
                    continue;
                }
                match now.checked_sub(age) {
                    Some(at) => account.failed.push(at),
                    None if age < window => account.failed.push(now),
                    None => {}
                }
            }
            if !account.failed.is_empty() {
                self.accounts.insert(key, account);
            }
        }
    }

    /// Admits a check for `target` at `now`, or answers how long until one would be.
    pub fn admit(&mut self, target: Target<'_>, now: Instant) -> Result<Ticket, Duration> {
        self.expire(now);
        let device = target
            .device
            .and_then(|(of, device)| self.devices.get(&key(of, device)))
            .map_or(Duration::ZERO, |d| d.wait(now));
        let account = target
            .account
            .and_then(|account| self.accounts.get(account))
            .map_or(Duration::ZERO, |a| a.wait(now));
        let wait = device.max(account);
        if !wait.is_zero() {
            return Err(wait);
        }
        if let Some((of, device)) = target.device {
            self.devices.entry(key(of, device)).or_default().pending = Some(now);
        }
        if let Some(account) = target.account {
            let entry = self.accounts.entry(account.to_owned()).or_default();
            entry.pending.push(now);
        }
        Ok(Ticket(now))
    }

    /// Counts `outcome`, the answer at `now` of the check `ticket` admitted for `target`.
    pub fn settle(&mut self, target: Target<'_>, ticket: Ticket, outcome: Outcome, now: Instant) {
        if let Some((of, device)) = target.device {
            let entry = self.devices.entry(key(of, device)).or_default();
            entry.pending = None;
            match outcome {
                Outcome::Passed => *entry = Device::default(),
                Outcome::Failed => {
                    entry.failures = entry.failures.saturating_add(1);
                    entry.last = Some((now, now + delay(entry.failures)));
                }
                Outcome::Unjudged => {}
            }
        }
        if let Some(account) = target.account {
            let entry = self.accounts.entry(account.to_owned()).or_default();
            if let Some(at) = entry.pending.iter().position(|t| *t == ticket.0) {
                entry.pending.swap_remove(at);
            }
            match outcome {
                Outcome::Passed => *entry = Account::default(),
                Outcome::Failed => {
                    entry.failed.push(now);
                    entry.streak = entry.streak.saturating_add(1);
                }
                Outcome::Unjudged => {}
            }
        }
    }

    /// The failures counted at `now`: the device's since its last pass, the account's in the
    /// window.
    pub fn failures(&mut self, target: Target<'_>, now: Instant) -> (u32, usize) {
        self.expire(now);
        let device = target
            .device
            .and_then(|(of, device)| self.devices.get(&key(of, device)))
            .map_or(0, |d| d.failures);
        let in_window = |a: &Account| {
            let window = a.window();
            let recent = |at: &&Instant| now.saturating_duration_since(**at) < window;
            a.failed.iter().filter(recent).count()
        };
        let account = target
            .account
            .and_then(|account| self.accounts.get(account))
            .map_or(0, in_window);
        (device, account)
    }

    /// Drops what is a window old. A check that never settled ages out as a failure would. An
    /// account keeps its failures, and with them its doubled window, for [`WINDOW_MAX`].
    fn expire(&mut self, now: Instant) {
        let within = |at: &Instant, window| now.saturating_duration_since(*at) < window;
        let live = |at: &Instant| within(at, WINDOW);
        self.devices.retain(|_, d| {
            d.pending = d.pending.filter(live);
            d.pending.is_some() || d.last.is_some_and(|(at, _)| live(&at))
        });
        self.accounts.retain(|_, a| {
            a.failed.retain(|at| within(at, WINDOW_MAX));
            a.pending.retain(live);
            !a.failed.is_empty() || !a.pending.is_empty()
        });
    }
}

impl Device {
    /// A check in flight refuses another for as long as its failure would.
    fn wait(&self, now: Instant) -> Duration {
        let pending = self
            .pending
            .map_or(Duration::ZERO, |_| delay(self.failures.saturating_add(1)));
        let set = self.last.map_or(Duration::ZERO, |(_, until)| {
            until.saturating_duration_since(now)
        });
        pending.max(set)
    }
}

impl Account {
    /// [`WINDOW`], doubled once for every [`ACCOUNT_STEP`] failures since the last pass, up to
    /// [`WINDOW_MAX`].
    fn window(&self) -> Duration {
        let doubled = WINDOW.saturating_mul(1 << (self.streak / ACCOUNT_STEP).min(16));
        doubled.min(WINDOW_MAX)
    }

    /// Zero while fewer than [`ACCOUNT_FAILURES`] failures and checks in flight count in the
    /// account's window, else until all but one of them have left it.
    fn wait(&self, now: Instant) -> Duration {
        let window = self.window();
        let mut counted: Vec<Instant> = (self.failed.iter().chain(&self.pending).copied())
            .filter(|at| now.saturating_duration_since(*at) < window)
            .collect();
        if counted.len() < ACCOUNT_FAILURES {
            return Duration::ZERO;
        }
        counted.sort_unstable();
        (counted[counted.len() - ACCOUNT_FAILURES] + window).saturating_duration_since(now)
    }
}

fn key(of: &str, device: &str) -> (String, String) {
    (of.to_owned(), device.to_owned())
}

/// How long a device waits after its `failures`th failure.
fn delay(failures: u32) -> Duration {
    if failures >= DEVICE_FAILURES {
        LOCKOUT
    } else {
        BACKOFF * 2_u32.pow(failures)
    }
}

/// `wait` in whole seconds under a minute, else in minutes, rounded up.
fn rounded(wait: Duration) -> (u64, &'static str) {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    if seconds < 60 {
        (seconds.max(1), "second")
    } else {
        (seconds.div_ceil(60), "minute")
    }
}

/// The `unlock_locked_out` refusal that lasts `wait`, with its message and count.
pub(crate) fn lockout(wait: Duration, password: bool) -> BackendError {
    let (count, unit) = rounded(wait);
    let said = match (password, unit) {
        (true, "second") => "unlock_locked_out_password_seconds",
        (true, _) => "unlock_locked_out_password_minutes",
        (false, "second") => "unlock_locked_out_device_seconds",
        (false, _) => "unlock_locked_out_device_minutes",
    };
    BackendError::new("unlock_locked_out", sentence(wait, password))
        .said(said)
        .param("count", count)
}

/// The sentence for a refusal that lasts `wait`: seconds under a minute, else minutes, rounded up.
/// `password` says whether a password was being checked; a key proof names no password.
pub(crate) fn sentence(wait: Duration, password: bool) -> String {
    let (count, unit) = rounded(wait);
    let plural = if count == 1 { "" } else { "s" };
    let what = if password {
        "Too many wrong passwords."
    } else {
        "Too many failed attempts on this device."
    };
    format!("{what} Try again in {count} {unit}{plural}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);
    const MINUTE: Duration = Duration::from_secs(60);

    fn console(account: &str) -> Target<'_> {
        Target {
            device: Some((account, "console")),
            account: Some(account),
        }
    }

    fn device_only<'a>(seat: &'a str, device: &'a str) -> Target<'a> {
        Target {
            device: Some((seat, device)),
            account: None,
        }
    }

    /// One attempt at `now`: admitted, then `checker`'s answer settles it.
    fn attempt(
        gate: &mut Attempts,
        target: Target<'_>,
        now: Instant,
        checker: &mut impl FnMut() -> Outcome,
    ) -> Result<Outcome, Duration> {
        let ticket = gate.admit(target, now)?;
        let outcome = checker();
        gate.settle(target, ticket, outcome, now);
        Ok(outcome)
    }

    /// One failure for `who`, after waiting out any refusal; then a second passes.
    fn fail(gate: &mut Attempts, who: Target<'_>, now: &mut Instant) {
        let ticket = gate.admit(who, *now).unwrap_or_else(|wait| {
            *now += wait;
            gate.admit(who, *now).unwrap()
        });
        gate.settle(who, ticket, Outcome::Failed, *now);
        *now += SECOND;
    }

    /// Each failure doubles the wait from 4 s; a refusal inside it changes nothing.
    #[test]
    fn a_device_backs_off_after_each_failure() {
        let mut gate = Attempts::default();
        let t0 = Instant::now();
        let target = device_only("seat", "phone");
        let mut wrong = || Outcome::Failed;
        attempt(&mut gate, target, t0, &mut wrong).unwrap();
        assert_eq!(
            attempt(&mut gate, target, t0 + SECOND, &mut wrong),
            Err(3 * SECOND)
        );
        assert_eq!(gate.admit(target, t0 + 3 * SECOND).unwrap_err(), SECOND);
        attempt(&mut gate, target, t0 + 4 * SECOND, &mut wrong).unwrap();
        let t1 = t0 + 4 * SECOND;
        assert_eq!(gate.admit(target, t1).unwrap_err(), 8 * SECOND);
        attempt(&mut gate, target, t1 + 8 * SECOND, &mut wrong).unwrap();
        let t2 = t1 + 8 * SECOND;
        assert_eq!(gate.admit(target, t2).unwrap_err(), 16 * SECOND);
    }

    /// The fifth failure locks the device for 15 minutes, and that device alone.
    #[test]
    fn five_failures_lock_a_device_for_fifteen_minutes() {
        let mut gate = Attempts::default();
        let mut now = Instant::now();
        let target = device_only("seat", "phone");
        let mut wrong = || Outcome::Failed;
        for failures in 1..=5 {
            attempt(&mut gate, target, now, &mut wrong).unwrap();
            if failures < 5 {
                now += delay(failures);
            }
        }
        assert_eq!(gate.admit(target, now).unwrap_err(), LOCKOUT);
        assert_eq!(gate.admit(target, now + 14 * MINUTE).unwrap_err(), MINUTE);
        let other = device_only("seat", "tv");
        assert!(
            gate.admit(other, now).is_ok(),
            "another device keeps its budget"
        );
        assert!(gate.admit(target, now + LOCKOUT).is_ok());
    }

    /// Two failed checks for an account, from two devices: the third attempt, from a third device,
    /// is refused before the checker runs, until the first failure is 15 minutes old.
    #[test]
    fn the_third_wrong_password_in_the_window_never_reaches_the_checker() {
        let mut gate = Attempts::default();
        let t0 = Instant::now();
        let checked = std::cell::Cell::new(0);
        let mut checker = || {
            checked.set(checked.get() + 1);
            Outcome::Failed
        };
        let from = |device| Target {
            device: Some(("uid:1500", device)),
            account: Some("uid:1500"),
        };
        attempt(&mut gate, from("phone"), t0, &mut checker).unwrap();
        attempt(&mut gate, from("tv"), t0 + MINUTE, &mut checker).unwrap();
        let refused = attempt(&mut gate, from("deck"), t0 + 2 * MINUTE, &mut checker);
        assert_eq!(refused, Err(13 * MINUTE));
        assert_eq!(
            attempt(
                &mut gate,
                console("uid:1500"),
                t0 + 10 * MINUTE,
                &mut checker
            ),
            Err(5 * MINUTE)
        );
        assert_eq!(checked.get(), 2, "the checker saw two wrong passwords");
        attempt(&mut gate, from("deck"), t0 + WINDOW, &mut checker).unwrap();
        assert_eq!(checked.get(), 3);
        assert_eq!(
            gate.admit(from("pad"), t0 + WINDOW).unwrap_err(),
            MINUTE,
            "the window slides: the second failure leaves it next"
        );
    }

    /// A pass clears the device's failures and the account's.
    #[test]
    fn a_pass_clears_the_device_and_the_account() {
        let mut gate = Attempts::default();
        let t0 = Instant::now();
        let target = console("uid:1500");
        attempt(&mut gate, target, t0, &mut || Outcome::Failed).unwrap();
        let elsewhere = Target {
            device: Some(("uid:1500", "phone")),
            account: Some("uid:1500"),
        };
        attempt(&mut gate, elsewhere, t0, &mut || Outcome::Passed).unwrap();
        assert!(gate.accounts["uid:1500"].failed.is_empty());
        attempt(&mut gate, target, t0 + 4 * SECOND, &mut || Outcome::Passed).unwrap();
        let mut wrong = || Outcome::Failed;
        attempt(&mut gate, target, t0 + 4 * SECOND, &mut wrong).unwrap();
        assert_eq!(
            gate.admit(target, t0 + 4 * SECOND).unwrap_err(),
            4 * SECOND,
            "counting starts over"
        );
    }

    /// A check that never judged the password counts for nothing.
    #[test]
    fn an_unjudged_check_costs_nothing() {
        let mut gate = Attempts::default();
        let t0 = Instant::now();
        let target = console("uid:1500");
        for _ in 0..5 {
            attempt(&mut gate, target, t0, &mut || Outcome::Unjudged).unwrap();
        }
        attempt(&mut gate, target, t0, &mut || Outcome::Failed).unwrap();
        assert_eq!(gate.admit(target, t0).unwrap_err(), 4 * SECOND);
    }

    /// Checks in flight count until they settle: a device has one at a time, and an account no more
    /// than its allowance.
    #[test]
    fn checks_in_flight_share_no_allowance() {
        let mut gate = Attempts::default();
        let t0 = Instant::now();
        let from = |device| Target {
            device: Some(("uid:1500", device)),
            account: Some("uid:1500"),
        };
        let first = gate.admit(from("phone"), t0).unwrap();
        assert_eq!(gate.admit(from("phone"), t0).unwrap_err(), 4 * SECOND);
        let second = gate.admit(from("tv"), t0).unwrap();
        assert_eq!(gate.admit(from("deck"), t0).unwrap_err(), WINDOW);
        gate.settle(from("phone"), first, Outcome::Unjudged, t0);
        let third = gate.admit(from("deck"), t0).unwrap();
        gate.settle(from("tv"), second, Outcome::Passed, t0);
        gate.settle(from("deck"), third, Outcome::Failed, t0 + SECOND);
        assert_eq!(gate.accounts["uid:1500"].failed, [t0 + SECOND]);
        assert!(gate.accounts["uid:1500"].pending.is_empty());
    }

    /// A device's entry goes once its last failure is 15 minutes old, a check that never settled
    /// with it. An account's failure stays a day, so its doubled window does.
    #[test]
    fn entries_expire() {
        let mut gate = Attempts::default();
        let t0 = Instant::now();
        let target = console("uid:1500");
        attempt(&mut gate, target, t0, &mut || Outcome::Failed).unwrap();
        let _lost = gate.admit(console("uid:1600"), t0).unwrap();
        gate.expire(t0 + WINDOW - SECOND);
        assert_eq!((gate.devices.len(), gate.accounts.len()), (2, 2));
        gate.expire(t0 + WINDOW);
        assert!(gate.devices.is_empty());
        assert_eq!(gate.accounts.keys().collect::<Vec<_>>(), ["uid:1500"]);
        gate.expire(t0 + WINDOW_MAX);
        assert!(gate.accounts.is_empty());
    }

    /// An account's window doubles with every ten failures since its last pass, up to a day.
    /// Waiting out one window doesn't reset it; a pass or a day without a failure does.
    #[test]
    fn an_account_s_window_doubles_under_sustained_guessing() {
        let who = Target {
            device: None,
            account: Some("uid:1001"),
        };
        let mut gate = Attempts::default();
        let mut now = Instant::now();
        // Failures come two to a window, so every even count ends locked.
        for _ in 0..8 {
            fail(&mut gate, who, &mut now);
        }
        let wait = |gate: &mut Attempts, now| gate.admit(who, now).unwrap_err();
        let first = wait(&mut gate, now);
        assert!(first <= WINDOW, "eight failures: {first:?}");
        for _ in 0..4 {
            fail(&mut gate, who, &mut now);
        }
        let doubled = wait(&mut gate, now);
        assert!(doubled > WINDOW && doubled <= WINDOW * 2, "{doubled:?}");
        for _ in 0..80 {
            fail(&mut gate, who, &mut now);
        }
        let capped = wait(&mut gate, now);
        let half = WINDOW_MAX / 2;
        assert!(capped > half && capped <= WINDOW_MAX, "{capped:?}");
        // Idle for one first window: the doubling stands.
        assert!(gate.admit(who, now + WINDOW).is_err());
        // A day without a failure forgets the account.
        assert!(gate.admit(who, now + WINDOW_MAX).is_ok());

        // A pass forgets it at once: the next two failures wait the first window again.
        let mut fresh = Attempts::default();
        for _ in 0..12 {
            fail(&mut fresh, who, &mut now);
        }
        now += WINDOW * 2;
        let ticket = fresh.admit(who, now).unwrap();
        fresh.settle(who, ticket, Outcome::Passed, now);
        fail(&mut fresh, who, &mut now);
        fail(&mut fresh, who, &mut now);
        let again = wait(&mut fresh, now);
        assert!(again <= WINDOW, "{again:?}");
    }

    const UNIX: i64 = 1_800_000_000;

    /// An account's state exported at one instant and imported at another, with the wall clock
    /// moved as far, waits as long and keeps its doubled window. A check in flight stays behind.
    #[test]
    fn an_account_s_state_survives_an_export_and_an_import() {
        let who = Target {
            device: None,
            account: Some("uid:1001"),
        };
        let mut gate = Attempts::default();
        let mut now = Instant::now();
        for _ in 0..12 {
            fail(&mut gate, who, &mut now);
        }
        let _in_flight = gate.admit(console("uid:1002"), now).unwrap();
        let saved = gate.export(now, UNIX);
        assert_eq!(saved.accounts.keys().collect::<Vec<_>>(), ["uid:1001"]);

        let restart = Instant::now() + 3 * MINUTE;
        let mut fresh = Attempts::default();
        fresh.import(saved, restart, UNIX + 180);
        assert_eq!(fresh.accounts["uid:1001"].streak, 12);
        let waits = gate.admit(who, now + 3 * MINUTE).unwrap_err();
        assert!(waits > WINDOW - 3 * MINUTE, "the doubled window: {waits:?}");
        assert_eq!(fresh.admit(who, restart).unwrap_err(), waits);
    }

    /// A right password leaves nothing to export.
    #[test]
    fn a_pass_exports_an_empty_state() {
        let mut gate = Attempts::default();
        let t0 = Instant::now();
        let target = console("uid:1500");
        attempt(&mut gate, target, t0, &mut || Outcome::Failed).unwrap();
        assert_ne!(gate.export(t0, UNIX), Saved::default());
        attempt(&mut gate, target, t0 + 4 * SECOND, &mut || Outcome::Passed).unwrap();
        assert_eq!(gate.export(t0 + 4 * SECOND, UNIX), Saved::default());
    }

    /// An import drops a failure a day old and counts one stamped in the future as now. A file
    /// that isn't JSON of this shape, or holds too many accounts, parses as nothing.
    #[test]
    fn an_import_drops_a_day_old_failure_and_clamps_a_future_one() {
        let day = i64::try_from(WINDOW_MAX.as_secs()).unwrap();
        let json = format!(
            r#"{{"accounts":{{"uid:1001":{{"failed":[{},{},{}],"streak":3}},
                "uid:1002":{{"failed":[{}],"streak":1}}}}}}"#,
            UNIX - day,
            UNIX,
            UNIX + 600,
            UNIX - day,
        );
        let mut gate = Attempts::default();
        let now = Instant::now();
        gate.import(Saved::parse(json.as_bytes()).unwrap(), now, UNIX);
        assert_eq!(gate.accounts.keys().collect::<Vec<_>>(), ["uid:1001"]);
        assert_eq!(gate.accounts["uid:1001"].failed, [now, now]);
        let who = Target {
            device: None,
            account: Some("uid:1001"),
        };
        assert_eq!(gate.admit(who, now).unwrap_err(), WINDOW);

        assert!(Saved::parse(b"not json").is_none());
        let entry = |n| format!(r#""uid:{n}":{{"failed":[{UNIX}],"streak":1}}"#);
        let many: Vec<String> = (0..=SAVED_ACCOUNTS).map(entry).collect();
        let many = format!(r#"{{"accounts":{{{}}}}}"#, many.join(","));
        assert!(Saved::parse(many.as_bytes()).is_none());
    }

    #[test]
    fn the_wait_reads_in_whole_seconds_or_minutes() {
        let read = |wait| sentence(wait, true);
        assert_eq!(
            sentence(4 * SECOND, false),
            "Too many failed attempts on this device. Try again in 4 seconds.",
            "a key proof names no password"
        );
        assert_eq!(
            read(12 * MINUTE - SECOND),
            "Too many wrong passwords. Try again in 12 minutes."
        );
        assert_eq!(
            read(4 * SECOND),
            "Too many wrong passwords. Try again in 4 seconds."
        );
        assert_eq!(
            read(Duration::from_millis(1)),
            "Too many wrong passwords. Try again in 1 second."
        );
        assert_eq!(
            read(MINUTE),
            "Too many wrong passwords. Try again in 1 minute."
        );
        assert_eq!(
            read(MINUTE + SECOND),
            "Too many wrong passwords. Try again in 2 minutes."
        );
    }
}
