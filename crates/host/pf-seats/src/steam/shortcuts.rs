//! Steam's binary `shortcuts.vdf`, and the merge that puts the box owner's non-Steam shortcuts
//! into a seat's own Steam.
//!
//! The file is one map named `shortcuts`. Its entries are keyed `0`, `1`, … and each is a map of
//! typed fields. Steam reads the file when it starts and writes it back from memory, so a merge
//! lasts only when it is written while that Steam is closed.
//!
//! An entry's stored `appid` is its identity. Steam lists the entry, launches it and keys its
//! art by that `appid`, and keeps it when it rewrites the file. A merge keeps every field byte
//! for byte, including the fields this code never reads.

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

/// The source ids a box's `library-scanners.json` switches off: its `disabled` list.
pub fn off_sources(scanners: &[u8]) -> BTreeSet<String> {
    let Ok(json) = serde_json::from_slice::<Json>(scanners) else {
        return BTreeSet::new();
    };
    let ids = json.get("disabled").and_then(Json::as_array);
    ids.into_iter()
        .flatten()
        .filter_map(|id| id.as_str().map(str::to_owned))
        .collect()
}

/// The grid art Steam shows for shortcut `id`, by file name in an account's `config/grid`.
pub fn art_names(id: u32) -> impl Iterator<Item = String> {
    ["p", "", "_hero", "_logo"]
        .into_iter()
        .flat_map(move |suffix| ["png", "jpg"].map(|ext| format!("{id}{suffix}.{ext}")))
}

/// String field `name` without the quotes Steam keeps around a path.
pub fn text(fields: &Fields, name: &str) -> Option<String> {
    match field(fields, name)? {
        Value::Str(s) => Some(
            String::from_utf8_lossy(s)
                .trim()
                .trim_matches('"')
                .to_owned(),
        ),
        _ => None,
    }
}

/// The absolute folder a shortcut starts in: its `StartDir`, else its program's folder, with no
/// trailing `/`.
pub fn folder(fields: &Fields) -> Option<String> {
    let dir = match text(fields, "StartDir").filter(|dir| !dir.is_empty()) {
        Some(dir) => dir,
        None => text(fields, "Exe")?.rsplit_once('/')?.0.to_owned(),
    };
    let dir = dir.trim_end_matches('/');
    (dir.starts_with('/')).then(|| dir.to_owned())
}

/// `fields` with its program, start folder and every path in its launch options moved from
/// folder `from` to `to`. `None` when its program isn't inside `from`.
pub fn moved(fields: &Fields, from: &str, to: &str) -> Option<Fields> {
    let to = to.trim_end_matches('/');
    let exe = text(fields, "Exe")?;
    let rest = exe.strip_prefix(from)?.strip_prefix('/')?;
    let mut out = fields.clone();
    set(
        &mut out,
        "Exe",
        Value::Str(format!("\"{to}/{rest}\"").into_bytes()),
    );
    set(
        &mut out,
        "StartDir",
        Value::Str(format!("\"{to}/\"").into_bytes()),
    );
    if let Some(Value::Str(options)) = field(fields, "LaunchOptions") {
        let options = repoint(&String::from_utf8_lossy(options), from, to);
        set(&mut out, "LaunchOptions", Value::Str(options.into_bytes()));
    }
    Some(out)
}

/// `text` with each whole path `from`, or one inside it, starting at `to` instead.
fn repoint(text: &str, from: &str, to: &str) -> String {
    let path_char = |c: char| c == '/' || c == '.' || c == '_' || c == '-' || c.is_alphanumeric();
    let ends = |c: char| c == '/' || c == '"' || c == '\'' || c.is_whitespace();
    let mut out = String::new();
    let mut last = 0;
    for (at, _) in text.match_indices(from) {
        let before = text[..at].chars().last();
        let after = text[at + from.len()..].chars().next();
        if before.is_some_and(path_char) || after.is_some_and(|c| !ends(c)) {
            continue;
        }
        out.push_str(&text[last..at]);
        out.push_str(to);
        last = at + from.len();
    }
    out.push_str(&text[last..]);
    out
}

/// A shortcut's row in a Steam `config/config.vdf` `CompatToolMapping`: the Proton it runs with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompatRow {
    pub appid: u32,
    /// The row's block, braces included, as Steam wrote it.
    pub block: String,
    /// The tool's internal name, as a `compatibilitytool.vdf` declares it.
    pub tool: String,
}

const STEAM_SETTINGS: [&str; 4] = ["InstallConfigStore", "Software", "Valve", "Steam"];
const COMPAT: &str = "CompatToolMapping";

/// The rows `ids` have in `config`, a Steam `config/config.vdf`.
pub fn compat_rows(config: &str, ids: &BTreeSet<u32>) -> Vec<CompatRow> {
    let t = super::tokens(config);
    let Some((_, open, _)) = block_at(&t, &STEAM_SETTINGS).and_then(|s| child(&t, s.1, COMPAT))
    else {
        return Vec::new();
    };
    children(&t, open)
        .into_iter()
        .filter_map(|(key, open, close)| {
            let appid = text_of(&t[key])?
                .parse()
                .ok()
                .filter(|id| ids.contains(id))?;
            let tool = value_of(&t[open + 1..close], "name")?;
            let block = config[t[open].start..t[close].end].to_owned();
            Some(CompatRow { appid, block, tool })
        })
        .collect()
}

/// `config`, a seat Steam's `config/config.vdf`, with each of `rows` in its `CompatToolMapping`
/// in place of the seat's row for that appid. `None` when it has no Steam settings block.
pub fn with_compat_rows(config: &str, rows: &[CompatRow]) -> Option<String> {
    let t = super::tokens(config);
    let steam = block_at(&t, &STEAM_SETTINGS)?;
    let mut edits = Vec::new();
    match child(&t, steam.1, COMPAT) {
        Some((_, open, close)) => {
            let mut added = String::new();
            for row in rows {
                match child(&t, open, &row.appid.to_string()) {
                    Some((_, at, end)) => edits.push((t[at].start..t[end].end, row.block.clone())),
                    None => added.push_str(&format!("\"{}\"\n{}\n", row.appid, row.block)),
                }
            }
            edits.push((t[close].start..t[close].start, added));
        }
        None if rows.is_empty() => {}
        None => {
            let rows: String = rows
                .iter()
                .map(|row| format!("\"{}\"\n{}\n", row.appid, row.block))
                .collect();
            let block = format!("\"{COMPAT}\"\n{{\n{rows}}}\n");
            edits.push((t[steam.2].start..t[steam.2].start, block));
        }
    }
    let mut out = config.to_owned();
    edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
    for (range, text) in edits {
        out.replace_range(range, &text);
    }
    Some(out)
}

/// The tool names a `compatibilitytool.vdf` declares.
pub fn tool_names(vdf: &str) -> Vec<String> {
    let t = super::tokens(vdf);
    let Some((_, open, _)) = block_at(&t, &["compatibilitytools", "compat_tools"]) else {
        return Vec::new();
    };
    children(&t, open)
        .into_iter()
        .filter_map(|(key, _, _)| text_of(&t[key]).map(str::to_owned))
        .collect()
}

fn text_of(token: &super::Token) -> Option<&str> {
    match &token.kind {
        super::Kind::Text(text) => Some(text),
        _ => None,
    }
}

/// The blocks directly inside the one opening at `open`: each key's, open and close index.
fn children(t: &[super::Token], open: usize) -> Vec<(usize, usize, usize)> {
    use super::Kind;
    let mut out = Vec::new();
    let Some(close) = super::closing(t, open) else {
        return out;
    };
    let mut i = open + 1;
    while i < close {
        match (t.get(i).map(|k| &k.kind), t.get(i + 1).map(|v| &v.kind)) {
            (Some(Kind::Text(_)), Some(Kind::Open)) => {
                let Some(end) = super::closing(t, i + 1) else {
                    return out;
                };
                out.push((i, i + 1, end));
                i = end + 1;
            }
            (Some(Kind::Text(_)), Some(Kind::Text(_))) => i += 2,
            _ => return out,
        }
    }
    out
}

/// Block `name` directly inside the one opening at `open`, without case.
fn child(t: &[super::Token], open: usize, name: &str) -> Option<(usize, usize, usize)> {
    children(t, open)
        .into_iter()
        .find(|&(key, _, _)| text_of(&t[key]).is_some_and(|k| k.eq_ignore_ascii_case(name)))
}

/// The block at `names` from the file's top, without case.
fn block_at(t: &[super::Token], names: &[&str]) -> Option<(usize, usize, usize)> {
    let (first, rest) = names.split_first()?;
    text_of(t.first()?).filter(|k| k.eq_ignore_ascii_case(first))?;
    if t.get(1)?.kind != super::Kind::Open {
        return None;
    }
    let mut at = (0, 1, super::closing(t, 1)?);
    for name in rest {
        at = child(t, at.1, name)?;
    }
    Some(at)
}

/// Value `key` directly in `inner`, a block's tokens without its braces.
fn value_of(inner: &[super::Token], key: &str) -> Option<String> {
    let mut depth = 0_usize;
    let mut i = 0;
    while let Some(token) = inner.get(i) {
        match &token.kind {
            super::Kind::Open => depth += 1,
            super::Kind::Close => depth = depth.saturating_sub(1),
            super::Kind::Text(k) if depth == 0 => {
                if let Some(super::Kind::Text(v)) = inner.get(i + 1).map(|t| &t.kind) {
                    if k.eq_ignore_ascii_case(key) {
                        return Some(v.clone());
                    }
                    i += 2;
                    continue;
                }
            }
            super::Kind::Text(_) => {}
        }
        i += 1;
    }
    None
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

    fn linux_entry(exe: &str, start: &str, options: &str) -> Fields {
        let s = |v: &str| Value::Str(v.as_bytes().to_vec());
        let mut fields = entry(7, "game", 0);
        fields.push((b"Exe".to_vec(), s(exe)));
        fields.push((b"StartDir".to_vec(), s(start)));
        fields.push((b"LaunchOptions".to_vec(), s(options)));
        fields
    }

    /// The folder is the start folder, else the program's; a relative one is none. A move
    /// repoints the program, the folder and each whole path in the options, nothing else.
    #[test]
    fn a_shortcut_moves_with_its_folder() {
        let game = linux_entry(
            "\"/home/o/Games/A/bin/run.sh\"",
            "\"/home/o/Games/A/\"",
            "--data \"/home/o/Games/A/data\" --other /home/o/Games/AB /x/home/o/Games/A",
        );
        assert_eq!(folder(&game).as_deref(), Some("/home/o/Games/A"));
        let no_start = linux_entry("\"/srv/g/run\"", "", "");
        assert_eq!(folder(&no_start).as_deref(), Some("/srv/g"));
        assert_eq!(folder(&linux_entry("run.exe", "./", "")), None);

        let to = "/run/media/punktfunk/shortcuts/00aa";
        let moved = moved(&game, "/home/o/Games/A", to).unwrap();
        assert_eq!(text(&moved, "Exe").unwrap(), format!("{to}/bin/run.sh"));
        assert_eq!(text(&moved, "StartDir").unwrap(), format!("{to}/"));
        assert_eq!(
            text(&moved, "LaunchOptions").unwrap(),
            format!("--data \"{to}/data\" --other /home/o/Games/AB /x/home/o/Games/A")
        );
        assert_eq!(appid(&moved), Some(7));
        let slashed = super::moved(&game, "/home/o/Games/A", &format!("{to}/")).unwrap();
        assert_eq!(
            slashed, moved,
            "a view with a trailing slash moves the same"
        );
        let flatpak = linux_entry("\"/usr/bin/flatpak\"", "\"/home/o/Games/A/\"", "");
        assert!(super::moved(&flatpak, "/home/o/Games/A", to).is_none());
    }

    const CONFIG: &str = r#""InstallConfigStore"
{
	"Software"
	{
		"Valve"
		{
			"Steam"
			{
				"CompatToolMapping"
				{
					"0"
					{
						"name"		"proton_9"
						"config"		""
						"priority"		"75"
					}
					"7"
					{
						"name"		"GE-Proton9-20"
						"config"		""
						"priority"		"250"
					}
				}
			}
		}
	}
}
"#;

    /// The owner's row for a shared appid goes into the seat's file: over the seat's row, beside
    /// the others, or in a mapping block of its own; a second merge changes nothing.
    #[test]
    fn a_shortcut_s_proton_follows_it_into_the_seat() {
        let rows = compat_rows(CONFIG, &BTreeSet::from([7, 8]));
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].appid, rows[0].tool.as_str()), (7, "GE-Proton9-20"));

        let seat_had = CONFIG.replace("GE-Proton9-20", "proton_8");
        let merged = with_compat_rows(&seat_had, &rows).unwrap();
        assert_eq!(merged, CONFIG);
        assert_eq!(with_compat_rows(&merged, &rows).unwrap(), merged);

        let no_row = CONFIG.replace("\"7\"", "\"9\"");
        let added = with_compat_rows(&no_row, &rows).unwrap();
        let read = compat_rows(&added, &BTreeSet::from([0, 7, 9]));
        assert_eq!(read.iter().map(|r| r.appid).collect::<Vec<_>>(), [0, 9, 7]);

        let bare = "\"InstallConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"x\" \"1\"\n\t\t\t}\n\t\t}\n\t}\n}\n";
        let made = with_compat_rows(bare, &rows).unwrap();
        assert_eq!(compat_rows(&made, &BTreeSet::from([7])), rows);
        assert!(with_compat_rows("\"Other\" {}", &rows).is_none());
    }

    #[test]
    fn a_tool_s_names_come_from_its_compatibilitytool_vdf() {
        let vdf = r#""compatibilitytools"
{
  "compat_tools"
  {
    "GE-Proton9-20" // Internal name of this tool
    {
      "install_path" "."
      "display_name" "GE-Proton9-20"
      "from_oslist"  "windows"
      "to_oslist"    "linux"
    }
  }
}"#;
        assert_eq!(tool_names(vdf), ["GE-Proton9-20"]);
        assert!(tool_names("junk").is_empty());
    }

    #[test]
    fn the_box_s_switched_off_sources_are_read() {
        let off = off_sources(br#"{"disabled":["steam","heroic",3]}"#);
        assert_eq!(off, BTreeSet::from(["steam".into(), "heroic".into()]));
        assert!(off_sources(b"{}").is_empty());
        assert!(off_sources(b"junk").is_empty());
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
