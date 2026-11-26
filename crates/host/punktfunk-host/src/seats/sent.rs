//! Redirect records: the seat the door sent each device to, and when. Only that seat's answer
//! keeps the device's hold (`native.rs`, `hold_while_on_seat`), and the device's own management
//! calls go to that seat alone (`mgmt/owner.rs`, `device_seat`). A record ends with its hold.
//! [`FILE`] in the door's config dir keeps the records across a restart.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

/// The door's own file. `pf-seats` copies only the files its lists name into a seat, and no
/// list names this one.
const FILE: &str = "seat-redirects.json";

/// One redirect: the seat's ledger id, and the unix second the device's hold began.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Sent {
    pub seat: String,
    pub since_unix: i64,
}

/// Device fingerprint, lowercase hex → where the door sent that device.
type Records = BTreeMap<String, Sent>;

static RECORDS: Mutex<Records> = Mutex::new(BTreeMap::new());

fn records() -> MutexGuard<'static, Records> {
    RECORDS.lock().unwrap_or_else(|e| e.into_inner())
}

fn path() -> PathBuf {
    pf_paths::config_dir().join(FILE)
}

/// `path`'s records; `None` when there is no file. A file that can't be read holds none.
fn read(path: &Path) -> Option<Records> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        read => Some(
            read.ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default(),
        ),
    }
}

fn write(path: &Path, records: &Records) -> std::io::Result<()> {
    pf_paths::replace_file(path, &serde_json::to_vec(records)?)
}

/// Writes `records` to [`FILE`] on a host with seats to ask. Any other host keeps them in
/// memory alone.
fn save_records(records: &Records) {
    if !super::in_play() {
        return;
    }
    if let Err(error) = write(&path(), records) {
        tracing::warn!(%error, "seat redirect records not written");
    }
}

/// Writes the records as they stand: the startup sweep's changes, and a first file.
pub(crate) fn save() {
    save_records(&records());
}

/// Records that the door sent `fp` to the seat `seat` now.
pub(crate) fn record(fp: &str, seat: &str) -> Sent {
    let sent = Sent {
        seat: seat.to_owned(),
        since_unix: crate::clock::unix_secs(),
    };
    let mut records = records();
    records.insert(fp.to_ascii_lowercase(), sent.clone());
    save_records(&records);
    sent
}

/// Where the door sent `fp`, while that device's hold lasts.
pub(crate) fn sent_to(fp: &str) -> Option<Sent> {
    records().get(&fp.to_ascii_lowercase()).cloned()
}

/// Ends `fp`'s record while it is still `sent`. A newer redirect of the device owns a newer one.
pub(crate) fn forget(fp: &str, sent: &Sent) {
    let mut records = records();
    let key = fp.to_ascii_lowercase();
    if records.get(&key) == Some(sent) {
        records.remove(&key);
        save_records(&records);
    }
}

/// Every record, for the holds a starting door resumes.
pub(crate) fn all() -> Records {
    records().clone()
}

/// The startup sweep's question for each "this session" device: [`at_start`] on the records
/// [`FILE`] holds, each seat asked by `deadline`. A host with no seats to ask reads no file,
/// and none of its devices plays on a seat.
pub(crate) fn on_seat_at_start(deadline: Instant) -> impl Fn(&str) -> Option<bool> {
    let play = super::in_play();
    let mut fallback = false;
    if play {
        let read = read(&path());
        fallback = read.is_none();
        *records() = read.unwrap_or_default();
    }
    move |fp| {
        if !play {
            return Some(false);
        }
        let (one, every) = (fp.to_owned(), fp.to_owned());
        let plays_on = |seat: &str| {
            let seat = seat.to_owned();
            super::by(deadline, move || super::plays_on(&seat, &one))
        };
        let listing = || super::by(deadline, move || super::listing_seat(&every));
        at_start(&mut records(), fallback, fp, plays_on, listing)
    }
}

/// The startup sweep's answer for `fp` from `records`. With `fallback`, the first start after an
/// update when the door had no record file at all, every seat is asked (`listing`) and the seat
/// that lists the device becomes its record. Otherwise only the recorded seat is asked
/// (`plays_on`), a device with no record is on no seat, and one found off its seat loses its
/// record.
fn at_start(
    records: &mut Records,
    fallback: bool,
    fp: &str,
    plays_on: impl FnOnce(&str) -> Option<bool>,
    listing: impl FnOnce() -> Option<String>,
) -> Option<bool> {
    let key = fp.to_ascii_lowercase();
    if fallback {
        let Some(seat) = listing() else {
            return Some(false);
        };
        let since_unix = crate::clock::unix_secs();
        records.insert(key, Sent { seat, since_unix });
        return Some(true);
    }
    let Some(sent) = records.get(&key) else {
        return Some(false);
    };
    let here = plays_on(&sent.seat);
    if here == Some(false) {
        records.remove(&key);
    }
    here
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "aa11bb22cc33dd44ee55ff6600112233445566778899aabbccddeeff00112233";
    const SEAT: &str = "fedcba9876543210fedcba9876543210";

    /// The records read back as they were written; a missing file reads as no file at all.
    #[test]
    fn the_records_read_back_as_written() {
        let path = crate::test_support::scratch().join(FILE);
        assert_eq!(read(&path), None);
        let sent = Sent {
            seat: SEAT.into(),
            since_unix: 1_700_000_000,
        };
        let records = Records::from([(FP.to_string(), sent)]);
        write(&path, &records).unwrap();
        assert_eq!(read(&path), Some(records));
    }

    /// With no file, the first start after an update, every seat is asked and the seat that lists
    /// the device becomes its record. With a file, even an empty one, a device with no record is
    /// on no seat, and only its recorded seat is ever asked.
    #[test]
    fn only_a_start_with_no_file_asks_every_seat() {
        let path = crate::test_support::scratch().join(FILE);
        let unasked = |_: &str| -> Option<bool> { panic!("a device with no record has no seat") };

        let file = read(&path);
        let mut records = file.clone().unwrap_or_default();
        let every = || Some(SEAT.to_string());
        let answer = at_start(&mut records, file.is_none(), FP, unasked, every);
        assert_eq!(answer, Some(true), "no file: a seat lists it");
        assert_eq!(records[FP].seat, SEAT, "and that seat is its record");

        write(&path, &Records::new()).unwrap();
        let file = read(&path);
        let mut records = file.clone().unwrap_or_default();
        let every = || -> Option<String> { panic!("a file exists: no seat is asked for all") };
        let answer = at_start(&mut records, file.is_none(), FP, unasked, every);
        assert_eq!(answer, Some(false), "an empty file: on no seat");

        let mut records = Records::from([(
            FP.to_string(),
            Sent {
                seat: SEAT.into(),
                since_unix: 1,
            },
        )]);
        let left = |seat: &str| (seat == SEAT).then_some(false);
        let none = || -> Option<String> { panic!("only the recorded seat is asked") };
        assert_eq!(at_start(&mut records, false, FP, left, none), Some(false));
        assert!(records.is_empty(), "a device off its seat loses its record");
    }
}
