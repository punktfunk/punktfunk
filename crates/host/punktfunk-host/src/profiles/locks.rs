//! Each profile's lock, in `profile-locks.json` beside `profiles.json` (`pf_seats::locks`). The
//! box's own host, the plain host or the door, is its one writer, owner-only; a seat host follows
//! the box's file read only, a Linux one through the supervisor's copy that holds no PIN.
//!
//! A file that is there and can't be read locks every profile, refuses every edit, and is logged.
//! A file that isn't there locks nothing. Wrong PINs and key proofs count per device in memory,
//! as an account password's do; wrong PINs also per profile in the file
//! ([`pf_seats::locks::PIN_FAILURES`]), so a restart clears no profile's count. A key proof
//! answers a challenge from memory, as the seat supervisor's does.

use super::{EditError, Profiles};
use pf_seats::attempts::{Attempts, Outcome, Target};
use pf_seats::ipc::{Device, UnlockKey};
use pf_seats::locks::{Lock, Locks, Opens};
use pf_seats::unlock::Challenges;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

pub(crate) struct LockStore {
    state: Mutex<State>,
    attempts: Mutex<Attempts>,
    /// Each device's live challenge, by the profile id the connect names.
    challenges: Mutex<Challenges<String>>,
}

struct State {
    path: PathBuf,
    /// `unreadable` when the file is there and didn't read.
    locks: Locks,
    /// The box's file on a seat host: never written, re-read when its mtime moves.
    read_only: bool,
    stamp: Option<SystemTime>,
}

/// How a PIN try ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PinAnswer {
    /// Right: the device is on the list.
    Opened,
    Wrong,
    /// Too many wrong PINs, from this device or for this profile: try again after this.
    Wait(Duration),
    /// The profile takes no PIN now.
    NoPin,
    /// The list is full.
    Full,
    /// Right, and the list couldn't be saved.
    NotSaved,
    /// The key proof didn't prove the device: no live challenge, no key on the list for it, or
    /// a signature over anything else.
    KeyFailed,
}

impl LockStore {
    /// `path` is the file; `read_only` on a seat host.
    pub(crate) fn open(path: PathBuf, read_only: bool) -> Self {
        let locks = read(&path, read_only);
        LockStore {
            state: Mutex::new(State {
                stamp: read_only.then(|| mtime(&path)).flatten(),
                path,
                locks,
                read_only,
            }),
            attempts: Mutex::new(Attempts::default()),
            challenges: Mutex::new(Challenges::default()),
        }
    }

    /// The state, re-read first on a seat host when the box's file changed.
    fn lock(&self) -> MutexGuard<'_, State> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.read_only {
            let now = mtime(&state.path);
            if now != state.stamp {
                state.locks = read(&state.path, true);
                state.stamp = now;
            }
        }
        state
    }

    fn attempts(&self) -> MutexGuard<'_, Attempts> {
        self.attempts.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn challenges(&self) -> MutexGuard<'_, Challenges<String>> {
        self.challenges.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// What device `fp` needs to open profile `id` now. A seat host never takes a proof.
    pub(crate) fn opens(&self, id: &str, fp: Option<&str>) -> Opens {
        let state = self.lock();
        match state.locks.opens(id, fp, crate::clock::unix_secs()) {
            Opens::Pin | Opens::Key if state.read_only => Opens::No,
            opens => opens,
        }
    }

    /// The unlock key `holder`'s list holds for `fp`, while the profile is locked and the box's
    /// file reads.
    fn key_of(&self, holder: &str, fp: &str) -> Option<UnlockKey> {
        let state = self.lock();
        let usable = !state.read_only && !state.locks.unreadable;
        let lock = state.locks.profiles.get(holder).filter(|l| l.locked);
        lock.filter(|_| usable)?.list.key_of(fp).cloned()
    }

    /// 32 fresh bytes for `fp`'s key to sign when it opens profile `asked`, whose lock is
    /// `holder`'s; they replace its last. `None` when the list holds no key for it.
    pub(crate) fn challenge(&self, asked: &str, holder: &str, fp: &str) -> Option<[u8; 32]> {
        self.key_of(holder, fp)?;
        let mut bytes = [0_u8; 32];
        aws_lc_rs::rand::fill(&mut bytes).ok()?;
        let asked = asked.to_string();
        self.challenges().mint(&asked, fp, bytes, Instant::now());
        Some(bytes)
    }

    /// Checks `signature`, from `device`'s enrolled key over `asked`'s live challenge and
    /// `box_fp`, within the device's attempt limits; right, the device is present on `holder`'s
    /// list. The challenge is used up whatever the outcome. A failure never counts against the
    /// profile's PINs. Blocking.
    pub(crate) fn try_key(
        &self,
        (asked, holder): (&str, &str),
        device: &Device,
        signature: &[u8],
        box_fp: &[u8; 32],
    ) -> PinAnswer {
        let fp = device.fingerprint.as_str();
        let challenge = self
            .challenges()
            .take(&asked.to_string(), fp, Instant::now());
        let Some(key) = self.key_of(holder, fp) else {
            return PinAnswer::KeyFailed;
        };
        let target = Target {
            device: Some((holder, fp)),
            account: None,
        };
        let counts = |store: &Self| {
            let device = store.attempts().failures(target, Instant::now()).0;
            (device, store.failures(holder))
        };
        let admitted = self.attempts().admit(target, Instant::now());
        let ticket = match admitted {
            Ok(ticket) => ticket,
            Err(wait) => {
                audit(holder, fp, "key_locked_out", counts(self));
                return PinAnswer::Wait(wait);
            }
        };
        let device_fp = pf_seats::unlock::fingerprint_bytes(fp);
        let message = challenge.zip(device_fp).and_then(|(c, d)| {
            let built = punktfunk_core::quic::v2::msg::profile_unlock_signed_message;
            built(&c, asked, &d, box_fp).ok()
        });
        let right = message.is_some_and(|m| pf_seats::unlock::verify(&key, &m, signature));
        let outcome = if right {
            Outcome::Passed
        } else {
            Outcome::Failed
        };
        self.attempts()
            .settle(target, ticket, outcome, Instant::now());
        if !right {
            audit(holder, fp, "key_invalid", counts(self));
            return PinAnswer::KeyFailed;
        }
        let granted = self.edit(holder, |lock, now| {
            let same = lock.locked && lock.list.key_of(fp) == Some(&key);
            same && lock.list.grant(device, now, false, None)
        });
        let answer = match granted {
            Ok(true) => PinAnswer::Opened,
            Ok(false) => PinAnswer::KeyFailed,
            Err(e) => {
                tracing::warn!(profile = %holder, error = %e, "device a key let in not saved");
                PinAnswer::NotSaved
            }
        };
        let result = match answer {
            PinAnswer::Opened => "key_opened",
            PinAnswer::NotSaved => "not_saved",
            _ => "key_invalid",
        };
        audit(holder, fp, result, counts(self));
        answer
    }

    /// Whether any profile is locked, or the file can't be read.
    pub(crate) fn any_locked(&self) -> bool {
        let state = self.lock();
        state.locks.unreadable || state.locks.profiles.values().any(|l| l.locked)
    }

    /// Profile `id`'s lock as it is now, its expired presence dropped; open when it has none.
    pub(crate) fn get(&self, id: &str) -> Lock {
        let mut lock = self
            .lock()
            .locks
            .profiles
            .get(id)
            .cloned()
            .unwrap_or_default();
        lock.list.expire(crate::clock::unix_secs());
        lock
    }

    /// Whether the file is there and can't be read.
    pub(crate) fn unreadable(&self) -> bool {
        self.lock().locks.unreadable
    }

    /// Wrong PINs for profile `id` in the last 15 minutes, over all devices.
    pub(crate) fn failures(&self, id: &str) -> usize {
        self.get(id).wrong_pins(crate::clock::unix_secs())
    }

    /// Applies `change` to profile `id`'s lock, its expired presence dropped, and saves the file
    /// when that changed it. Refused on a seat host and while the file can't be read.
    pub(crate) fn edit<T>(
        &self,
        id: &str,
        change: impl FnOnce(&mut Lock, i64) -> T,
    ) -> Result<T, EditError> {
        let mut state = self.lock();
        if state.read_only {
            return Err(EditError::Store(anyhow::anyhow!(
                "profile locks are the box's; a seat host never edits them"
            )));
        }
        if state.locks.unreadable {
            return Err(EditError::Store(anyhow::anyhow!(
                "{} did not read; fix or remove it before changing a profile's lock",
                state.path.display()
            )));
        }
        let now = crate::clock::unix_secs();
        let mut next = state.locks.clone();
        let lock = next.profiles.entry(id.to_string()).or_default();
        lock.list.expire(now);
        let out = change(lock, now);
        if *lock == Lock::default() {
            next.profiles.remove(id);
        }
        if next != state.locks {
            next.version = 1;
            let bytes = serde_json::to_vec_pretty(&next).map_err(|e| EditError::Store(e.into()))?;
            pf_paths::replace_secret_file(&state.path, &bytes).map_err(|e| {
                EditError::Store(anyhow::anyhow!("save {}: {e}", state.path.display()))
            })?;
            state.locks = next;
        }
        Ok(out)
    }

    /// Drops profile `id`'s lock: the profile is gone.
    pub(crate) fn forget(&self, id: &str) {
        let known = self.lock().locks.profiles.contains_key(id);
        if known {
            if let Err(e) = self.edit(id, |lock, _| *lock = Lock::default()) {
                tracing::warn!(profile = %id, error = %e, "removed profile's lock not dropped");
            }
        }
    }

    /// Checks `pin` for `device` on profile `id` within the attempt limits; right, the device
    /// joins the list, for now or, with `stay`, until it is signed out, or with `enroll` as its
    /// unlock key. Blocking: the hash is slow. Nothing logged holds the PIN.
    pub(crate) fn try_pin(
        &self,
        id: &str,
        device: &Device,
        pin: &str,
        (stay, enroll): (bool, Option<UnlockKey>),
    ) -> PinAnswer {
        let hash = {
            let state = self.lock();
            let lock = state.locks.profiles.get(id).filter(|l| l.locked);
            let usable = !state.read_only && !state.locks.unreadable;
            match lock.and_then(|l| l.pin.clone()).filter(|_| usable) {
                Some(hash) => hash,
                None => return PinAnswer::NoPin,
            }
        };
        let fp = device.fingerprint.as_str();
        let target = Target {
            device: Some((id, fp)),
            account: None,
        };
        let counts = |store: &Self| {
            let device = store.attempts().failures(target, Instant::now()).0;
            (device, store.failures(id))
        };
        let admitted = self.attempts().admit(target, Instant::now());
        let ticket = match admitted {
            Ok(ticket) => ticket,
            Err(wait) => {
                audit(id, fp, "locked_out", counts(self));
                return PinAnswer::Wait(wait);
            }
        };
        // Counted in the file before the hash runs: a restart or a crash forgets no try.
        let charged = self.edit(id, |lock, now| lock.charge_pin(now).map(|()| now));
        let unjudged = |ticket| {
            let now = Instant::now();
            self.attempts()
                .settle(target, ticket, Outcome::Unjudged, now);
        };
        let at = match charged {
            Ok(Ok(at)) => at,
            Ok(Err(secs)) => {
                unjudged(ticket);
                audit(id, fp, "locked_out", counts(self));
                return PinAnswer::Wait(Duration::from_secs(secs.unsigned_abs()));
            }
            Err(e) => {
                unjudged(ticket);
                tracing::warn!(profile = %id, error = %e, "PIN try not counted");
                return PinAnswer::NotSaved;
            }
        };
        let right = hash.verify(pin);
        let outcome = if right {
            Outcome::Passed
        } else {
            Outcome::Failed
        };
        self.attempts()
            .settle(target, ticket, outcome, Instant::now());
        if !right {
            audit(id, fp, "wrong_pin", counts(self));
            return PinAnswer::Wrong;
        }
        let granted = self.edit(id, |lock, now| {
            lock.refund_pin(at);
            // The PIN changed while this one was checked: it opens nothing.
            if lock.pin.as_ref() != Some(&hash) || !lock.locked {
                return PinAnswer::NoPin;
            }
            let joined = match enroll {
                Some(key) => lock.enrol(device, now, key),
                None => lock.admit(device, now, stay),
            };
            match joined {
                true => PinAnswer::Opened,
                false => PinAnswer::Full,
            }
        });
        let counts = counts(self);
        let answer = granted.unwrap_or_else(|e| {
            tracing::warn!(profile = %id, error = %e, "device a PIN let in not saved");
            PinAnswer::NotSaved
        });
        let result = match answer {
            PinAnswer::Opened => "opened",
            PinAnswer::Full => "list_full",
            _ => "not_saved",
        };
        audit(id, fp, result, counts);
        answer
    }

    /// `fp` connects again to profile `id`: a presence that was ending lasts again. Whether the
    /// profile is locked and lists the device, so its leaving is to be told.
    pub(crate) fn rejoin(&self, id: &str, fp: &str) -> bool {
        let listed = {
            let state = self.lock();
            let lock = state.locks.profiles.get(id).filter(|l| l.locked);
            !state.read_only
                && lock.is_some_and(|l| l.admitted(crate::clock::unix_secs()).contains(fp))
        };
        listed
            && self
                .edit(id, |lock, now| lock.list.rejoin(fp, now))
                .unwrap_or_else(|e| {
                    tracing::warn!(profile = %id, error = %e, "returning device not saved");
                    true
                })
    }

    /// `fp` left profile `id`: a presence from a PIN ends after its grace.
    fn left(&self, id: &str, fp: &str) {
        let edited = self.edit(id, |lock, now| lock.list.left(fp, now));
        if let Err(e) = edited {
            tracing::warn!(profile = %id, error = %e, "departed device not saved");
        }
    }
}

/// One line per PIN try: the profile, the device, how it ended and the failures counted. Never
/// the PIN.
fn audit(id: &str, fp: &str, result: &str, (device, profile): (u32, usize)) {
    tracing::info!(
        target: "punktfunk::audit",
        profile = %id,
        device = %fp,
        result,
        device_failures = device,
        profile_failures = profile,
        "profile PIN checked"
    );
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The file at `path`: none when it isn't there, every profile locked when it can't be read. A
/// file a non-admin planted before the box host's first run is quarantined: none.
fn read(path: &Path, read_only: bool) -> Locks {
    if !read_only && crate::planted::quarantine_planted_secret(path) {
        return Locks::default();
    }
    let read = match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Locks::default(),
        Err(e) => Err(e.to_string()),
        Ok(bytes) => Locks::parse(&bytes).ok_or_else(|| "not a lock file".to_string()),
    };
    read.unwrap_or_else(|error| {
        tracing::error!(path = %path.display(), %error,
            "profile lock file unreadable; every profile is locked until it is fixed");
        Locks {
            unreadable: true,
            ..Locks::default()
        }
    })
}

/// Each (profile, device) a session holds a presence on, and how many sessions. The last to go
/// starts the device's grace, under this lock, so a grace never lands after the rejoin that
/// followed it.
static HELD: Mutex<BTreeMap<(String, String), u32>> = Mutex::new(BTreeMap::new());

fn held() -> MutexGuard<'static, BTreeMap<(String, String), u32>> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// A session's hold on a device's place in a locked profile. Dropped by the last session, the
/// device's presence from a PIN ends after its grace. Drop writes the file: blocking.
pub(crate) struct Presence {
    profiles: Arc<Profiles>,
    key: (String, String),
}

impl Presence {
    /// `fp`'s hold on profile `id`'s lock ([`Profiles::lock_holder`]), when it is locked and
    /// lists the device: its presence lasts while this is held.
    pub(crate) fn join(profiles: &Arc<Profiles>, id: &str, fp: &str) -> Option<Presence> {
        let fp = fp.to_ascii_lowercase();
        let key = (profiles.lock_holder(id), fp);
        let mut held = held();
        if !profiles.locks.rejoin(&key.0, &key.1) {
            return None;
        }
        *held.entry(key.clone()).or_insert(0) += 1;
        Some(Presence {
            profiles: Arc::clone(profiles),
            key,
        })
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        let mut held = held();
        match held.get_mut(&self.key) {
            Some(n) if *n > 1 => *n -= 1,
            _ => {
                held.remove(&self.key);
                self.profiles.locks.left(&self.key.0, &self.key.1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_seats::locks::PinHash;

    const KID: &str = "c1d000000001";

    fn device(n: u8) -> Device {
        Device {
            fingerprint: hex::encode([n; 32]),
            name: format!("Device {n}"),
        }
    }

    /// The box's store and its file in a dir of its own.
    fn store() -> (LockStore, PathBuf) {
        let path = crate::test_support::scratch().join(pf_seats::locks::FILE);
        (LockStore::open(path.clone(), false), path)
    }

    /// No file locks nothing. A locked profile opens for its list alone, and the file the box
    /// writes is its user's alone.
    #[test]
    fn a_locked_profile_opens_for_its_list_alone() {
        let (locks, path) = store();
        let (tv, phone) = (device(1), device(2));
        assert_eq!(locks.opens(KID, Some(&tv.fingerprint)), Opens::Yes);
        assert!(!path.exists(), "an open profile writes nothing");
        locks
            .edit(KID, |lock, now| {
                lock.locked = true;
                lock.admit(&tv, now, true)
            })
            .unwrap();
        assert_eq!(locks.opens(KID, Some(&tv.fingerprint)), Opens::Yes);
        assert_eq!(locks.opens(KID, Some(&phone.fingerprint)), Opens::No);
        let other = locks.opens("0123456789ab", Some(&phone.fingerprint));
        assert_eq!(other, Opens::Yes);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let again = LockStore::open(path, false);
        assert_eq!(again.opens(KID, Some(&phone.fingerprint)), Opens::No);
        locks.forget(KID);
        assert_eq!(locks.opens(KID, Some(&phone.fingerprint)), Opens::Yes);
    }

    /// A file that is there and doesn't read locks every profile and takes no edit.
    #[test]
    fn a_file_that_does_not_read_locks_everything() {
        let bad: [&[u8]; 2] = [
            b"not json",
            br#"{"profiles":{"x":{"devices":[{"fingerprint":"AB"}]}}}"#,
        ];
        for bytes in bad {
            let path = crate::test_support::scratch().join(pf_seats::locks::FILE);
            std::fs::write(&path, bytes).unwrap();
            let locks = LockStore::open(path.clone(), false);
            assert!(locks.unreadable() && locks.any_locked());
            assert_eq!(locks.opens(KID, Some(&device(1).fingerprint)), Opens::No);
            assert!(locks.edit(KID, |lock, _| lock.locked = false).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes, "kept as it was");
        }
    }

    /// A seat host follows the box's file, writes nothing and never takes a PIN.
    #[test]
    fn a_seat_follows_the_box_and_takes_no_pin() {
        let (locks, path) = store();
        let seat = LockStore::open(path.clone(), true);
        let tv = device(1).fingerprint;
        assert_eq!(seat.opens(KID, Some(&tv)), Opens::Yes);
        locks
            .edit(KID, |lock, _| {
                lock.locked = true;
                lock.pin = PinHash::new("2468");
            })
            .unwrap();
        // A later stamp than the seat read, whatever the filesystem's clock granularity.
        let later = SystemTime::now() + Duration::from_secs(5);
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(later).unwrap();
        assert_eq!(locks.opens(KID, Some(&tv)), Opens::Pin);
        assert_eq!(
            seat.opens(KID, Some(&tv)),
            Opens::No,
            "the box takes the PIN"
        );
        assert!(seat.edit(KID, |lock, _| lock.locked = false).is_err());
        let tried = seat.try_pin(KID, &device(1), "2468", (false, None));
        assert_eq!(tried, PinAnswer::NoPin);
    }

    /// Wrong PINs count per device as a password's do, and ten across devices lock PIN entry for
    /// the profile, the right PIN included; a device on its list keeps opening it. A right PIN
    /// puts the device on the list, for now or until signed out.
    #[test]
    fn ten_wrong_pins_close_the_pin_to_every_device() {
        let (locks, path) = store();
        let listed = device(99);
        locks
            .edit(KID, |lock, now| {
                lock.locked = true;
                lock.pin = PinHash::new("2468");
                lock.admit(&listed, now, true)
            })
            .unwrap();
        assert_eq!(
            locks.try_pin(KID, &device(1), "1357", (false, None)),
            PinAnswer::Wrong
        );
        let waits = locks.try_pin(KID, &device(1), "2468", (false, None));
        assert!(
            matches!(waits, PinAnswer::Wait(_)),
            "the device waits: {waits:?}"
        );
        for n in 2..=10 {
            assert_eq!(
                locks.try_pin(KID, &device(n), "0000", (false, None)),
                PinAnswer::Wrong
            );
        }
        assert_eq!(locks.failures(KID), 10);
        let closed = locks.try_pin(KID, &device(11), "2468", (false, None));
        let fourteen = Duration::from_secs(14 * 60);
        assert!(
            matches!(closed, PinAnswer::Wait(w) if w > fourteen),
            "{closed:?}"
        );
        assert_eq!(locks.opens(KID, Some(&listed.fingerprint)), Opens::Yes);
        assert_eq!(locks.opens(KID, Some(&device(11).fingerprint)), Opens::Pin);
        // A restart forgets no wrong PIN: the profile's count lives in its file.
        let restarted = LockStore::open(path, false);
        assert_eq!(restarted.failures(KID), 10);
        let still = restarted.try_pin(KID, &device(12), "2468", (false, None));
        assert!(
            matches!(still, PinAnswer::Wait(w) if w > fourteen),
            "{still:?}"
        );

        let (fresh, _) = store();
        fresh
            .edit(KID, |lock, _| {
                lock.locked = true;
                lock.pin = PinHash::new("2468");
            })
            .unwrap();
        let (tv, phone) = (device(1), device(2));
        assert_eq!(
            fresh.try_pin(KID, &tv, "2468", (true, None)),
            PinAnswer::Opened
        );
        assert_eq!(
            fresh.try_pin(KID, &phone, "2468", (false, None)),
            PinAnswer::Opened
        );
        let kinds: Vec<_> = fresh
            .get(KID)
            .list
            .entries()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        use pf_seats::ipc::DeviceKind;
        assert_eq!(kinds, [DeviceKind::Stay, DeviceKind::ForNow]);
        assert_eq!(fresh.opens(KID, Some(&phone.fingerprint)), Opens::Yes);
    }

    /// A right PIN enrols the key it brings. Once the device left, a signature opens the profile
    /// only over a live challenge of this profile, this device and this host, and each challenge
    /// answers once. A failed proof costs the device's allowance, never the profile's PINs.
    #[test]
    fn a_key_opens_a_pin_profile_once_per_challenge() {
        use aws_lc_rs::rand::SystemRandom;
        use aws_lc_rs::signature::{EcdsaKeyPair, KeyPair as _, ECDSA_P256_SHA256_ASN1_SIGNING};
        use punktfunk_core::quic::v2::msg::profile_unlock_signed_message;
        let (locks, path) = store();
        locks
            .edit(KID, |lock, _| {
                lock.locked = true;
                lock.pin = PinHash::new("2468");
            })
            .unwrap();
        let pair = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING).unwrap();
        let key = UnlockKey {
            alg: pf_seats::ipc::KeyAlg::P256,
            public: hex::encode(pair.public_key().as_ref()),
        };
        let host = [0xbb; 32];
        let ways = (KID, KID);
        // Each device on the list with the key, its presence over. The PIN's hash is slow, so
        // only the first types it.
        let enrol = |n: u8| {
            let d = device(n);
            if n == 1 {
                let tried = locks.try_pin(KID, &d, "2468", (false, Some(key.clone())));
                assert_eq!(tried, PinAnswer::Opened);
            } else {
                let listed = locks.edit(KID, |lock, now| lock.enrol(&d, now, key.clone()));
                assert!(listed.unwrap());
            }
            let gone = |lock: &mut Lock, now: i64| lock.list.left(&d.fingerprint, now - 601);
            locks.edit(KID, gone).unwrap();
            assert_eq!(locks.opens(KID, Some(&d.fingerprint)), Opens::Key);
            d
        };
        let sign = |n: u8, id: &str, host: &[u8; 32]| {
            let c = locks.challenge(KID, KID, &device(n).fingerprint).unwrap();
            let m = profile_unlock_signed_message(&c, id, &[n; 32], host).unwrap();
            pair.sign(&SystemRandom::new(), &m)
                .unwrap()
                .as_ref()
                .to_vec()
        };
        let tv = enrol(1);
        let signed = sign(1, KID, &host);
        assert_eq!(locks.try_key(ways, &tv, &signed, &host), PinAnswer::Opened);
        assert_eq!(locks.opens(KID, Some(&tv.fingerprint)), Opens::Yes);
        let again = locks.try_key(ways, &tv, &signed, &host);
        assert_eq!(again, PinAnswer::KeyFailed, "used up");
        let phone = enrol(2);
        let other = sign(2, "0123456789ab", &host);
        let tried = locks.try_key(ways, &phone, &other, &host);
        assert_eq!(tried, PinAnswer::KeyFailed, "another profile");
        let deck = enrol(3);
        let elsewhere = sign(3, KID, &[0xcc; 32]);
        let tried = locks.try_key(ways, &deck, &elsewhere, &host);
        assert_eq!(tried, PinAnswer::KeyFailed, "another host");
        assert!(matches!(
            locks.try_key(ways, &deck, &sign(3, KID, &host), &host),
            PinAnswer::Wait(_)
        ));
        assert_eq!(locks.failures(KID), 0, "the profile's PINs are untouched");
        let stranger = device(9).fingerprint;
        assert_eq!(locks.challenge(KID, KID, &stranger), None, "no key");
        let seat = LockStore::open(path, true);
        assert_eq!(seat.challenge(KID, KID, &tv.fingerprint), None);
        assert_eq!(seat.opens(KID, Some(&deck.fingerprint)), Opens::No);
    }

    /// A device signed in for now stays while any of its sessions plays, and leaves ten minutes
    /// after the last ends.
    #[test]
    fn a_device_for_now_leaves_after_its_last_session() {
        let dir = crate::test_support::scratch();
        let file = serde_json::json!({"version": 1, "profiles": [
            {"id": KID, "display_name": "Kid", "os_account": {"kind": "operator"}},
        ]});
        std::fs::write(dir.join("profiles.json"), file.to_string()).unwrap();
        let profiles = Arc::new(Profiles::load_with(Some(dir.join("profiles.json")), None));
        let tv = device(1);
        profiles
            .locks()
            .edit(KID, |lock, now| {
                lock.locked = true;
                lock.admit(&tv, now, false)
            })
            .unwrap();
        let ends = || {
            profiles.locks().get(KID).list.devices[0]
                .present
                .unwrap()
                .expires_unix
        };
        let first = Presence::join(&profiles, KID, &tv.fingerprint).unwrap();
        let second = Presence::join(&profiles, KID, &tv.fingerprint).unwrap();
        drop(first);
        assert_eq!(ends(), None, "a session still plays");
        drop(second);
        let left = ends().expect("its grace runs");
        assert!(left > crate::clock::unix_secs());
        assert!(Presence::join(&profiles, KID, &device(2).fingerprint).is_none());
        let back = Presence::join(&profiles, KID, &tv.fingerprint).unwrap();
        assert_eq!(ends(), None, "back within its grace");
        drop(back);
    }

    /// A profile sharing the owner's desktop opens with the owner's lock, on the box and on a
    /// seat host reading the box's file, and a session on it holds the owner's list. A
    /// take-turns profile keeps its own.
    #[test]
    fn a_profile_sharing_the_owner_s_desktop_has_the_owner_s_lock() {
        const OWNER: &str = "4f1c3a9b0e27";
        const GUEST: &str = "5a1e00000001";
        const TURNS: &str = "c1d000000002";
        let dir = crate::test_support::scratch();
        let file = serde_json::json!({"version": 1, "profiles": [
            {"id": OWNER, "display_name": "Enrico", "os_account": {"kind": "operator"}},
            {"id": GUEST, "display_name": "Guest", "os_account": {"kind": "operator"}},
            {"id": TURNS, "display_name": "Sam", "os_account": {"kind": "windows",
                "account_name": "Sam", "credential": {"source": "none"}}},
        ]});
        let path = dir.join("profiles.json");
        std::fs::write(&path, file.to_string()).unwrap();
        let profiles = Arc::new(Profiles::load_with(Some(path.clone()), None));
        let (tv, phone) = (device(1), device(2));
        assert_eq!(profiles.lock_holder(GUEST), OWNER);
        assert_eq!(profiles.lock_holder(TURNS), TURNS);
        assert_eq!(profiles.lock_holder(OWNER), OWNER);
        profiles
            .locks()
            .edit(OWNER, |lock, now| {
                lock.locked = true;
                lock.admit(&tv, now, false)
            })
            .unwrap();
        let seat = Profiles::load_box(path, None);
        for host in [&*profiles, &seat] {
            assert_eq!(host.opens(GUEST, Some(&tv.fingerprint)), Opens::Yes);
            assert_eq!(host.opens(GUEST, Some(&phone.fingerprint)), Opens::No);
            assert_eq!(host.opens(TURNS, Some(&phone.fingerprint)), Opens::Yes);
        }
        let held = Presence::join(&profiles, GUEST, &tv.fingerprint).expect("the owner's list");
        drop(held);
        let present = profiles.locks().get(OWNER).list.devices[0].present.unwrap();
        assert!(
            present.expires_unix.is_some(),
            "its grace runs on the owner's list"
        );
    }

    /// A removed profile's lock goes with it, a seat's once its seat is gone; and while
    /// `profiles.json` doesn't read, the owner's stand-in opens nothing once a profile is locked.
    #[test]
    fn a_lock_goes_with_its_profile() {
        const TURNS: &str = "c1d000000002";
        let dir = crate::test_support::scratch();
        let file = serde_json::json!({"version": 1, "profiles": [
            {"id": "4f1c3a9b0e27", "display_name": "Enrico", "os_account": {"kind": "operator"}},
            {"id": KID, "display_name": "Kid", "os_account": {"kind": "seat", "seat": "ab".repeat(16)}},
            {"id": TURNS, "display_name": "Sam", "os_account": {"kind": "windows",
                "account_name": "Sam", "credential": {"source": "none"}}},
        ]});
        std::fs::write(dir.join("profiles.json"), file.to_string()).unwrap();
        let profiles = Profiles::load_with(Some(dir.join("profiles.json")), None);
        let tv = device(1).fingerprint;
        for id in [KID, TURNS] {
            profiles
                .locks()
                .edit(id, |lock, _| lock.locked = true)
                .unwrap();
            assert_eq!(profiles.opens(id, Some(&tv)), Opens::No);
        }
        profiles.delete(TURNS, false).unwrap();
        assert_eq!(profiles.opens(TURNS, Some(&tv)), Opens::Yes);
        let seat = profiles.delete(KID, false).unwrap();
        assert_eq!(
            profiles.opens(KID, Some(&tv)),
            Opens::No,
            "until its seat goes"
        );
        profiles.restore(seat, false).unwrap();
        assert_eq!(profiles.opens(KID, Some(&tv)), Opens::No, "restored locked");
        profiles.delete(KID, false).unwrap();
        profiles.erase(KID);
        assert_eq!(profiles.opens(KID, Some(&tv)), Opens::Yes);

        let stand_in = super::super::OPERATOR_PROFILE_ID;
        std::fs::write(dir.join("profiles.json"), "not json").unwrap();
        let garbled = Profiles::load_with(Some(dir.join("profiles.json")), None);
        assert_eq!(
            garbled.opens(stand_in, Some(&tv)),
            Opens::Yes,
            "nothing locked"
        );
        garbled
            .locks()
            .edit(KID, |lock, _| lock.locked = true)
            .unwrap();
        assert_eq!(garbled.opens(stand_in, Some(&tv)), Opens::No);
    }
}
