//! Game overrides: the stream settings this host sets for one game, or for every title of a
//! source ([`GameOverrides`]). A side table like `hidden.rs`, keyed by the stable
//! `<store>:<external_id>` ids, so a rescan keeps it.
//!
//! An entry names a shared override (`set`), carries values of its own, or is a game's `none`:
//! the device decides, whatever its source sets. A game's entry wins over its source's. A shared
//! override that is gone, or values that fail validation, override nothing. A profile's layer
//! holds a file of the same shape; its entry for a game or its source wins over the box's.
//! [`GameEntry::overrides`] carries what each title resolves to; the handshake holds a client to
//! the downward half. Pin `library-overrides.json` in the tests below.

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

/// A game's or a source's entry: `set` names a shared override, the fields are its own, or a
/// game's `none` opts out. Only one of the three; `{}` is no entry. `name` is the shared
/// override's alone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OverrideEntry {
    /// A game's alone, with no other field: the device decides, and the source's entry is
    /// skipped. A source that sets nothing has no entry.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    pub none: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "a1b2c3d4e5f6")]
    pub set: Option<String>,
    #[serde(flatten)]
    #[schema(value_type = GameOverridesSchema)]
    pub values: GameOverrides,
}

impl OverrideEntry {
    fn is_empty(&self) -> bool {
        !self.none && self.set.is_none() && self.values.is_empty()
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
    /// `games[id] ?? sources[store]`.
    pub(crate) fn entry(&self, id: &str) -> Option<&OverrideEntry> {
        self.games
            .get(id)
            .or_else(|| self.sources.get(store_of(id)))
    }

    /// [`Self::entry`], then the shared override it names, in the stored shape.
    pub(crate) fn resolve(&self, id: &str) -> Option<GameOverrides> {
        resolve_entry(self.entry(id)?, |set| self.sets.get(set))
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

/// `entry` with the shared override it names found by `set`. A pad name the host does not know
/// drops only the pad. Absent when nothing is set, the entry is `none`, or another value fails
/// validation.
fn resolve_entry<'a>(
    entry: &OverrideEntry,
    set: impl Fn(&str) -> Option<&'a GameOverrides>,
) -> Option<GameOverrides> {
    if entry.none {
        return None;
    }
    let mut o = match &entry.set {
        Some(id) => set(id)?.clone(),
        None => GameOverrides {
            name: None,
            ..entry.values.clone()
        },
    };
    // Android knows a pad only by its own name, and a hand-edited one may be any alias.
    o.gamepad = o.gamepad_pref().map(|p| p.as_str().to_string());
    checked(o).ok().filter(|o| !o.is_empty())
}

/// The box's overrides and the profile in view's own, read once per list.
pub(crate) struct Overrides {
    base: OverridesDoc,
    own: Option<OverridesDoc>,
}

impl Overrides {
    pub(crate) fn load(view: &View) -> Self {
        Self {
            base: load_overrides(&view.base()),
            own: view.layer().map(|_| load_overrides(view)),
        }
    }

    /// [`Self::own_entry`] when there is one, else the box's.
    pub(crate) fn resolve(&self, id: &str) -> Option<GameOverrides> {
        match self.own_entry(id) {
            Some(entry) => resolve_entry(entry, |set| self.set(set)),
            None => self.base.resolve(id),
        }
    }

    /// The profile's entry for `id`, its game's then its source's. One naming a shared override
    /// that is gone sets nothing, so the box's applies; a `none` is an entry.
    pub(crate) fn own_entry(&self, id: &str) -> Option<&OverrideEntry> {
        let own = self.own.as_ref()?;
        let live = |e: &&OverrideEntry| e.set.as_deref().is_none_or(|s| self.set(s).is_some());
        let game = own.games.get(id).filter(live);
        game.or_else(|| own.sources.get(store_of(id)).filter(live))
    }

    /// Shared override `id`: the profile's, then the box's.
    fn set(&self, id: &str) -> Option<&GameOverrides> {
        let own = self.own.as_ref().and_then(|own| own.sets.get(id));
        own.or_else(|| self.base.sets.get(id))
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
    /// The field out of range or unknown; `set` or `none` also when values ride beside it.
    Invalid(&'static str),
    /// Another shared override has this name, ignoring case.
    NameTaken,
    NoSuchSet,
    Save(anyhow::Error),
}

const FILE: &str = "library-overrides.json";

/// The file in `view`'s own layer, or the box's. Malformed or absent file → no overrides. A
/// bad parse must cost the overrides, not a stream.
pub fn load_overrides(view: &View) -> OverridesDoc {
    let mut doc: OverridesDoc = read_json_or_default(&view.path(FILE));
    doc.version = VERSION;
    doc
}

/// What the launch of library id `id` in `view` is held to.
pub fn overrides_for(view: &View, id: &str) -> Option<GameOverrides> {
    Overrides::load(view).resolve(id)
}

/// Load `view`'s layer, change and save it under one lock, then one `library.changed` per source
/// `f` names. A bulk assign is many writes at once and must not lose one. No change, no save.
/// `f` also gets the shared override ids a profile's entry may name in the box's file. A layer
/// file that does not parse is refused rather than replaced.
fn mutate<T>(
    view: &View,
    f: impl FnOnce(
        &mut OverridesDoc,
        &BTreeSet<String>,
    ) -> std::result::Result<(T, BTreeSet<String>), OverrideError>,
) -> std::result::Result<T, OverrideError> {
    static LOCK: Mutex<()> = Mutex::new(());
    let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let box_sets = match view.layer() {
        Some(_) => load_overrides(&view.base()).sets.into_keys().collect(),
        None => BTreeSet::new(),
    };
    let mut doc: OverridesDoc =
        read_json_for_write(&view.path(FILE)).map_err(OverrideError::Save)?;
    doc.version = VERSION;
    let before = doc.clone();
    let (out, sources) = f(&mut doc, &box_sets)?;
    if doc == before {
        return Ok(out);
    }
    let json = serde_json::to_string_pretty(&doc).map_err(|e| OverrideError::Save(e.into()))?;
    save_json(&view.path(FILE), &json).map_err(OverrideError::Save)?;
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

/// `e` as stored. Its `set` names a shared override in `doc` or in `box_sets`, the box's that a
/// profile's entry may name.
fn checked_entry(
    doc: &OverridesDoc,
    box_sets: &BTreeSet<String>,
    mut e: OverrideEntry,
) -> std::result::Result<OverrideEntry, OverrideError> {
    e.values.name = None;
    if e.none && (e.set.is_some() || !e.values.is_empty()) {
        return Err(OverrideError::Invalid("none"));
    }
    if let Some(set) = e.set.as_deref() {
        if !is_set_id(set) || !e.values.is_empty() {
            return Err(OverrideError::Invalid("set"));
        }
        if !doc.sets.contains_key(set) && !box_sets.contains(set) {
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

/// One game's entry in `view`'s layer, as stored after the call. Not checked against the
/// catalog, like a hide: an absent title keeps its entry for when it returns.
pub fn set_game_overrides(
    view: &View,
    id: &str,
    entry: OverrideEntry,
) -> std::result::Result<OverrideEntry, OverrideError> {
    mutate(view, |doc, box_sets| {
        let entry = checked_entry(doc, box_sets, entry)?;
        put(&mut doc.games, id, entry.clone());
        Ok((entry, BTreeSet::from([store_of(id).to_string()])))
    })
}

/// One source's entry in `view`'s layer, which each of its titles without one of its own takes.
/// Never `none`.
pub fn set_source_overrides(
    view: &View,
    source: &str,
    entry: OverrideEntry,
) -> std::result::Result<OverrideEntry, OverrideError> {
    if entry.none {
        return Err(OverrideError::Invalid("none"));
    }
    mutate(view, |doc, box_sets| {
        let entry = checked_entry(doc, box_sets, entry)?;
        put(&mut doc.sources, source, entry.clone());
        Ok((entry, BTreeSet::from([source.to_string()])))
    })
}

/// A shared override in `view`'s layer.
pub fn create_override_set(
    view: &View,
    o: GameOverrides,
) -> std::result::Result<OverrideSet, OverrideError> {
    mutate(view, |doc, _| {
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

/// A shared override of `view`'s layer; a profile's changes only its own.
pub fn update_override_set(
    view: &View,
    id: &str,
    patch: OverrideSetPatch,
) -> std::result::Result<OverrideSet, OverrideError> {
    mutate(view, |doc, _| {
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

/// Removes a shared override of `view`'s layer and every entry of that layer that names it.
pub fn delete_override_set(
    view: &View,
    id: &str,
) -> std::result::Result<OverrideUsers, OverrideError> {
    mutate(view, |doc, _| {
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
                 "heroic:legendary:fc0b13b7":{"set":"0123456789ab"},"lutris:7":{"none":true}},
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

    /// `none` skips the source's entry, and with no source entry it is no override either.
    #[test]
    fn none_opts_a_game_out_of_its_source() {
        let mut d = doc();
        assert_eq!(d.resolve("lutris:7"), None, "the source's pad is skipped");
        assert!(
            d.resolve("lutris:4").is_some(),
            "the source covers the rest"
        );
        d.sources.remove("lutris");
        assert_eq!(d.resolve("lutris:7"), None);
    }

    /// A profile's entry for a game, or for its source, beats the box's; its `set` may name the
    /// box's shared override; where it set nothing, the box's applies.
    #[test]
    fn a_profile_s_own_entry_wins_and_the_box_s_fills_the_rest() {
        let own: OverridesDoc = serde_json::from_str(
            r#"{"sets":{"0123456789ab":{"name":"Mine","hz":30}},
                "games":{"steam:413150":{"none":true},"steam:1":{"set":"a1b2c3d4e5f6"},
                         "steam:2":{"set":"0123456789ab"}},
                "sources":{"lutris":{"gamepad":"dualsense"}}}"#,
        )
        .unwrap();
        let o = Overrides {
            base: doc(),
            own: Some(own.clone()),
        };
        assert_eq!(o.resolve("steam:413150"), None, "its none beats the box's");
        let named = |id| o.resolve(id).and_then(|v| v.name);
        assert_eq!(named("steam:1").as_deref(), Some("1080p60 DualSense"));
        assert_eq!(o.resolve("steam:2").and_then(|v| v.hz), Some(30));
        let pad = |id| o.resolve(id).and_then(|v| v.gamepad);
        assert_eq!(pad("lutris:7").as_deref(), Some("dualsense"), "its source");
        assert_eq!(named("steam:1091500").as_deref(), Some("1080p60 DualSense"));
        let alone = Overrides {
            base: doc(),
            own: None,
        };
        assert_eq!(alone.resolve("steam:413150"), doc().resolve("steam:413150"));

        let box_sets = BTreeSet::from(["a1b2c3d4e5f6".to_string()]);
        let entry: OverrideEntry = serde_json::from_str(r#"{"set":"a1b2c3d4e5f6"}"#).unwrap();
        assert!(checked_entry(&own, &box_sets, entry.clone()).is_ok());
        assert!(matches!(
            checked_entry(&own, &BTreeSet::new(), entry),
            Err(OverrideError::NoSuchSet)
        ));
    }

    /// A profile's entry, for a game or its source, naming a shared override that is gone sets
    /// nothing: the box's applies. Its `none` still wins.
    #[test]
    fn a_profile_s_entry_on_a_gone_set_leaves_the_box_s() {
        let own: OverridesDoc = serde_json::from_str(
            r#"{"games":{"steam:413150":{"set":"fedcba987654"},"steam:1091500":{"none":true}},
                "sources":{"heroic":{"set":"fedcba987654"}}}"#,
        )
        .unwrap();
        let o = Overrides {
            base: doc(),
            own: Some(own),
        };
        for id in ["steam:413150", "heroic:other"] {
            assert!(o.own_entry(id).is_none(), "{id}");
            assert_eq!(o.resolve(id), doc().resolve(id), "{id}");
            assert!(o.resolve(id).is_some(), "{id}");
        }
        assert!(o.own_entry("steam:1091500").is_some());
        assert_eq!(
            o.resolve("steam:1091500"),
            None,
            "its none beats the box's set"
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
    fn an_entry_is_a_set_values_or_none_never_two() {
        let d = doc();
        let entry = |json: &str| serde_json::from_str::<OverrideEntry>(json).unwrap();
        for both in [
            r#"{"none":true,"hz":60}"#,
            r#"{"none":true,"set":"a1b2c3d4e5f6"}"#,
        ] {
            assert!(
                matches!(
                    checked_entry(&d, &BTreeSet::new(), entry(both)),
                    Err(OverrideError::Invalid("none"))
                ),
                "{both}"
            );
        }
        assert!(
            checked_entry(&d, &BTreeSet::new(), entry(r#"{"none":true}"#))
                .unwrap()
                .none
        );
        assert!(matches!(
            checked_entry(
                &d,
                &BTreeSet::new(),
                entry(r#"{"set":"a1b2c3d4e5f6","hz":60}"#)
            ),
            Err(OverrideError::Invalid("set"))
        ));
        assert!(matches!(
            checked_entry(&d, &BTreeSet::new(), entry(r#"{"set":"A1B2C3D4E5F6"}"#)),
            Err(OverrideError::Invalid("set"))
        ));
        assert!(matches!(
            checked_entry(&d, &BTreeSet::new(), entry(r#"{"set":"0123456789ab"}"#)),
            Err(OverrideError::NoSuchSet)
        ));
        assert!(matches!(
            checked_entry(&d, &BTreeSet::new(), entry(r#"{"gamepad":"joycon"}"#)),
            Err(OverrideError::Invalid("gamepad"))
        ));
        let stored = checked_entry(
            &d,
            &BTreeSet::new(),
            entry(r#"{"name":"mine","gamepad":"DS4"}"#),
        )
        .unwrap();
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
