//! The bring-up rung a device proved, kept across sessions so the next video start begins
//! there. [`RungFloor`](crate::input_stall::RungFloor) lives for one surface; without this a
//! device whose first rung stops its decoder pays that freeze on every start.
//!
//! The file holds one line per (build fingerprint, decoder, codec): `\t`-separated, the fields
//! of [`Entry`] in order. A line that does not parse is dropped. A rung is written only when the
//! floor moved (two clean failures) and a decoder then settled on it ([`outcome`]). An entry
//! from another OS build is dropped on the next write; one [`MAX_AGE_SECS`] old or from another
//! app version gives rung 0 one more chance ([`Start::Retry`]).
//!
//! Rungs are ladder indices, and the low-latency toggle reshapes the ladder. Toggled off, the
//! session takes the platform's default decoder (keyed `default`), so it keys its own entry.
//! [`Memory`] is one surface's copy; the decode loop reads the file, the build fingerprint and
//! the clock for it, and logs and writes what it hands back.
//! Plan: `design/android-presenter-overhaul-v3.md` §3.4.

use std::path::{Path, PathBuf};

use crate::input_stall::{RungFloor, SETTLED_FRAMES};

/// How long a proven rung holds before rung 0 gets another chance: 30 days.
const MAX_AGE_SECS: u64 = 30 * 86_400;

/// The version a remembered rung was proven under.
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// One remembered rung: a line of the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) fingerprint: String,
    pub(crate) decoder: String,
    pub(crate) codec: String,
    pub(crate) rung: usize,
    /// Unix seconds when the rung was proven.
    pub(crate) proven_at_secs: u64,
    /// The app version that proved it.
    pub(crate) version: String,
}

/// What an entry is remembered for.
#[derive(Clone, Default)]
pub(crate) struct Key {
    pub(crate) fingerprint: String,
    pub(crate) decoder: String,
    pub(crate) codec: String,
}

impl Entry {
    fn is(&self, key: &Key) -> bool {
        self.fingerprint == key.fingerprint
            && self.decoder == key.decoder
            && self.codec == key.codec
    }
}

/// Where a codec's first decoder starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Start {
    /// Nothing remembered: rung 0.
    Fresh,
    /// A current entry: its rung.
    Remembered(usize),
    /// An old entry, or one from another app version: rung 0, the entry kept in hand.
    Retry(usize),
}

impl Start {
    /// The rung the floor begins at.
    pub(crate) fn rung(self) -> usize {
        match self {
            Start::Remembered(rung) => rung,
            Start::Fresh | Start::Retry(_) => 0,
        }
    }
}

/// What a decoder that showed frames does to the memory ([`outcome`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Keep,
    Record(usize),
    Forget,
}

pub(crate) fn parse(text: &str) -> Vec<Entry> {
    text.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<Entry> {
    let [fingerprint, decoder, codec, rung, proven_at_secs, version] =
        <[&str; 6]>::try_from(line.split('\t').collect::<Vec<_>>()).ok()?;
    Some(Entry {
        fingerprint: fingerprint.to_owned(),
        decoder: decoder.to_owned(),
        codec: codec.to_owned(),
        rung: rung.parse().ok()?,
        proven_at_secs: proven_at_secs.parse().ok()?,
        version: version.to_owned(),
    })
}

pub(crate) fn render(entries: &[Entry]) -> String {
    entries
        .iter()
        .map(|e| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\n",
                e.fingerprint, e.decoder, e.codec, e.rung, e.proven_at_secs, e.version
            )
        })
        .collect()
}

pub(crate) fn find<'a>(entries: &'a [Entry], key: &Key) -> Option<&'a Entry> {
    entries.iter().find(|e| e.is(key))
}

/// Where `key`'s first decoder starts. A stamp in the future (the clock went back) counts as old.
pub(crate) fn lookup(entries: &[Entry], key: &Key, now_secs: u64, version: &str) -> Start {
    let Some(e) = find(entries, key) else {
        return Start::Fresh;
    };
    let fresh = now_secs
        .checked_sub(e.proven_at_secs)
        .is_some_and(|age| age < MAX_AGE_SECS);
    if fresh && e.version == version {
        Start::Remembered(e.rung)
    } else {
        Start::Retry(e.rung)
    }
}

/// Entries from another OS build go; `true`: some did.
fn drop_other_builds(entries: &mut Vec<Entry>, key: &Key) -> bool {
    let len = entries.len();
    entries.retain(|e| e.fingerprint == key.fingerprint);
    entries.len() != len
}

/// Stamp `rung` for `key`, replacing its entry. `true`: the list changed.
pub(crate) fn record(
    entries: &mut Vec<Entry>,
    key: &Key,
    rung: usize,
    now_secs: u64,
    version: &str,
) -> bool {
    let dropped = drop_other_builds(entries, key);
    let entry = Entry {
        fingerprint: key.fingerprint.clone(),
        decoder: key.decoder.clone(),
        codec: key.codec.clone(),
        rung,
        proven_at_secs: now_secs,
        version: version.to_owned(),
    };
    match entries.iter_mut().find(|e| e.is(key)) {
        Some(e) if *e == entry => dropped,
        Some(e) => {
            *e = entry;
            true
        }
        None => {
            entries.push(entry);
            true
        }
    }
}

/// Remove `key`'s entry. `true`: the list changed.
pub(crate) fn forget(entries: &mut Vec<Entry>, key: &Key) -> bool {
    let dropped = drop_other_builds(entries, key);
    let len = entries.len();
    entries.retain(|e| !e.is(key));
    dropped || entries.len() != len
}

/// A decoder showed `rendered` frames with the floor at `floor_rung`, under a codec that
/// started as `start`. Only a settled decoder ([`SETTLED_FRAMES`]) changes anything: one that
/// settled on a floor that moved records it; a retry that settled on rung 0 clears the entry.
pub(crate) fn outcome(start: Start, floor_rung: usize, rendered: u64) -> Outcome {
    if rendered < SETTLED_FRAMES {
        Outcome::Keep
    } else if floor_rung != start.rung() {
        Outcome::Record(floor_rung)
    } else if matches!(start, Start::Retry(_)) {
        Outcome::Forget
    } else {
        Outcome::Keep
    }
}

/// One surface's copy of the memory: the file's entries, the stream codec's key, and where that
/// codec's decoders started.
pub(crate) struct Memory {
    /// Where the file is written: `None` with no files dir, or once a write failed.
    path: Option<PathBuf>,
    entries: Vec<Entry>,
    key: Key,
    start: Start,
}

impl Memory {
    /// The memory in `text`, the file at `path`: empty when the file is missing or unreadable.
    pub(crate) fn load(path: Option<PathBuf>, text: &str) -> Memory {
        Memory {
            path,
            entries: parse(text),
            key: Key::default(),
            start: Start::Fresh,
        }
    }

    /// Key the memory on the stream's codec: the floor its first decoder starts on, and the line
    /// the loop logs when an entry put it there.
    pub(crate) fn begin(&mut self, key: Key, now_secs: u64) -> (RungFloor, Option<String>) {
        self.key = key;
        self.start = lookup(&self.entries, &self.key, now_secs, APP_VERSION);
        let Key { decoder, codec, .. } = &self.key;
        let note = find(&self.entries, &self.key).and_then(|e| {
            let days = now_secs.saturating_sub(e.proven_at_secs) / 86_400;
            match self.start {
                Start::Remembered(r) => Some(format!(
                    "decode: starting at rung {r} from memory ({decoder} / {codec}, proven {days} \
                     days ago)"
                )),
                Start::Retry(_) => Some(format!(
                    "decode: retrying rung 0 for {decoder} / {codec} (memory from {}, {days} \
                     days old)",
                    e.version
                )),
                Start::Fresh => None,
            }
        });
        (RungFloor::starting_at(self.start.rung()), note)
    }

    /// A decoder has shown `rendered` frames with the floor at `floor`. `Some` when that proved
    /// something: the line the loop logs, and where to write the file's new text when it
    /// changed. The clock is read only for a rung it records.
    pub(crate) fn shown(
        &mut self,
        floor: usize,
        rendered: u64,
        now_secs: impl FnOnce() -> u64,
    ) -> Option<(String, Option<(&Path, String)>)> {
        let Key { decoder, codec, .. } = &self.key;
        let (note, changed) = match outcome(self.start, floor, rendered) {
            Outcome::Keep => return None,
            Outcome::Record(r) => {
                self.start = Start::Remembered(r);
                (
                    format!("decode: rung {r} remembered for {decoder} / {codec}"),
                    record(&mut self.entries, &self.key, r, now_secs(), APP_VERSION),
                )
            }
            Outcome::Forget => {
                self.start = Start::Fresh;
                (
                    format!("decode: rung 0 works again for {decoder} / {codec} — memory cleared"),
                    forget(&mut self.entries, &self.key),
                )
            }
        };
        let path = self.path.as_deref().filter(|_| changed);
        Some((note, path.map(|path| (path, render(&self.entries)))))
    }

    /// The last write failed: nothing more is written.
    pub(crate) fn unwritable(&mut self) {
        self.path = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = "0.45.0";
    const NOW: u64 = 1_800_000_000;
    const DAY: u64 = 86_400;

    fn key(decoder: &str) -> Key {
        Key {
            fingerprint: "jmgo/n1s/1".into(),
            decoder: decoder.into(),
            codec: "video/av01".into(),
        }
    }

    fn entry(key: &Key, rung: usize, proven_at_secs: u64, version: &str) -> Entry {
        Entry {
            fingerprint: key.fingerprint.clone(),
            decoder: key.decoder.clone(),
            codec: key.codec.clone(),
            rung,
            proven_at_secs,
            version: version.into(),
        }
    }

    const FILE: &str = "decode-rungs.txt";

    fn line(key: &Key, rung: usize, proven_at_secs: u64, version: &str) -> String {
        render(&[entry(key, rung, proven_at_secs, version)])
    }

    /// Load `file`, begin on `key` at `NOW`, then show each `(floor, rendered)` in order: the
    /// rung the first decoder starts on, and every text written.
    fn session(file: &str, key: &Key, shows: &[(usize, u64)]) -> (usize, Vec<String>) {
        let mut memory = Memory::load(Some(FILE.into()), file);
        let start = memory.begin(key.clone(), NOW).0.rung();
        let mut writes = Vec::new();
        for &(floor, rendered) in shows {
            let shown = memory.shown(floor, rendered, || NOW);
            writes.extend(shown.and_then(|(_, write)| write).map(|(_, text)| text));
        }
        (start, writes)
    }

    /// The text written when `memory` shows `floor` settled, or `None`.
    fn written(memory: &mut Memory, floor: usize) -> Option<String> {
        let shown = memory.shown(floor, SETTLED_FRAMES, || NOW);
        shown.and_then(|(_, write)| write).map(|(_, text)| text)
    }

    /// The line logged when `memory` shows `floor` settled, or `None`.
    fn note(memory: &mut Memory, floor: usize) -> Option<String> {
        memory
            .shown(floor, SETTLED_FRAMES, || NOW)
            .map(|(note, _)| note)
    }

    #[test]
    fn a_surface_starts_where_the_file_says_and_writes_what_its_decoders_prove() {
        let k = key("OMX.MS.av1");
        let other_decoder = key("default");
        let mut other_build = k.clone();
        other_build.fingerprint = "jmgo/n1s/2".into();
        let s = SETTLED_FRAMES;
        let now = |rung| line(&k, rung, NOW, APP_VERSION);
        let current = line(&k, 2, NOW - DAY, APP_VERSION);
        let old = line(&k, 2, NOW - 30 * DAY, APP_VERSION);
        let other_version = line(&k, 2, NOW - DAY, "0.0.0");
        let beside = line(&other_decoder, 3, NOW, APP_VERSION);
        let gone = line(&other_build, 3, NOW, APP_VERSION);
        // (row, session(file, key, (floor, rendered) shown in order), (start rung, texts written))
        let rows = [
            (
                "nothing remembered, rung 0 settles",
                session("", &k, &[(0, s)]),
                (0, vec![]),
            ),
            (
                "a floor that moved is written once, when it settles",
                session("", &k, &[(2, s - 1), (2, s), (2, s + 1)]),
                (0, vec![now(2)]),
            ),
            (
                "a floor that never settles writes nothing",
                session("", &k, &[(3, s - 1)]),
                (0, vec![]),
            ),
            (
                "a current entry starts at its rung",
                session(&current, &k, &[(2, s)]),
                (2, vec![]),
            ),
            (
                "a remembered floor that moves is re-stamped",
                session(&current, &k, &[(3, s)]),
                (2, vec![now(3)]),
            ),
            (
                "an old entry retries rung 0, cleared once it settles",
                session(&old, &k, &[(0, s), (0, s + 1)]),
                (0, vec![String::new()]),
            ),
            (
                "another version's entry retries rung 0",
                session(&other_version, &k, &[(2, s)]),
                (0, vec![now(2)]),
            ),
            (
                "another decoder's entry stays beside a new one",
                session(&beside, &k, &[(1, s)]),
                (0, vec![beside.clone() + &now(1)]),
            ),
            (
                "another OS build's entry goes on the next write",
                session(&gone, &k, &[(1, s)]),
                (0, vec![now(1)]),
            ),
            (
                "a file that does not parse is no memory",
                session("garbage\n", &k, &[(0, s)]),
                (0, vec![]),
            ),
        ];
        for (row, got, want) in rows {
            assert_eq!(got, want, "{row}");
        }
    }

    #[test]
    fn a_format_change_keys_the_next_codec_on_the_entries_in_hand() {
        let av1 = key("OMX.MS.av1");
        let hevc = Key {
            codec: "video/hevc".into(),
            ..av1.clone()
        };
        let mut memory = Memory::load(Some(FILE.into()), "");
        assert_eq!(memory.begin(av1.clone(), NOW).0.rung(), 0);
        // The clock is read only for a rung that is recorded.
        assert!(memory.shown(0, SETTLED_FRAMES, || unreachable!()).is_none());
        assert!(written(&mut memory, 2).is_some());
        assert_eq!(memory.begin(hevc, NOW).0.rung(), 0);
        assert_eq!(memory.begin(av1, NOW).0.rung(), 2);
    }

    #[test]
    fn nothing_is_written_with_no_files_dir_or_after_a_failed_write() {
        let k = key("OMX.MS.av1");
        let mut memory = Memory::load(None, "");
        memory.begin(k.clone(), NOW);
        assert!(note(&mut memory, 2).is_some());
        assert_eq!(written(&mut memory, 3), None);
        let mut memory = Memory::load(Some(FILE.into()), "");
        memory.begin(k, NOW);
        let shown = memory.shown(2, SETTLED_FRAMES, || NOW);
        let path = shown
            .and_then(|(_, write)| write)
            .map(|(p, _)| p.to_owned());
        assert_eq!(path, Some(FILE.into()));
        memory.unwritable();
        assert!(note(&mut memory, 3).is_some());
        assert_eq!(written(&mut memory, 4), None);
    }

    #[test]
    fn the_notes_are_the_decode_log_lines() {
        let k = key("OMX.MS.av1");
        let begin = |file: &str| Memory::load(None, file).begin(k.clone(), NOW).1;
        assert_eq!(begin(""), None);
        assert_eq!(
            begin(&line(&k, 2, NOW - DAY, APP_VERSION)).as_deref(),
            Some(
                "decode: starting at rung 2 from memory (OMX.MS.av1 / video/av01, proven 1 days \
                 ago)"
            )
        );
        assert_eq!(
            begin(&line(&k, 2, NOW - DAY, "0.0.0")).as_deref(),
            Some(
                "decode: retrying rung 0 for OMX.MS.av1 / video/av01 (memory from 0.0.0, 1 days \
                 old)"
            )
        );
        let mut memory = Memory::load(None, &line(&k, 2, NOW - 30 * DAY, APP_VERSION));
        memory.begin(k.clone(), NOW);
        assert_eq!(
            note(&mut memory, 0).as_deref(),
            Some("decode: rung 0 works again for OMX.MS.av1 / video/av01 — memory cleared")
        );
        assert_eq!(
            note(&mut memory, 3).as_deref(),
            Some("decode: rung 3 remembered for OMX.MS.av1 / video/av01")
        );
    }

    #[test]
    fn entries_round_trip_and_a_bad_line_is_dropped() {
        let entries = vec![
            entry(&key("OMX.MS.av1"), 2, NOW, V),
            entry(&key("default"), 3, NOW - DAY, "0.44.1"),
        ];
        assert_eq!(parse(&render(&entries)), entries);
        let text = format!(
            "{}a\tb\tc\tnot-a-rung\t1\tv\nshort\tline\na\tb\tc\t1\t1\tv\textra\n{}",
            render(&entries[..1]),
            render(&entries[1..]).trim_end()
        );
        assert_eq!(parse(&text), entries);
    }

    #[test]
    fn a_current_entry_is_remembered_and_an_old_one_retried() {
        let k = key("OMX.MS.av1");
        assert_eq!(lookup(&[], &k, NOW, V), Start::Fresh);
        let at = |proven, version| lookup(&[entry(&k, 2, proven, version)], &k, NOW, V);
        assert_eq!(at(NOW - 29 * DAY, V), Start::Remembered(2));
        assert_eq!(at(NOW - 30 * DAY, V), Start::Retry(2));
        assert_eq!(at(NOW - DAY, "0.44.1"), Start::Retry(2));
        assert_eq!(at(NOW + DAY, V), Start::Retry(2));
        // Another decoder, codec or OS build is another key.
        let other = [entry(&key("c2.android.av1.decoder"), 2, NOW, V)];
        assert_eq!(lookup(&other, &k, NOW, V), Start::Fresh);
        let mut build = k.clone();
        build.fingerprint = "jmgo/n1s/2".into();
        assert_eq!(
            lookup(&[entry(&k, 2, NOW, V)], &build, NOW, V),
            Start::Fresh
        );
        assert_eq!(Start::Remembered(2).rung(), 2);
        assert_eq!(Start::Retry(2).rung(), 0);
    }

    #[test]
    fn a_write_replaces_the_keys_entry_and_drops_other_builds() {
        let k = key("OMX.MS.av1");
        let mut entries = vec![entry(&key("default"), 3, NOW, V)];
        assert!(record(&mut entries, &k, 2, NOW, V));
        assert!(!record(&mut entries, &k, 2, NOW, V));
        assert!(record(&mut entries, &k, 3, NOW + 1, V));
        assert_eq!(
            entries,
            [entry(&key("default"), 3, NOW, V), entry(&k, 3, NOW + 1, V)]
        );
        let mut updated = k.clone();
        updated.fingerprint = "jmgo/n1s/2".into();
        assert!(forget(&mut entries, &updated));
        assert!(entries.is_empty());
        let mut entries = vec![entry(&k, 2, NOW, V)];
        assert!(forget(&mut entries, &k));
        assert!(!forget(&mut entries, &k));
    }

    #[test]
    fn a_floor_that_moved_and_settled_is_recorded() {
        assert_eq!(outcome(Start::Fresh, 2, SETTLED_FRAMES), Outcome::Record(2));
        assert_eq!(outcome(Start::Fresh, 0, SETTLED_FRAMES), Outcome::Keep);
        assert_eq!(
            outcome(Start::Remembered(2), 3, SETTLED_FRAMES),
            Outcome::Record(3)
        );
    }

    #[test]
    fn a_floor_that_moved_but_never_settled_records_nothing() {
        for start in [Start::Fresh, Start::Remembered(2), Start::Retry(2)] {
            assert_eq!(outcome(start, 4, SETTLED_FRAMES - 1), Outcome::Keep);
        }
        assert_eq!(
            outcome(Start::Retry(2), 0, SETTLED_FRAMES - 1),
            Outcome::Keep
        );
    }

    #[test]
    fn a_remembered_start_that_settles_on_its_rung_keeps_the_entry() {
        assert_eq!(
            outcome(Start::Remembered(2), 2, SETTLED_FRAMES),
            Outcome::Keep
        );
    }

    #[test]
    fn a_retry_that_settles_on_rung_0_clears_the_memory() {
        assert_eq!(outcome(Start::Retry(2), 0, SETTLED_FRAMES), Outcome::Forget);
    }

    #[test]
    fn a_retry_whose_floor_moves_again_is_re_stamped() {
        assert_eq!(
            outcome(Start::Retry(2), 2, SETTLED_FRAMES),
            Outcome::Record(2)
        );
        assert_eq!(
            outcome(Start::Retry(2), 3, SETTLED_FRAMES),
            Outcome::Record(3)
        );
    }
}
