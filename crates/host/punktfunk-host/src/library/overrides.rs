//! Game overrides: the stream settings this host sets for one game, or for every title of a
//! source ([`GameOverrides`]). A side table like `hidden.rs`, keyed by the stable
//! `<store>:<external_id>` ids, so a rescan keeps it.
//!
//! An entry names a shared override (`set`) or carries values of its own. A game's entry wins
//! over its source's. A shared override that is gone, or values that fail validation, override
//! nothing. [`GameEntry::overrides`] carries what each title resolves to; the handshake holds
//! a client to the downward half. Pin `library-overrides.json` in the tests below.

use super::hidden::store_of;
use super::*;
use punktfunk_core::game_overrides::GameOverrides;
use std::collections::BTreeSet;
use std::sync::Mutex;

const VERSION: u32 = 1;

// The API document's view of the core's `GameOverrides`, which has no schema of its own.
// `the_schema_names_every_field_the_core_sends` holds the two together.

/// Stream settings this host sets for a game. A field left out is the device's to decide.
#[derive(ToSchema)]
#[schema(as = GameOverrides)]
#[allow(dead_code, reason = "a schema only: the core's type is what is sent")]
pub struct GameOverridesSchema {
    /// The shared override these values came from. A game's or source's own values have none.
    #[schema(required = false, example = "1080p60 DualSense")]
    name: Option<String>,
    /// Height in lines: 2160 for 4K. The width follows the device's aspect.
    #[schema(value_type = u32, required = false, minimum = 360, maximum = 4320, example = 1080)]
    lines: Option<u32>,
    /// Refresh rate in Hz, never above the device's screen.
    #[schema(value_type = u32, required = false, minimum = 24, maximum = 240, example = 60)]
    hz: Option<u32>,
    #[schema(required = false)]
    hdr: Option<bool>,
    /// A controller by the Hello's name for it: `xbox360`, `dualsense`, `dualshock4`, `auto`, …
    /// The host stores the name itself, so `ds` reads back as `dualsense`.
    #[schema(required = false, example = "dualsense")]
    gamepad: Option<String>,
    /// `lines` also reaches a device whose screen is smaller, scaled down there.
    #[schema(required = false)]
    beyond_screen: bool,
}

/// A game's or a source's entry: `set` names a shared override, or the fields are its own.
/// Both at once is refused, and `{}` is no entry. `name` is the shared override's alone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OverrideEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "a1b2c3d4e5f6")]
    pub set: Option<String>,
    #[serde(flatten)]
    #[schema(value_type = GameOverridesSchema)]
    pub values: GameOverrides,
}

impl OverrideEntry {
    fn is_empty(&self) -> bool {
        self.set.is_none() && self.values.is_empty()
    }
}

/// `library-overrides.json`, and the body of `GET /library/overrides`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OverridesDoc {
    #[serde(default)]
    #[schema(example = 1)]
    pub version: u32,
    /// Shared overrides by id, 12 lowercase hex.
    #[serde(default)]
    #[schema(value_type = BTreeMap<String, GameOverridesSchema>)]
    pub sets: BTreeMap<String, GameOverrides>,
    /// Library id (`steam:570`) → its entry.
    #[serde(default)]
    pub games: BTreeMap<String, OverrideEntry>,
    /// Source (`lutris`) → the entry each of its titles takes when it has none of its own.
    #[serde(default)]
    pub sources: BTreeMap<String, OverrideEntry>,
}

impl OverridesDoc {
    /// `games[id] ?? sources[store]`, then the shared override it names, in the stored shape.
    /// A pad name the host does not know drops only the pad. Absent when nothing is set or
    /// another value fails validation.
    pub(crate) fn resolve(&self, id: &str) -> Option<GameOverrides> {
        let entry = self
            .games
            .get(id)
            .or_else(|| self.sources.get(store_of(id)))?;
        let mut o = match &entry.set {
            Some(set) => self.sets.get(set)?.clone(),
            None => GameOverrides {
                name: None,
                ..entry.values.clone()
            },
        };
        // Android knows a pad only by its own name, and a hand-edited one may be any alias.
        o.gamepad = o.gamepad_pref().map(|p| p.as_str().to_string());
        checked(o).ok().filter(|o| !o.is_empty())
    }

    /// The sources whose titles see shared override `set`: each user game's store, and each
    /// user source.
    fn reach(&self, set: &str) -> BTreeSet<String> {
        let uses = |e: &OverrideEntry| e.set.as_deref() == Some(set);
        let games = self.games.iter().filter(|(_, e)| uses(e));
        let sources = self.sources.iter().filter(|(_, e)| uses(e));
        games
            .map(|(id, _)| store_of(id).to_string())
            .chain(sources.map(|(s, _)| s.clone()))
            .collect()
    }
}

/// A shared override with its id: what create and update answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct OverrideSet {
    #[schema(example = "a1b2c3d4e5f6")]
    pub id: String,
    #[serde(flatten)]
    #[schema(value_type = GameOverridesSchema)]
    pub values: GameOverrides,
}

/// How many entries named a deleted shared override. They are removed: those games and sources
/// go back to what the device decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
pub struct OverrideUsers {
    pub games: usize,
    pub sources: usize,
}

/// `PUT /library/overrides/sets/{id}`: a field left out keeps its value, `null` clears it.
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct OverrideSetPatch {
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub name: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<u32>)]
    pub lines: Option<Option<u32>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<u32>)]
    pub hz: Option<Option<u32>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<bool>)]
    pub hdr: Option<Option<bool>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>)]
    pub gamepad: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<bool>)]
    pub beyond_screen: Option<Option<bool>>,
}

/// A key that is there, `null` included; `default` covers the absent one.
fn present<'de, D, T>(d: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

impl OverrideSetPatch {
    fn apply(self, o: &mut GameOverrides) {
        if let Some(v) = self.name {
            o.name = v;
        }
        if let Some(v) = self.lines {
            o.lines = v;
        }
        if let Some(v) = self.hz {
            o.hz = v;
        }
        if let Some(v) = self.hdr {
            o.hdr = v;
        }
        if let Some(v) = self.gamepad {
            o.gamepad = v;
        }
        if let Some(v) = self.beyond_screen {
            o.beyond_screen = v.unwrap_or(false);
        }
    }
}

/// Why a write was refused.
#[derive(Debug)]
pub enum OverrideError {
    /// The field out of range or unknown; `set` also when values ride beside it.
    Invalid(&'static str),
    /// Another shared override has this name, ignoring case.
    NameTaken,
    NoSuchSet,
    Save(anyhow::Error),
}

fn settings_path() -> PathBuf {
    pf_paths::seat::library_dir().join("library-overrides.json")
}

/// Malformed or absent file → no overrides. A bad parse must cost the overrides, not a stream.
pub fn load_overrides() -> OverridesDoc {
    let mut doc: OverridesDoc = read_json_or_default(&settings_path());
    doc.version = VERSION;
    doc
}

/// What the launch of library id `id` is held to.
pub fn overrides_for(id: &str) -> Option<GameOverrides> {
    load_overrides().resolve(id)
}

/// Load, change and save under one lock, then one `library.changed` per source `f` names. A
/// bulk assign is many writes at once and must not lose one. No change, no save.
fn mutate<T>(
    f: impl FnOnce(&mut OverridesDoc) -> std::result::Result<(T, BTreeSet<String>), OverrideError>,
) -> std::result::Result<T, OverrideError> {
    static LOCK: Mutex<()> = Mutex::new(());
    let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut doc = load_overrides();
    let before = doc.clone();
    let (out, sources) = f(&mut doc)?;
    if doc == before {
        return Ok(out);
    }
    let json = serde_json::to_string_pretty(&doc).map_err(|e| OverrideError::Save(e.into()))?;
    save_json(&settings_path(), &json).map_err(OverrideError::Save)?;
    for source in sources {
        crate::events::emit(crate::events::EventKind::LibraryChanged { source });
    }
    Ok(out)
}

/// Validated, in the stored shape: the name trimmed, the pad by its own name.
fn checked(mut o: GameOverrides) -> std::result::Result<GameOverrides, OverrideError> {
    o.validate().map_err(OverrideError::Invalid)?;
    o.name = o.name.map(|n| n.trim().to_string());
    o.gamepad = o.gamepad_pref().map(|p| p.as_str().to_string());
    Ok(o)
}

/// A shared override needs a name no other one has, ignoring case.
fn checked_set(
    doc: &OverridesDoc,
    id: Option<&str>,
    o: GameOverrides,
) -> std::result::Result<GameOverrides, OverrideError> {
    let o = checked(o)?;
    let name = o
        .name
        .as_deref()
        .ok_or(OverrideError::Invalid("name"))?
        .to_lowercase();
    let taken = doc.sets.iter().any(|(other, s)| {
        Some(other.as_str()) != id && s.name.as_deref().is_some_and(|n| n.to_lowercase() == name)
    });
    if taken {
        return Err(OverrideError::NameTaken);
    }
    Ok(o)
}

fn is_set_id(s: &str) -> bool {
    s.len() == 12 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn checked_entry(
    doc: &OverridesDoc,
    mut e: OverrideEntry,
) -> std::result::Result<OverrideEntry, OverrideError> {
    e.values.name = None;
    if let Some(set) = e.set.as_deref() {
        if !is_set_id(set) || !e.values.is_empty() {
            return Err(OverrideError::Invalid("set"));
        }
        if !doc.sets.contains_key(set) {
            return Err(OverrideError::NoSuchSet);
        }
    }
    e.values = checked(e.values)?;
    Ok(e)
}

fn put(map: &mut BTreeMap<String, OverrideEntry>, key: &str, e: OverrideEntry) {
    if e.is_empty() {
        map.remove(key);
    } else {
        map.insert(key.to_string(), e);
    }
}

/// One game's entry, as stored after the call. Not checked against the catalog, like a hide:
/// an absent title keeps its entry for when it returns.
pub fn set_game_overrides(
    id: &str,
    entry: OverrideEntry,
) -> std::result::Result<OverrideEntry, OverrideError> {
    mutate(|doc| {
        let entry = checked_entry(doc, entry)?;
        put(&mut doc.games, id, entry.clone());
        Ok((entry, BTreeSet::from([store_of(id).to_string()])))
    })
}

/// One source's entry, which each of its titles without one of its own takes.
pub fn set_source_overrides(
    source: &str,
    entry: OverrideEntry,
) -> std::result::Result<OverrideEntry, OverrideError> {
    mutate(|doc| {
        let entry = checked_entry(doc, entry)?;
        put(&mut doc.sources, source, entry.clone());
        Ok((entry, BTreeSet::from([source.to_string()])))
    })
}

pub fn create_override_set(o: GameOverrides) -> std::result::Result<OverrideSet, OverrideError> {
    mutate(|doc| {
        let values = checked_set(doc, None, o)?;
        let id = loop {
            let id = hex::encode(rand::random::<[u8; 6]>());
            if !doc.sets.contains_key(&id) {
                break id;
            }
        };
        doc.sets.insert(id.clone(), values.clone());
        Ok((OverrideSet { id, values }, BTreeSet::new()))
    })
}

pub fn update_override_set(
    id: &str,
    patch: OverrideSetPatch,
) -> std::result::Result<OverrideSet, OverrideError> {
    mutate(|doc| {
        let mut o = doc.sets.get(id).cloned().ok_or(OverrideError::NoSuchSet)?;
        patch.apply(&mut o);
        let values = checked_set(doc, Some(id), o)?;
        doc.sets.insert(id.to_string(), values.clone());
        let reach = doc.reach(id);
        Ok((
            OverrideSet {
                id: id.to_string(),
                values,
            },
            reach,
        ))
    })
}

/// Removes a shared override and every entry that names it.
pub fn delete_override_set(id: &str) -> std::result::Result<OverrideUsers, OverrideError> {
    mutate(|doc| {
        doc.sets.remove(id).ok_or(OverrideError::NoSuchSet)?;
        let reach = doc.reach(id);
        let (games, sources) = (doc.games.len(), doc.sources.len());
        doc.games.retain(|_, e| e.set.as_deref() != Some(id));
        doc.sources.retain(|_, e| e.set.as_deref() != Some(id));
        let users = OverrideUsers {
            games: games - doc.games.len(),
            sources: sources - doc.sources.len(),
        };
        Ok((users, reach))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented example, as an operator may hand-edit it.
    const DOC: &str = r#"{"version":1,
        "sets":{"a1b2c3d4e5f6":{"name":"1080p60 DualSense","lines":1080,"hz":60,"gamepad":"dualsense"}},
        "games":{"steam:1091500":{"set":"a1b2c3d4e5f6"},"steam:413150":{"lines":2160,"hdr":false},
                 "heroic:legendary:fc0b13b7":{"set":"0123456789ab"}},
        "sources":{"lutris":{"gamepad":"xbox"},"heroic":{"hz":30}}}"#;

    fn doc() -> OverridesDoc {
        serde_json::from_str(DOC).expect("the documented shape parses")
    }

    #[test]
    fn a_game_takes_its_set_its_own_values_or_its_source() {
        let d = doc();
        let set = d.resolve("steam:1091500").expect("the set");
        assert_eq!(set.name.as_deref(), Some("1080p60 DualSense"));
        assert_eq!((set.lines, set.hz), (Some(1080), Some(60)));
        let own = d.resolve("steam:413150").expect("its own values");
        assert_eq!(
            (own.name, own.lines, own.hdr),
            (None, Some(2160), Some(false))
        );
        let source = d.resolve("lutris:4").expect("the source's");
        assert_eq!(
            source.gamepad.as_deref(),
            Some("xbox360"),
            "by its own name"
        );
        assert_eq!(d.resolve("steam:70"), None, "no entry, no source entry");
    }

    /// A game's entry shadows its source's even when the set it names is gone.
    #[test]
    fn a_dangling_set_overrides_nothing() {
        assert_eq!(doc().resolve("heroic:legendary:fc0b13b7"), None);
        assert!(
            doc().resolve("heroic:other").is_some(),
            "the source still covers the rest"
        );
    }

    #[test]
    fn malformed_or_out_of_range_overrides_nothing() {
        let d: OverridesDoc = serde_json::from_str("{ not json").unwrap_or_default();
        assert_eq!(d.resolve("steam:70"), None);
        let d: OverridesDoc =
            serde_json::from_str(r#"{"games":{"steam:70":{"lines":99999}}}"#).unwrap();
        assert_eq!(d.resolve("steam:70"), None);
        let d: OverridesDoc =
            serde_json::from_str(r#"{"games":{"steam:70":{"name":"x"}}}"#).unwrap();
        assert_eq!(d.resolve("steam:70"), None, "a name alone sets nothing");
    }

    #[test]
    fn the_file_roundtrips_its_documented_shape() {
        let json = serde_json::to_value(doc()).unwrap();
        let want: serde_json::Value = serde_json::from_str(DOC).unwrap();
        assert_eq!(json, want);
    }

    #[test]
    fn an_entry_is_a_set_or_values_never_both() {
        let d = doc();
        let entry = |json: &str| serde_json::from_str::<OverrideEntry>(json).unwrap();
        assert!(matches!(
            checked_entry(&d, entry(r#"{"set":"a1b2c3d4e5f6","hz":60}"#)),
            Err(OverrideError::Invalid("set"))
        ));
        assert!(matches!(
            checked_entry(&d, entry(r#"{"set":"A1B2C3D4E5F6"}"#)),
            Err(OverrideError::Invalid("set"))
        ));
        assert!(matches!(
            checked_entry(&d, entry(r#"{"set":"0123456789ab"}"#)),
            Err(OverrideError::NoSuchSet)
        ));
        assert!(matches!(
            checked_entry(&d, entry(r#"{"gamepad":"joycon"}"#)),
            Err(OverrideError::Invalid("gamepad"))
        ));
        let stored = checked_entry(&d, entry(r#"{"name":"mine","gamepad":"DS4"}"#)).unwrap();
        assert_eq!(
            stored.values.name, None,
            "only a shared override has a name"
        );
        assert_eq!(stored.values.gamepad.as_deref(), Some("dualshock4"));
    }

    #[test]
    fn a_shared_override_needs_a_name_no_other_one_has() {
        let d = doc();
        let named = |n: Option<&str>| GameOverrides {
            name: n.map(str::to_string),
            lines: Some(720),
            ..Default::default()
        };
        assert!(matches!(
            checked_set(&d, None, named(None)),
            Err(OverrideError::Invalid("name"))
        ));
        assert!(matches!(
            checked_set(&d, None, named(Some(" 1080P60 dualsense "))),
            Err(OverrideError::NameTaken)
        ));
        let renamed = checked_set(&d, Some("a1b2c3d4e5f6"), named(Some(" 1080P60 dualsense ")));
        assert_eq!(renamed.unwrap().name.as_deref(), Some("1080P60 dualsense"));
    }

    /// Clients get the pad by its own name, whatever alias the file holds.
    #[test]
    fn a_stored_pad_alias_resolves_by_its_own_name() {
        let d: OverridesDoc = serde_json::from_str(
            r#"{"games":{"steam:1":{"gamepad":"ds4"},"steam:2":{"lines":1080,"gamepad":"joycon"}}}"#,
        )
        .unwrap();
        let pad = |id| d.resolve(id).and_then(|o| o.gamepad);
        assert_eq!(pad("steam:1").as_deref(), Some("dualshock4"));
        assert_eq!(pad("steam:2"), None, "an unknown pad is dropped");
        assert_eq!(d.resolve("steam:2").and_then(|o| o.lines), Some(1080));
    }

    /// The API document's `GameOverrides` names every field the core's type sends.
    #[test]
    fn the_schema_names_every_field_the_core_sends() {
        use utoipa::PartialSchema;
        let full = GameOverrides {
            name: Some("4K".into()),
            lines: Some(2160),
            hz: Some(60),
            hdr: Some(true),
            gamepad: Some("xbox360".into()),
            beyond_screen: true,
        };
        let keys = |v: &serde_json::Value| -> BTreeSet<String> {
            v.as_object().expect("an object").keys().cloned().collect()
        };
        let schema = serde_json::to_value(GameOverridesSchema::schema()).unwrap();
        assert_eq!(
            keys(&serde_json::to_value(full).unwrap()),
            keys(&schema["properties"])
        );
    }

    #[test]
    fn a_patch_keeps_what_it_leaves_out_and_null_clears() {
        let mut o = doc().sets["a1b2c3d4e5f6"].clone();
        let patch: OverrideSetPatch =
            serde_json::from_str(r#"{"hz":null,"lines":720,"beyond_screen":true}"#).unwrap();
        patch.apply(&mut o);
        assert_eq!((o.lines, o.hz, o.beyond_screen), (Some(720), None, true));
        assert_eq!(o.gamepad.as_deref(), Some("dualsense"));
        assert_eq!(o.name.as_deref(), Some("1080p60 DualSense"));
    }

    #[test]
    fn a_set_reaches_the_sources_of_its_users() {
        let mut d = doc();
        d.sources.insert(
            "custom".into(),
            OverrideEntry {
                set: Some("a1b2c3d4e5f6".into()),
                ..Default::default()
            },
        );
        let reach: Vec<String> = d.reach("a1b2c3d4e5f6").into_iter().collect();
        assert_eq!(reach, ["custom", "steam"]);
    }
}
