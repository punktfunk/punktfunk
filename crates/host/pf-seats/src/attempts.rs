//! Attempt limits for secret checks, kept in memory: the supervisor's for passwords, the host's
//! for profile PINs. Pure: the caller passes the time.
//!
//! A device, of a seat, an account or a profile, waits `2 s × 2^failures` after each failed check
//! and [`LOCKOUT`] after the fifth. An account lets at most [`ACCOUNT_FAILURES`] failed checks
//! through in any [`WINDOW`], over all its devices, so a login stack that locks an account after
//! three (`pam_faillock`) is never driven there from here; [`Attempts::with_cap`] sets another
//! cap. A check in flight counts as failed until it settles, so concurrent attempts can't share
//! one allowance. A refusal here is not a failure, a pass clears its device and account, and an
//! entry goes once its last failure is a window old.

use crate::backend::BackendError;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const BACKOFF: Duration = Duration::from_secs(2);
/// Failures after which a device waits [`LOCKOUT`].
const DEVICE_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Failed checks an account takes in any [`WINDOW`].
const ACCOUNT_FAILURES: usize = 2;
const WINDOW: Duration = Duration::from_secs(15 * 60);

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

#[derive(Debug)]
pub struct Attempts {
    devices: HashMap<(String, String), Device>,
    accounts: HashMap<String, Account>,
    /// Failed checks an account takes in any [`WINDOW`].
    cap: usize,
}

impl Default for Attempts {
    fn default() -> Self {
        Self::with_cap(ACCOUNT_FAILURES)
    }
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
}

impl Attempts {
    /// Limits whose account side takes `cap` failed checks in any [`WINDOW`].
    pub fn with_cap(cap: usize) -> Self {
        Self {
            devices: HashMap::new(),
            accounts: HashMap::new(),
            cap,
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
            .map_or(Duration::ZERO, |a| a.wait(now, self.cap));
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
                Outcome::Passed => entry.failed.clear(),
                Outcome::Failed => entry.failed.push(now),
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
        let account = target
            .account
            .and_then(|account| self.accounts.get(account))
            .map_or(0, |a| a.failed.len());
        (device, account)
    }

    /// Drops what is a window old. A check that never settled ages out as a failure would.
    fn expire(&mut self, now: Instant) {
        let live = |at: &Instant| now.saturating_duration_since(*at) < WINDOW;
        self.devices.retain(|_, d| {
            d.pending = d.pending.filter(live);
            d.pending.is_some() || d.last.is_some_and(|(at, _)| live(&at))
        });
        self.accounts.retain(|_, a| {
            a.failed.retain(live);
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
    /// Zero while fewer than `cap` failures and checks in flight count, else until all but
    /// `cap - 1` of them have left the window.
    fn wait(&self, now: Instant, cap: usize) -> Duration {
        let mut counted: Vec<Instant> = self.failed.iter().chain(&self.pending).copied().collect();
        if counted.len() < cap {
            return Duration::ZERO;
        }
        counted.sort_unstable();
        (counted[counted.len() - cap] + WINDOW).saturating_duration_since(now)
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

    /// Another cap lets that many failures through the window, over all devices, then waits
    /// until the oldest is a window old.
    #[test]
    fn a_wider_cap_takes_that_many_failures() {
        let mut gate = Attempts::with_cap(10);
        let t0 = Instant::now();
        let from = |device| Target {
            device: Some(("profile", device)),
            account: Some("profile"),
        };
        let devices = ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j"];
        for (n, device) in devices.into_iter().enumerate() {
            let at = t0 + SECOND * n as u32;
            attempt(&mut gate, from(device), at, &mut || Outcome::Failed).unwrap();
        }
        let refused = gate.admit(from("k"), t0 + 10 * SECOND).unwrap_err();
        assert_eq!(refused, WINDOW - 10 * SECOND);
        assert!(gate.admit(from("k"), t0 + WINDOW).is_ok());
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

    /// Entries go once their last failure is 15 minutes old, a check that never settled with them.
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
        assert!(gate.devices.is_empty() && gate.accounts.is_empty());
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
