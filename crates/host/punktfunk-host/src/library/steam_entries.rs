//! The Steam entries the Decky plugin made on this box, in `library-steam-entries.json`: each
//! synced title's library id → the 32-bit app id of its entry, and every other entry it made
//! (`own`). The plugin sends the map whole. Steam lists each entry back as a `steam:<app id>`
//! title, so the library leaves those out ([`SteamEntries::hidden`]).

use super::*;

const FILE: &str = "library-steam-entries.json";

/// The one shape this host reads.
pub const STEAM_MAP_VERSION: u32 = 1;

/// Far past any library; a map with more entries, or more own ones, is refused whole.
const MAX_ENTRIES: usize = 50_000;

/// A library id is a store-qualified name: bounded, no control bytes.
const MAX_ID_BYTES: usize = 512;

/// Steam's 64-bit game id of a non-Steam entry is its app id over this marker.
const SHORTCUT_MARK: u64 = 0x0200_0000;

/// The map, as stored and as `PUT /steam/entries` takes it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SteamEntries {
    /// Always 1.
    pub version: u32,
    /// Library id → the 32-bit app id of that title's own Steam entry.
    #[serde(default)]
    pub entries: BTreeMap<String, u32>,
    /// Every other entry the plugin made: the Punktfunk ones and the per-game stream ones.
    #[serde(default)]
    pub own: Vec<u32>,
}

impl SteamEntries {
    /// The stored map. Absent or malformed → empty: a bad file hides nothing.
    pub fn load() -> Self {
        read_json_or_default(&path())
    }

    /// The app id of title `id`'s entry.
    pub fn entry_for(&self, id: &str) -> Option<u32> {
        self.entries.get(id).copied()
    }

    /// The title whose entry is app `appid`.
    pub fn id_for(&self, appid: u32) -> Option<&str> {
        let mut found = self.entries.iter().filter(|(_, a)| **a == appid);
        found.next().map(|(id, _)| id.as_str())
    }

    /// Every app id the plugin made, synced or its own.
    pub fn hidden(&self) -> HashSet<u32> {
        self.entries.values().chain(&self.own).copied().collect()
    }

    /// What is wrong with a map someone sent, if anything: version 1, at most [`MAX_ENTRIES`]
    /// of each kind, a library id that is no Steam title's, app ids that are not 0, and no app id
    /// for two titles.
    pub fn check(&self) -> std::result::Result<(), &'static str> {
        if self.version != STEAM_MAP_VERSION {
            return Err("the map's version must be 1");
        }
        if self.entries.len() > MAX_ENTRIES || self.own.len() > MAX_ENTRIES {
            return Err("the map names too many entries");
        }
        let bad_id = |id: &String| {
            id.is_empty()
                || id.len() > MAX_ID_BYTES
                || id.starts_with("steam:")
                || id.chars().any(char::is_control)
        };
        if self.entries.keys().any(bad_id) {
            return Err("an entry's library id is empty, too long or a Steam title's");
        }
        if self.entries.values().chain(&self.own).any(|a| *a == 0) {
            return Err("an app id is 0");
        }
        let unique: HashSet<u32> = self.entries.values().copied().collect();
        if unique.len() != self.entries.len() {
            return Err("two titles name the same app id");
        }
        Ok(())
    }
}

fn path() -> PathBuf {
    pf_paths::seat::library_dir().join(FILE)
}

/// Stores `map` (checked by the caller), one write at a time. `true` when it changed.
pub fn save_steam_entries(map: &SteamEntries) -> Result<bool> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if read_json_or_default::<SteamEntries>(&path()) == *map {
        return Ok(false);
    }
    save_json(&path(), &serde_json::to_string_pretty(map)?)?;
    Ok(true)
}

/// Steam's 64-bit game id of non-Steam entry `appid`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
pub fn steam_gameid(appid: u32) -> u64 {
    (u64::from(appid) << 32) | SHORTCUT_MARK
}

/// The app id of the non-Steam entry Steam's game id `gameid` names; `None` for a Steam game.
pub fn shortcut_appid(gameid: u64) -> Option<u32> {
    (gameid & 0xffff_ffff == SHORTCUT_MARK).then_some((gameid >> 32) as u32)
}

/// Whether the library leaves title `id` out: a `steam:<n>` the plugin made, `n` in `hidden`.
pub(crate) fn synced_away(id: &str, hidden: &HashSet<u32>) -> bool {
    id.strip_prefix("steam:")
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|n| hidden.contains(&n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, u32)], own: &[u32]) -> SteamEntries {
        SteamEntries {
            version: 1,
            entries: entries.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            own: own.to_vec(),
        }
    }

    /// The game id Steam sets for an entry carries its app id over the shortcut mark, and only
    /// such an id names an entry.
    #[test]
    fn a_game_id_names_its_entry_both_ways() {
        assert_eq!(steam_gameid(2_890_012_345), 12_412_508_506_844_823_552);
        assert_eq!(
            shortcut_appid(12_412_508_506_844_823_552),
            Some(2_890_012_345)
        );
        for appid in [1, 0x8000_0000, u32::MAX] {
            assert_eq!(shortcut_appid(steam_gameid(appid)), Some(appid));
        }
        assert_eq!(shortcut_appid(570), None, "a Steam game's id");
        assert_eq!(shortcut_appid(steam_gameid(7) | 1), None);
    }

    /// A map is refused whole for a wrong version, a Steam title, a 0 or a shared app id; the
    /// stored shape is the documented one.
    #[test]
    fn a_map_is_checked_and_keeps_its_shape() {
        let good = map(
            &[("rom-manager:snes/chrono", 2_890_012_345)],
            &[2_764_001_122],
        );
        assert_eq!(good.check(), Ok(()));
        assert_eq!(
            good.entry_for("rom-manager:snes/chrono"),
            Some(2_890_012_345)
        );
        assert_eq!(good.id_for(2_890_012_345), Some("rom-manager:snes/chrono"));
        assert_eq!(
            good.id_for(2_764_001_122),
            None,
            "an own entry plays nothing"
        );
        assert_eq!(good.hidden(), HashSet::from([2_890_012_345, 2_764_001_122]));
        let json = serde_json::to_value(&good).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "version": 1,
                "entries": { "rom-manager:snes/chrono": 2_890_012_345u32 },
                "own": [2_764_001_122u32] })
        );
        let wrong = [
            SteamEntries {
                version: 2,
                ..good.clone()
            },
            map(&[("steam:570", 9)], &[]),
            map(&[("", 9)], &[]),
            map(&[("a:\u{7}", 9)], &[]),
            map(&[(&"x".repeat(513), 9)], &[]),
            map(&[("a:1", 0)], &[]),
            map(&[], &[0]),
            map(&[("a:1", 9), ("a:2", 9)], &[]),
        ];
        for m in wrong {
            assert!(m.check().is_err(), "{m:?}");
        }
        let past_u32 = r#"{"version":1,"entries":{"a:1":4294967296},"own":[]}"#;
        assert!(serde_json::from_str::<SteamEntries>(past_u32).is_err());
    }

    #[test]
    fn only_a_synced_steam_title_is_left_out() {
        let hidden = HashSet::from([9, 11]);
        assert!(synced_away("steam:9", &hidden));
        assert!(!synced_away("steam:10", &hidden));
        assert!(!synced_away("custom:9", &hidden));
        assert!(!synced_away("steam:9x", &hidden));
    }
}
