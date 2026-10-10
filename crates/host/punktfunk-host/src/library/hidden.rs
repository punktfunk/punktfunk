//! Per-title hide list. `scanners.rs` hides a whole source; this hides one id.
//!
//! A side table, not a field on the entry: scanner and plugin titles are rebuilt
//! on every scan, so a flag written onto one would vanish at the next reconcile.
//! Keys are the stable `<store>:<external_id>` ids — the same pin GameStream app
//! ids and client art caches already use.
//!
//! [`all_games`](crate::library::all_games) drops hidden ids from every play
//! surface, including launch resolution. The console still lists them so they
//! can be un-hidden. Nothing is deleted. Pin `library-hidden.json` in the tests
//! below.

use super::*;

/// `library-hidden.json`. Not shared with `library-scanners.json`: `steam` is a
/// source id, `steam:70` is a title id — mixing them corrupts both.
#[derive(Debug, Default, Serialize, Deserialize)]
struct HiddenSettings {
    #[serde(default)]
    hidden: Vec<String>,
}

fn settings_path() -> PathBuf {
    pf_paths::seat::library_dir().join("library-hidden.json")
}

/// Malformed or absent file → nothing hidden. A bad parse must show too much,
/// not an empty library.
fn load_settings() -> HiddenSettings {
    read_json_or_default(&settings_path())
}

pub(crate) fn hidden_ids() -> HashSet<String> {
    load_settings().hidden.into_iter().collect()
}

/// `library.changed` source. A missing `:` yields the whole id, not `""` —
/// a blank source matches no cache key.
fn store_of(id: &str) -> &str {
    id.split_once(':').map_or(id, |(store, _)| store)
}

/// Not checked against the live catalog: a title can be absent (launcher closed,
/// disk unmounted) and the hide must still stick when it returns.
pub fn set_entry_hidden(id: &str, hidden: bool) -> Result<bool> {
    let mut settings = load_settings();
    let was_hidden = settings.hidden.iter().any(|h| h == id);
    if was_hidden == hidden {
        return Ok(hidden);
    }
    if hidden {
        settings.hidden.push(id.to_string());
        settings.hidden.sort();
        settings.hidden.dedup();
    } else {
        settings.hidden.retain(|h| h != id);
    }
    save_json(&settings_path(), &serde_json::to_string_pretty(&settings)?)?;
    crate::events::emit(crate::events::EventKind::LibraryChanged {
        source: store_of(id).to_string(),
    });
    Ok(hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Event source is the store prefix: client caches and console invalidation
    /// group on that, not the whole id.
    #[test]
    fn store_of_takes_the_prefix_and_tolerates_a_bare_id() {
        assert_eq!(store_of("steam:70"), "steam");
        assert_eq!(store_of("custom:abc"), "custom");
        // External ids may contain `:`. Split on the first, or the store is wrong.
        assert_eq!(store_of("heroic:legendary:fc0b13b7"), "heroic");
        assert_eq!(store_of("weird-no-colon"), "weird-no-colon");
    }

    #[test]
    fn malformed_settings_hide_nothing() {
        let s: HiddenSettings = serde_json::from_str("{ not json").unwrap_or_default();
        assert!(s.hidden.is_empty());
        let s: HiddenSettings = serde_json::from_str("{}").expect("an empty object is valid");
        assert!(s.hidden.is_empty(), "absent key means nothing hidden");
    }

    /// The persisted shape is the contract an operator may hand-edit — pin it.
    #[test]
    fn settings_roundtrip_the_documented_shape() {
        let s: HiddenSettings =
            serde_json::from_str(r#"{"hidden":["steam:70","lutris:4"]}"#).expect("parses");
        assert_eq!(s.hidden, vec!["steam:70", "lutris:4"]);
        let json = serde_json::to_string(&s).expect("serializes");
        assert_eq!(json, r#"{"hidden":["steam:70","lutris:4"]}"#);
    }
}
