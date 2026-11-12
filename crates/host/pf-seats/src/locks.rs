//! Which paired devices may open each profile: [`FILE`], kept by the host that pairs, beside its
//! pairing store and readable by its user alone. The supervisor reads it to fill a locked
//! profile's seat copy. Pure: the caller passes the time.
//!
//! A profile with no entry, or one not `locked`, is open. A locked profile opens for the devices
//! its list admits, by [`DeviceList::admitted`]'s rules; any other device needs its PIN, and with
//! no PIN never opens it. A file that can't be read locks every profile. A seat's copy carries no
//! PIN: a seat host never takes one.
//!
//! A profile takes [`PIN_FAILURES`] wrong PINs in any [`PIN_WINDOW`], over all devices, counted
//! in the file so a restart forgets none. A try counts before its hash runs and is taken back
//! when right, so neither a crash nor tries at once get past the cap.

use crate::ipc::Device;
use crate::unlock::{lower_hex, DeviceList};
use aws_lc_rs::pbkdf2::{self, PBKDF2_HMAC_SHA256};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

/// The file's name, beside the pairing store and in each seat's trust copy.
pub const FILE: &str = "profile-locks.json";
/// A PIN is 4 to 8 ASCII digits: a gamepad and a TV remote type it.
pub const PIN_LEN: std::ops::RangeInclusive<usize> = 4..=8;
/// Wrong PINs a profile takes in any [`PIN_WINDOW`]: about 960 tries a day.
pub const PIN_FAILURES: usize = 10;
/// Seconds.
pub const PIN_WINDOW: i64 = 15 * 60;
/// OWASP's PBKDF2-HMAC-SHA256 floor in 2023. The lockout, not this, guards a short PIN.
const ITERATIONS: u32 = 600_000;
/// Rounds a stored hash may name: under this is no slow hash, over it a check that never ends.
const ITERATIONS_OK: std::ops::RangeInclusive<u32> = 100_000..=10_000_000;
const SALT_BYTES: usize = 16;
const HASH_BYTES: usize = 32;

/// The file: one entry per profile id.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Locks {
    #[serde(default)]
    pub version: u32,
    /// A seat's copy of a file the supervisor couldn't read: every profile is locked.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unreadable: bool,
    #[serde(default)]
    pub profiles: BTreeMap<String, Lock>,
}

/// One profile's lock. Unlocking keeps its PIN and devices for the next lock.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    #[serde(default)]
    pub locked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<PinHash>,
    /// When each wrong PIN, or try in flight, of the last [`PIN_WINDOW`] came, unix seconds.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wrong_pins: Vec<i64>,
    #[serde(flatten)]
    pub list: DeviceList,
}

/// What a device needs to open a profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opens {
    /// Nothing: the profile is open, or its list admits the device.
    Yes,
    /// The profile's PIN.
    Pin,
    /// Nothing opens it from this device.
    No,
}

impl Locks {
    /// The file `bytes` hold, `None` for one this crate would not have written.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let locks: Locks = serde_json::from_slice(bytes).ok()?;
        locks.valid().then_some(locks)
    }

    fn valid(&self) -> bool {
        self.profiles.values().all(|l| {
            l.list.valid()
                && l.pin.as_ref().is_none_or(PinHash::valid)
                && l.wrong_pins.len() <= PIN_FAILURES
        })
    }

    /// What device `fp`, lowercase hex or none, needs to open `profile` at `now`.
    pub fn opens(&self, profile: &str, fp: Option<&str>, now: i64) -> Opens {
        if self.unreadable {
            return Opens::No;
        }
        let Some(lock) = self.profiles.get(profile).filter(|l| l.locked) else {
            return Opens::Yes;
        };
        let Some(fp) = fp.map(str::to_ascii_lowercase) else {
            return Opens::No;
        };
        if lock.admitted(now).contains(&fp) {
            Opens::Yes
        } else if lock.pin.is_some() {
            Opens::Pin
        } else {
            Opens::No
        }
    }

    /// The devices a seat that plays `served` admits at `now`: `None` for every paired device,
    /// when it plays an open profile or none.
    pub fn admitted_for(&self, served: &[String], now: i64) -> Option<BTreeSet<String>> {
        if self.unreadable {
            return Some(BTreeSet::new());
        }
        let mut admitted = BTreeSet::new();
        for id in served {
            let lock = self.profiles.get(id).filter(|l| l.locked)?;
            admitted.extend(lock.admitted(now));
        }
        (!served.is_empty()).then_some(admitted)
    }

    /// What a seat that plays `served` reads: their entries, without a PIN.
    pub fn seat_copy(&self, served: &[String]) -> Locks {
        let entry = |id: &String| {
            let lock = self.profiles.get(id)?;
            let bare = Lock {
                pin: None,
                wrong_pins: Vec::new(),
                ..lock.clone()
            };
            Some((id.clone(), bare))
        };
        Locks {
            version: self.version,
            unreadable: self.unreadable,
            profiles: served.iter().filter_map(entry).collect(),
        }
    }
}

impl Lock {
    /// The devices that open it at `now` with nothing asked.
    pub fn admitted(&self, now: i64) -> BTreeSet<String> {
        let mut list = self.list.clone();
        list.expire(now);
        list.admitted()
    }

    /// `device` on the list from `now`: until signed out when `stay`, else while it plays and
    /// for a grace after. `false`, changing nothing, when the list is full without it.
    pub fn admit(&mut self, device: &Device, now: i64, stay: bool) -> bool {
        self.list.expire(now);
        self.list.grant(device, now, stay, None)
    }

    /// [`Self::admit`] with `stay`, marked as the operator's add: its kind stays `added` through
    /// a later PIN.
    pub fn add(&mut self, device: &Device, now: i64) -> bool {
        if !self.admit(device, now, true) {
            return false;
        }
        let fp = &device.fingerprint;
        let entry = self.list.devices.iter_mut().find(|e| &e.fingerprint == fp);
        entry.into_iter().for_each(|e| e.added = true);
        true
    }

    /// Wrong PINs, and tries in flight, counted at `now`.
    pub fn wrong_pins(&mut self, now: i64) -> usize {
        self.wrong_pins
            .retain(|at| now.saturating_sub(*at) < PIN_WINDOW);
        self.wrong_pins.len()
    }

    /// Counts a PIN try at `now` before its hash runs, or answers the seconds until one would
    /// count. [`Self::refund_pin`] takes a right one back.
    pub fn charge_pin(&mut self, now: i64) -> Result<(), i64> {
        if self.wrong_pins(now) < PIN_FAILURES {
            self.wrong_pins.push(now);
            return Ok(());
        }
        let mut counted = self.wrong_pins.clone();
        counted.sort_unstable();
        let frees = counted[counted.len() - PIN_FAILURES] + PIN_WINDOW;
        Err(frees.saturating_sub(now).max(1))
    }

    /// Takes back the try [`Self::charge_pin`] counted at `at`: its PIN was right.
    pub fn refund_pin(&mut self, at: i64) {
        if let Some(i) = self.wrong_pins.iter().position(|t| *t == at) {
            self.wrong_pins.swap_remove(i);
        }
    }
}

/// A PIN as stored: PBKDF2-HMAC-SHA256 over a random salt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinHash {
    pub iterations: u32,
    /// 16 bytes, lowercase hex.
    pub salt: String,
    /// 32 bytes, lowercase hex.
    pub hash: String,
}

impl PinHash {
    /// `pin` hashed under a fresh salt; `None` for one that is not [`valid_pin`] or a system
    /// with no randomness.
    pub fn new(pin: &str) -> Option<Self> {
        if !valid_pin(pin) {
            return None;
        }
        let mut salt = [0_u8; SALT_BYTES];
        aws_lc_rs::rand::fill(&mut salt).ok()?;
        let mut hash = [0_u8; HASH_BYTES];
        let rounds = NonZeroU32::new(ITERATIONS)?;
        pbkdf2::derive(PBKDF2_HMAC_SHA256, rounds, &salt, pin.as_bytes(), &mut hash);
        Some(Self {
            iterations: ITERATIONS,
            salt: hex::encode(salt),
            hash: hex::encode(hash),
        })
    }

    /// Whether `pin` is the one hashed, in constant time once a hash runs.
    pub fn verify(&self, pin: &str) -> bool {
        let (Some(salt), Some(hash)) = (self.salt_bytes(), self.hash_bytes()) else {
            return false;
        };
        let Some(rounds) = NonZeroU32::new(self.iterations) else {
            return false;
        };
        let shaped = valid_pin(pin) && ITERATIONS_OK.contains(&self.iterations);
        shaped && pbkdf2::verify(PBKDF2_HMAC_SHA256, rounds, &salt, pin.as_bytes(), &hash).is_ok()
    }

    fn valid(&self) -> bool {
        ITERATIONS_OK.contains(&self.iterations)
            && self.salt_bytes().is_some()
            && self.hash_bytes().is_some()
    }

    fn salt_bytes(&self) -> Option<Vec<u8>> {
        lower_hex(&self.salt, SALT_BYTES).filter(|b| b.len() == SALT_BYTES)
    }

    fn hash_bytes(&self) -> Option<Vec<u8>> {
        lower_hex(&self.hash, HASH_BYTES).filter(|b| b.len() == HASH_BYTES)
    }
}

/// 4 to 8 ASCII digits.
pub fn valid_pin(pin: &str) -> bool {
    PIN_LEN.contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn device(n: u8) -> Device {
        Device {
            fingerprint: hex::encode([n; 32]),
            name: format!("Device {n}"),
        }
    }

    /// Anna's profile locked to her phone, kept signed in, with a PIN; Ben's open.
    fn locks() -> Locks {
        let mut anna = Lock {
            locked: true,
            pin: PinHash::new("2468"),
            ..Lock::default()
        };
        assert!(anna.admit(&device(1), NOW, true));
        let ben = Lock {
            locked: false,
            ..Lock::default()
        };
        Locks {
            version: 1,
            unreadable: false,
            profiles: BTreeMap::from([("anna".into(), anna), ("ben".into(), ben)]),
        }
    }

    #[test]
    fn a_locked_profile_opens_for_its_devices_and_asks_the_rest_for_its_pin() {
        let mut locks = locks();
        let (phone, tv) = (device(1).fingerprint, device(2).fingerprint);
        assert_eq!(locks.opens("anna", Some(&phone), NOW), Opens::Yes);
        assert_eq!(
            locks.opens("anna", Some(&phone.to_uppercase()), NOW),
            Opens::Yes
        );
        assert_eq!(locks.opens("anna", Some(&tv), NOW), Opens::Pin);
        assert_eq!(locks.opens("anna", None, NOW), Opens::No, "no device");
        for open in ["ben", "carl"] {
            assert_eq!(locks.opens(open, Some(&tv), NOW), Opens::Yes, "{open}");
            assert_eq!(locks.opens(open, None, NOW), Opens::Yes, "{open}");
        }
        locks.profiles.get_mut("anna").unwrap().pin = None;
        assert_eq!(locks.opens("anna", Some(&tv), NOW), Opens::No, "no PIN");
        locks.unreadable = true;
        for profile in ["anna", "ben", "carl"] {
            assert_eq!(locks.opens(profile, Some(&phone), NOW), Opens::No);
        }
    }

    /// A PIN's "for now" lasts while the device plays and ten minutes after it left.
    #[test]
    fn a_device_signed_in_for_now_leaves_after_its_grace() {
        let mut locks = locks();
        let tv = device(2);
        let anna = locks.profiles.get_mut("anna").unwrap();
        assert!(anna.admit(&tv, NOW, false));
        anna.list.left(&tv.fingerprint, NOW + 10);
        let fp = Some(tv.fingerprint.as_str());
        assert_eq!(locks.opens("anna", fp, NOW + 609), Opens::Yes);
        assert_eq!(locks.opens("anna", fp, NOW + 610), Opens::Pin);
    }

    /// A seat admits every paired device while it plays an open profile, else the union of its
    /// locked profiles' devices; a file it can't read admits none.
    #[test]
    fn a_seat_admits_what_its_profiles_admit() {
        let mut locks = locks();
        let anna = |ids: &[&str]| ids.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let phone = device(1).fingerprint;
        assert_eq!(
            locks.admitted_for(&anna(&["anna"]), NOW),
            Some(BTreeSet::from([phone.clone()]))
        );
        assert_eq!(locks.admitted_for(&anna(&["anna", "ben"]), NOW), None);
        assert_eq!(locks.admitted_for(&anna(&["anna", "carl"]), NOW), None);
        assert_eq!(locks.admitted_for(&[], NOW), None, "plays nobody");
        locks.profiles.get_mut("ben").unwrap().locked = true;
        assert_eq!(
            locks.admitted_for(&anna(&["anna", "ben"]), NOW),
            Some(BTreeSet::from([phone]))
        );
        locks.unreadable = true;
        assert_eq!(locks.admitted_for(&[], NOW), Some(BTreeSet::new()));
    }

    #[test]
    fn a_seat_s_copy_holds_its_own_profiles_and_no_pin() {
        let locks = locks();
        let copy = locks.seat_copy(&["anna".to_string(), "carl".to_string()]);
        assert_eq!(copy.profiles.keys().collect::<Vec<_>>(), ["anna"]);
        let anna = &copy.profiles["anna"];
        assert!(anna.locked && anna.pin.is_none());
        assert_eq!(anna.list, locks.profiles["anna"].list);
        let text = serde_json::to_string(&copy).unwrap();
        assert!(!text.contains("salt") && !text.contains("hash"), "{text}");
        assert_eq!(Locks::parse(text.as_bytes()), Some(copy));
    }

    #[test]
    fn a_pin_is_four_to_eight_digits_and_hashed_with_its_own_salt() {
        for good in ["0000", "12345678"] {
            assert!(valid_pin(good), "{good}");
        }
        for bad in ["", "123", "123456789", "12a4", " 1234", "１２３４"] {
            assert!(!valid_pin(bad), "{bad:?}");
            assert!(PinHash::new(bad).is_none(), "{bad:?}");
        }
        let (one, two) = (PinHash::new("2468").unwrap(), PinHash::new("2468").unwrap());
        assert_ne!(one.salt, two.salt);
        assert!(one.verify("2468") && two.verify("2468"));
        for wrong in ["2469", "24680", "hunter2", ""] {
            assert!(!one.verify(wrong), "{wrong:?}");
        }
        let weak = PinHash {
            iterations: 1,
            ..one.clone()
        };
        assert!(!weak.verify("2468"), "a hash too fast to be one");
        let short = PinHash {
            salt: "ab".into(),
            ..one
        };
        assert!(!short.verify("2468"));
    }

    /// Ten tries count in any window, a right one is taken back, and the eleventh waits until
    /// the oldest counted is a window old.
    #[test]
    fn a_profile_takes_ten_wrong_pins_a_window() {
        let mut lock = Lock::default();
        for n in 0..10 {
            assert_eq!(lock.charge_pin(NOW + n), Ok(()));
        }
        assert_eq!(lock.charge_pin(NOW + 10), Err(PIN_WINDOW - 10));
        lock.refund_pin(NOW + 3);
        assert_eq!(
            lock.charge_pin(NOW + 10),
            Ok(()),
            "a right one is taken back"
        );
        assert_eq!(lock.charge_pin(NOW + PIN_WINDOW), Ok(()), "the oldest left");
        assert_eq!(lock.wrong_pins(NOW + 2 * PIN_WINDOW), 0);
    }

    /// The operator's add reads as `added` through a later PIN, and as `key` once a key is
    /// enrolled. An entry written before the marker keeps the kind it had.
    #[test]
    fn a_device_the_operator_added_says_so() {
        use crate::ipc::DeviceKind;
        let mut lock = Lock::default();
        assert!(lock.add(&device(1), NOW));
        assert!(lock.admit(&device(1), NOW + 1, false), "then types the PIN");
        assert!(lock.admit(&device(2), NOW, true));
        let kinds = |lock: &Lock| {
            lock.list
                .entries()
                .into_iter()
                .map(|e| e.kind)
                .collect::<Vec<_>>()
        };
        assert_eq!(kinds(&lock), [DeviceKind::Added, DeviceKind::Stay]);
        let later = lock.admitted(NOW + 2 * crate::unlock::CAP_SECS);
        assert_eq!(later.len(), 2, "both stay");
        let key = crate::ipc::UnlockKey {
            alg: crate::ipc::KeyAlg::P256,
            public: hex::encode([4_u8; 65]),
        };
        lock.list.devices[0].key = Some(key);
        assert_eq!(kinds(&lock), [DeviceKind::Key, DeviceKind::Stay]);

        let old = r#"{"devices":[{"fingerprint":"0101010101010101010101010101010101010101010101010101010101010101","name":"TV","stay":true,"added_unix":1,"last_unix":1}]}"#;
        let old: Lock = serde_json::from_str(old).unwrap();
        assert_eq!(kinds(&old), [DeviceKind::Stay]);
        let mut added = Lock::default();
        assert!(added.add(&device(1), NOW));
        let text = serde_json::to_string(&added).unwrap();
        assert_eq!(serde_json::from_str::<Lock>(&text).unwrap(), added);
    }

    /// A file is read only as one this crate wrote: a bad entry or PIN spoils all of it.
    #[test]
    fn a_file_with_a_bad_entry_is_not_read() {
        let written = locks();
        let text = serde_json::to_string(&written).unwrap();
        assert_eq!(Locks::parse(text.as_bytes()), Some(written.clone()));
        let mut upper = written;
        upper.profiles.get_mut("anna").unwrap().list.devices[0].fingerprint = "AB".repeat(32);
        let upper = serde_json::to_string(&upper).unwrap();
        assert_eq!(Locks::parse(upper.as_bytes()), None);
        let fast = text.replace("600000", "10");
        assert_eq!(Locks::parse(fast.as_bytes()), None);
        assert_eq!(Locks::parse(b"{\"profiles\": 7}"), None);
        assert_eq!(Locks::parse(b"{}"), Some(Locks::default()));
    }
}
