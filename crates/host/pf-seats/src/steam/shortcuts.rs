//! Steam's binary `shortcuts.vdf`, and the merge that puts the box owner's non-Steam shortcuts
//! into a seat's own Steam.
//!
//! The file is one `shortcuts` map whose entries, keyed `0`, `1`, …, are maps of typed fields.
//! Steam reads it at start and writes it back from memory, so a merge lands only while that Steam
//! is closed. An entry's stored `appid` is its identity: Steam lists, launches and keys art by it,
//! and keeps it when it rewrites the file. Every field survives a merge byte for byte, those this
//! code never reads too.

use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

/// One field's value: a nested map, a string, or a fixed-width number as its raw bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Map(Fields),
    Str(Vec<u8>),
    /// Type tag and little-endian bytes: `2` int32, `3` float32, `7` uint64, `10` int64.
    Fixed(u8, Vec<u8>),
}

/// A map's fields in file order.
pub type Fields = Vec<(Vec<u8>, Value)>;

const MAP: u8 = 0;
const STR: u8 = 1;
const END: u8 = 8;

/// Steam nests an entry's `tags` one map below the entry; anything deeper is refused unread.
const MAX_DEPTH: usize = 8;

/// The entries of a `shortcuts.vdf`, or `None` for a file this code can't write back whole.
pub fn parse(bytes: &[u8]) -> Option<Vec<Fields>> {
    let mut at = 0;
    let root = map(bytes, &mut at, 0)?;
    if at != bytes.len() {
        return None;
    }
    match root.as_slice() {
        [(key, Value::Map(entries))] if key.eq_ignore_ascii_case(b"shortcuts") => entries
            .iter()
            .map(|(_, entry)| match entry {
                Value::Map(fields) => Some(fields.clone()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

fn map(b: &[u8], at: &mut usize, depth: usize) -> Option<Fields> {
    if depth > MAX_DEPTH {
        return None;
    }
    let mut out = Vec::new();
    loop {
        let tag = *b.get(*at)?;
        *at += 1;
        if tag == END {
            return Some(out);
        }
        let key = cstr(b, at)?;
        let value = match tag {
            MAP => Value::Map(map(b, at, depth + 1)?),
            STR => Value::Str(cstr(b, at)?),
            2 | 3 | 7 | 10 => {
                let n = if tag == 2 || tag == 3 { 4 } else { 8 };
                let raw = b.get(*at..*at + n)?.to_vec();
                *at += n;
                Value::Fixed(tag, raw)
            }
            _ => return None,
        };
        out.push((key, value));
    }
}

fn cstr(b: &[u8], at: &mut usize) -> Option<Vec<u8>> {
    let len = b.get(*at..)?.iter().position(|&c| c == 0)?;
    let s = b[*at..*at + len].to_vec();
    *at += len + 1;
    Some(s)
}

/// `entries` as a `shortcuts.vdf`, keyed `0`, `1`, … in order.
pub fn write(entries: &[Fields]) -> Vec<u8> {
    let keyed = entries
        .iter()
        .enumerate()
        .map(|(i, fields)| (i.to_string().into_bytes(), Value::Map(fields.clone())))
        .collect();
    let mut out = Vec::new();
    put(&mut out, &vec![(b"shortcuts".to_vec(), Value::Map(keyed))]);
    out
}

fn put(out: &mut Vec<u8>, fields: &Fields) {
    for (key, value) in fields {
        out.push(match value {
            Value::Map(_) => MAP,
            Value::Str(_) => STR,
            Value::Fixed(tag, _) => *tag,
        });
        out.extend_from_slice(key);
        out.push(0);
        match value {
            Value::Map(inner) => put(out, inner),
            Value::Str(s) => {
                out.extend_from_slice(s);
                out.push(0);
            }
            Value::Fixed(_, raw) => out.extend_from_slice(raw),
        }
    }
    out.push(END);
}

/// Field `name` of `fields`, without case: older Steam wrote `appname` where newer writes `AppName`.
pub fn field<'a>(fields: &'a Fields, name: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name.as_bytes()))
        .map(|(_, value)| value)
}

/// Sets field `name`, in place when `fields` has it.
pub fn set(fields: &mut Fields, name: &str, value: Value) {
    match fields
        .iter_mut()
        .find(|(key, _)| key.eq_ignore_ascii_case(name.as_bytes()))
    {
        Some((_, old)) => *old = value,
        None => fields.push((name.as_bytes().to_vec(), value)),
    }
}

/// The entry's stored appid.
pub fn appid(fields: &Fields) -> Option<u32> {
    match field(fields, "appid")? {
        Value::Fixed(2, raw) => Some(u32::from_le_bytes(raw.as_slice().try_into().ok()?)),
        _ => None,
    }
}

/// The id `steam://rungameid/` takes for the shortcut with `appid`.
pub fn game_id(appid: u32) -> u64 {
    (u64::from(appid) << 32) | 0x0200_0000
}

/// The shortcut appid a `rungameid` value names; `None` for a store app.
pub fn appid_of_game_id(id: u64) -> Option<u32> {
    (id & 0xFFFF_FFFF == 0x0200_0000).then_some((id >> 32) as u32)
}

/// A seat's `shortcuts.vdf` after a merge, and the appids the box put there.
#[derive(Debug)]
pub struct Merged {
    pub file: Vec<u8>,
    pub ours: BTreeSet<u32>,
}

/// The seat's file `seat` with `owner`'s entries in it. `ours` names the appids the box put there
/// before: those entries are the box's to replace or drop. Every other entry is the person's and
/// stays as it is, and an owner entry with a person's appid stays out. An entry the box put there
/// keeps the seat's `LastPlayTime`. `None` for a seat file this code can't write back whole.
pub fn merge(seat: Option<&[u8]>, owner: &[Fields], ours: &BTreeSet<u32>) -> Option<Merged> {
    let had = match seat {
        Some(bytes) => parse(bytes)?,
        None => Vec::new(),
    };
    let (box_before, mut entries): (Vec<_>, Vec<_>) = had
        .into_iter()
        .partition(|f| appid(f).is_some_and(|id| ours.contains(&id)));
    let person: BTreeSet<u32> = entries.iter().filter_map(appid).collect();
    let mut now = BTreeSet::new();
    for entry in owner {
        let Some(id) = appid(entry) else { continue };
        if person.contains(&id) || !now.insert(id) {
            continue;
        }
        let mut entry = entry.clone();
        let played = box_before
            .iter()
            .find(|f| appid(f) == Some(id))
            .and_then(|f| field(f, "LastPlayTime"));
        if let Some(played) = played {
            set(&mut entry, "LastPlayTime", played.clone());
        }
        entries.push(entry);
    }
    Some(Merged {
        file: write(&entries),
        ours: now,
    })
}

/// The shortcut appids the box's `library.json` lists, from a source not in `off` (its
/// `library-scanners.json`). Only these reach a seat, so the Steam plugin's own switches and the
/// box's source switch hold there too.
pub fn listed(catalog: &[u8], off: &BTreeSet<String>) -> BTreeSet<u32> {
    let Ok(json) = serde_json::from_slice::<Json>(catalog) else {
        return BTreeSet::new();
    };
    let entries = json.get("entries").and_then(Json::as_array);
    entries
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let source = e.get("store").or_else(|| e.get("provider"));
            if source
                .and_then(Json::as_str)
                .is_some_and(|s| off.contains(s))
            {
                return None;
            }
            let launch = e.get("launch")?;
            (launch.get("kind")?.as_str()? == "steam_appid").then_some(())?;
            appid_of_game_id(launch.get("value")?.as_str()?.parse().ok()?)
        })
        .collect()
}

/// The file in the box's library folder that names, per seat id, the shortcut appids the
/// supervisor put into that seat's Steam. A seat lists a shortcut only when its row names it.
/// The `library` prefix puts it in the key of the host's cached library lists.
pub const SEAT_FILE: &str = "library-seat-shortcuts.json";

/// Seat `seat`'s appids in a [`SEAT_FILE`]: none when the file or the seat's row doesn't read.
pub fn shared_with(file: &[u8], seat: &str) -> BTreeSet<u32> {
    serde_json::from_slice::<BTreeMap<String, BTreeSet<u32>>>(file)
        .ok()
        .and_then(|mut rows| rows.remove(seat))
        .unwrap_or_default()
}

/// `file`, a [`SEAT_FILE`], with seat `seat`'s row set to `ours`. One that doesn't read starts
/// afresh.
pub fn with_seat(file: Option<&[u8]>, seat: &str, ours: &BTreeSet<u32>) -> Vec<u8> {
    let mut rows: BTreeMap<String, BTreeSet<u32>> = file
        .and_then(|f| serde_json::from_slice(f).ok())
        .unwrap_or_default();
    rows.insert(seat.to_owned(), ours.clone());
    serde_json::to_vec(&rows).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Written by Steam itself on a Windows seat after it launched an entry placed there: one
    /// shortcut, its `LastPlayTime` set and `sortas` added.
    const STEAM_WROTE: &[u8] = include_bytes!("../../tests/fixtures/steam-shortcuts.vdf");

    fn entry(id: u32, name: &str, played: i32) -> Fields {
        vec![
            (
                b"appid".to_vec(),
                Value::Fixed(2, id.to_le_bytes().to_vec()),
            ),
            (b"AppName".to_vec(), Value::Str(name.as_bytes().to_vec())),
            (
                b"LastPlayTime".to_vec(),
                Value::Fixed(2, played.to_le_bytes().to_vec()),
            ),
            (b"tags".to_vec(), Value::Map(Vec::new())),
        ]
    }

    fn names(file: &[u8]) -> Vec<String> {
        parse(file)
            .unwrap()
            .iter()
            .map(|f| match field(f, "appname") {
                Some(Value::Str(s)) => String::from_utf8_lossy(s).into_owned(),
                _ => String::new(),
            })
            .collect()
    }

    fn played(file: &[u8], id: u32) -> Option<Value> {
        let entries = parse(file).unwrap();
        let entry = entries.iter().find(|f| appid(f) == Some(id))?;
        field(entry, "LastPlayTime").cloned()
    }

    #[test]
    fn a_file_steam_wrote_comes_back_byte_for_byte() {
        let entries = parse(STEAM_WROTE).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(appid(&entries[0]), Some(2_803_639_124));
        assert!(field(&entries[0], "sortas").is_some());
        assert_eq!(write(&entries), STEAM_WROTE);
    }

    #[test]
    fn a_file_this_code_cant_write_back_whole_is_refused() {
        assert!(parse(&STEAM_WROTE[..STEAM_WROTE.len() - 1]).is_none());
        assert!(parse(&[STEAM_WROTE, b"x"].concat()).is_none());
        let mut unknown = STEAM_WROTE.to_vec();
        unknown[14] = 9; // the first entry's `appid` tag
        assert!(parse(&unknown).is_none());
        let deep: Vec<u8> = std::iter::repeat_n([MAP, b'k', 0], 64).flatten().collect();
        assert!(parse(&deep).is_none());
        assert!(merge(Some(b"junk"), &[entry(1, "a", 0)], &BTreeSet::new()).is_none());
    }

    #[test]
    fn the_person_s_shortcuts_stay_and_the_box_s_follow_the_owner() {
        let seat = write(&[
            entry(10, "theirs", 5),
            entry(20, "box, still the owner's", 7),
            entry(30, "box, owner removed it", 9),
            entry(40, "theirs, same id as the owner's", 11),
        ]);
        let owner = [
            entry(20, "owner renamed it", 1),
            entry(40, "owner's", 1),
            entry(50, "owner's new one", 1),
        ];
        let ours = BTreeSet::from([20, 30]);
        let merged = merge(Some(&seat), &owner, &ours).unwrap();
        assert_eq!(
            names(&merged.file),
            [
                "theirs",
                "theirs, same id as the owner's",
                "owner renamed it",
                "owner's new one"
            ]
        );
        assert_eq!(merged.ours, BTreeSet::from([20, 50]));
        assert_eq!(
            played(&merged.file, 20),
            Some(Value::Fixed(2, 7i32.to_le_bytes().to_vec()))
        );
        assert_eq!(
            played(&merged.file, 50),
            Some(Value::Fixed(2, 1i32.to_le_bytes().to_vec()))
        );
    }

    #[test]
    fn a_seat_with_no_file_gets_the_owner_s_and_a_merge_again_changes_nothing() {
        let owner = [
            entry(20, "a", 0),
            entry(50, "b", 0),
            entry(20, "a twice", 0),
        ];
        let first = merge(None, &owner, &BTreeSet::new()).unwrap();
        assert_eq!(names(&first.file), ["a", "b"]);
        let again = merge(Some(&first.file), &owner, &first.ours).unwrap();
        assert_eq!(again.file, first.file);
        assert_eq!(again.ours, first.ours);
        let gone = merge(Some(&first.file), &[], &first.ours).unwrap();
        assert!(parse(&gone.file).unwrap().is_empty());
        assert!(gone.ours.is_empty());
    }

    #[test]
    fn a_shortcut_s_game_id_and_appid_round_trip() {
        assert_eq!(game_id(2_803_639_124), 12_041_538_347_399_643_136);
        assert_eq!(
            appid_of_game_id(12_041_538_347_399_643_136),
            Some(2_803_639_124)
        );
        assert_eq!(appid_of_game_id(570), None);
    }

    #[test]
    fn a_seat_reads_back_only_its_own_row() {
        let one = with_seat(None, "aa", &BTreeSet::from([1, 2]));
        let both = with_seat(Some(&one), "bb", &BTreeSet::from([3]));
        assert_eq!(shared_with(&both, "aa"), BTreeSet::from([1, 2]));
        assert_eq!(shared_with(&both, "bb"), BTreeSet::from([3]));
        assert!(shared_with(&both, "cc").is_empty());
        assert!(shared_with(b"junk", "aa").is_empty());
        let afresh = with_seat(Some(b"junk"), "aa", &BTreeSet::from([4]));
        assert_eq!(shared_with(&afresh, "aa"), BTreeSet::from([4]));
    }

    #[test]
    fn only_listed_shortcuts_from_a_source_left_on_reach_a_seat() {
        let catalog = br#"{"entries":[
            {"id":"a","title":"Shortcut","provider":"steam","store":"steam",
             "launch":{"kind":"steam_appid","value":"12041538347399643136"}},
            {"id":"b","title":"Store game","provider":"steam","store":"steam",
             "launch":{"kind":"steam_appid","value":"570"}},
            {"id":"c","title":"Other kind","launch":{"kind":"command","value":"12041538347399643136"}},
            {"id":"d","title":"No launch"}
        ]}"#;
        assert_eq!(
            listed(catalog, &BTreeSet::new()),
            BTreeSet::from([2_803_639_124])
        );
        assert!(listed(catalog, &BTreeSet::from(["steam".to_owned()])).is_empty());
        assert!(listed(b"not json", &BTreeSet::new()).is_empty());
    }
}
