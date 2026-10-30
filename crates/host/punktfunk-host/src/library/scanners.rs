//! Per-source enable toggles for this host's library. Every source is **on by
//! default**; disabling one hides its titles from every library surface (grid,
//! clients, GameStream app list, launch resolution) on the next read.
//!
//! Only the *disabled* set is persisted, so a source that appears later starts
//! enabled. Ids are stable (provider id = claimed store id = old scanner id),
//! which is why `library-scanners.json` keeps its name and shape across the
//! built-in → plugin extraction.
//!
//! The user-curated **custom** store is not a source and cannot be disabled
//! here. Pin: [`list_scanners`], [`set_scanner_enabled`].

use super::*;

/// One game source and its enable state — the console toggle row.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct ScannerInfo {
    /// Entry `store`, provider id, and store claim. One string, so a disabled
    /// toggle survives the store's plugin taking over.
    #[schema(example = "steam")]
    pub id: String,
    #[schema(example = "Steam")]
    pub label: String,
    pub enabled: bool,
    /// Always `plugin` on this host. `Builtin` remains so OpenAPI still names it for N-1 consoles.
    #[schema(example = "plugin")]
    pub origin: SourceOrigin,
    /// Provider id. `None` only for a `Builtin` source an N-1 host still reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Titles this source currently contributes. `None` on `Builtin` (counting would walk launcher files).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<usize>,
}

/// Origin of a [`ScannerInfo`]. This host emits only `Plugin`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SourceOrigin {
    /// Compiled-in scanner. This host never emits it; keep the variant so OpenAPI
    /// still names `builtin` for an N-1 host the console may drive.
    #[allow(dead_code)]
    Builtin,
    Plugin,
}

/// Console labels for former built-in stores. Unlisted ids (rom-manager, playnite) show as themselves.
const STORE_LABELS: &[(&str, &str)] = &[
    ("steam", "Steam"),
    ("lutris", "Lutris"),
    ("heroic", "Heroic (Epic / GOG / Amazon)"),
    ("epic", "Epic Games Launcher"),
    ("gog", "GOG Galaxy"),
    ("xbox", "Xbox / Game Pass"),
];

/// Providers whose titles belong to one Windows account. A seat host reading the box's library
/// leaves them out: they are the owner's. Manifests that mark a per-account source replace this
/// list (`windows-seat-trust-model.md` L1.1).
const PER_ACCOUNT_PROVIDERS: &[&str] = &["playnite", "windows-games", "amazon", "itch", "hydra"];

/// Whether this host leaves `provider`'s titles out of every surface.
fn left_out_here(provider: &str) -> bool {
    PER_ACCOUNT_PROVIDERS.contains(&provider) && pf_paths::seat::box_library_dir().is_some()
}

/// Whether `e` is out of every library surface: its source is switched off (`off` from
/// [`disabled_scanners`]), its provider's titles are one account's and this host is a seat, or
/// this is a Linux seat and `e` doesn't play there.
pub(crate) fn source_off(e: &CustomEntry, off: &HashSet<String>) -> bool {
    source_id_for(e).is_some_and(|src| off.contains(src))
        || e.provider.as_deref().is_some_and(left_out_here)
        || (!plays_on_a_seat(e) && linux_seat())
}

/// A Linux seat reading the box's library: it plays as its own user, and has no plugins. The door
/// reads the same library and plays nothing, so it lists all of it.
pub(crate) fn linux_seat() -> bool {
    cfg!(target_os = "linux")
        && pf_paths::seat::role().is_seat_host()
        && pf_paths::seat::box_library_dir().is_some()
}

/// Whether a Linux seat can play `e`: its own Steam sees the owner's libraries, and hermir finds
/// the seat's own copy of an emulator. A Steam app id fits 32 bits; a non-Steam shortcut's id is
/// wider and lives in the owner's Steam alone, as does every title another launcher starts from
/// the owner's home. An `exec` title needs its plugin's manifest, which the owner's host holds.
fn plays_on_a_seat(e: &CustomEntry) -> bool {
    e.launch.as_ref().is_some_and(|l| match l.kind.as_str() {
        "steam_appid" => l.value.parse::<u32>().is_ok(),
        "steam_ui" | "emulator" => true,
        _ => false,
    })
}

/// `library-scanners.json`: ids the operator turned off. Absent file = all on.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ScannerSettings {
    #[serde(default)]
    disabled: Vec<String>,
}

fn settings_path() -> PathBuf {
    // Beside library.json (see `custom_path` for the rationale).
    pf_paths::seat::library_dir().join("library-scanners.json")
}

/// Absent or malformed file → all on (warn, do not fail the library read).
fn load_settings() -> ScannerSettings {
    read_json_or_default(&settings_path())
}

/// Disabled source ids. [`all_games`] filters each entry on this set.
pub(crate) fn disabled_scanners() -> HashSet<String> {
    load_settings().disabled.into_iter().collect()
}

/// Every source on this host with its enable state: claimed stores, then
/// providers that have entries but never claimed a store (rom-manager, playnite), then
/// plugins with nothing published yet whose folder requests wait for the operator — the
/// source line is where those requests are shown. Sorted by id for the console. A seat lists no
/// per-account source, and a Linux seat only those with a title it plays.
pub fn list_scanners() -> Vec<ScannerInfo> {
    let off = disabled_scanners();
    let claims = crate::library::claimed_stores();
    let mut entries = crate::library::load_custom();
    let seat = linux_seat();
    if seat {
        entries.retain(plays_on_a_seat);
    }

    let mut plugin_ids: Vec<(String, String)> = claims
        .iter()
        .map(|(store, provider)| (store.clone(), provider.clone()))
        .collect();
    for e in &entries {
        let Some(provider) = e.provider.as_deref() else {
            continue;
        };
        if e.store.is_none() && !plugin_ids.iter().any(|(id, _)| id == provider) {
            plugin_ids.push((provider.to_string(), provider.to_string()));
        }
    }
    let access = crate::plugins::access::AccessStore::open(pf_paths::seat::library_dir());
    for s in access.snapshot().unwrap_or_default() {
        if !s.pending.is_empty() && !plugin_ids.iter().any(|(_, p)| *p == s.plugin) {
            plugin_ids.push((s.plugin.clone(), s.plugin));
        }
    }
    plugin_ids.retain(|(_, provider)| !left_out_here(provider));
    if seat {
        plugin_ids.retain(|(id, _)| {
            entries
                .iter()
                .any(|e| crate::library::source_id_for(e) == Some(id.as_str()))
        });
    }
    plugin_ids.sort();
    plugin_ids.dedup();

    plugin_ids
        .into_iter()
        .map(|(id, provider)| {
            let count = entries
                .iter()
                .filter(|e| crate::library::source_id_for(e) == Some(id.as_str()))
                .count();
            ScannerInfo {
                label: store_label(&id),
                enabled: !off.contains(&id),
                origin: SourceOrigin::Plugin,
                provider: Some(provider),
                entries: Some(count),
                id,
            }
        })
        .collect()
}

fn store_label(id: &str) -> String {
    STORE_LABELS
        .iter()
        .find(|(sid, _)| *sid == id)
        .map(|(_, label)| (*label).to_string())
        .unwrap_or_else(|| id.to_string())
}

/// True if `id` is a claimed store or a provider with entries. Unknown or never-reconciled → 404.
fn is_known_source(id: &str) -> bool {
    list_scanners().iter().any(|s| s.id == id)
}

/// Enable or disable one source. `None` → mgmt maps to 404. Persists and emits
/// `library.changed` only when the state actually changed (repeated PUT is a no-op).
pub fn set_scanner_enabled(id: &str, enabled: bool) -> Result<Option<Vec<ScannerInfo>>> {
    if !is_known_source(id) {
        return Ok(None);
    }
    let mut settings = load_settings();
    let was_disabled = settings.disabled.iter().any(|d| d == id);
    if enabled != was_disabled {
        return Ok(Some(list_scanners()));
    }
    if enabled {
        settings.disabled.retain(|d| d != id);
    } else {
        settings.disabled.push(id.to_string());
        settings.disabled.sort();
        settings.disabled.dedup();
    }
    save_json(&settings_path(), &serde_json::to_string_pretty(&settings)?)?;
    crate::events::emit(crate::events::EventKind::LibraryChanged {
        source: id.to_string(),
    });
    Ok(Some(list_scanners()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_labels_are_unique_and_unknown_ids_degrade_to_themselves() {
        let ids: HashSet<_> = STORE_LABELS.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids.len(), STORE_LABELS.len(), "source ids must be unique");
        // `custom` is a store but never a source — the toggle surface must not offer it.
        assert!(!ids.contains("custom"));

        assert_eq!(store_label("steam"), "Steam");
        assert_eq!(store_label("rom-manager"), "rom-manager");
        assert_eq!(store_label(""), "");
    }

    /// A Linux seat plays what it can start as its own user: a Steam app, the Steam client's
    /// tiles and a catalog emulator, never a shortcut's 64-bit id, another launcher's title or a
    /// template only its plugin's manifest spells out.
    #[test]
    fn a_linux_seat_plays_only_what_it_can_start_itself() {
        let entry = |launch: serde_json::Value| -> CustomEntry {
            serde_json::from_value(serde_json::json!({"id": "r", "title": "T", "launch": launch}))
                .unwrap()
        };
        let plays = |kind: &str, value: &str| {
            plays_on_a_seat(&entry(serde_json::json!({"kind": kind, "value": value})))
        };
        assert!(plays("steam_appid", "570"));
        assert!(plays("steam_ui", "bigpicture"));
        assert!(plays("emulator", "dolphin"));
        assert!(!plays("steam_appid", "11111111111111111111"));
        assert!(!plays("lutris_id", "42"));
        assert!(!plays("command", "dolphin-emu"));
        assert!(!plays("exec", "custom"));
        assert!(!plays_on_a_seat(&entry(serde_json::Value::Null)));
    }

    #[test]
    fn absent_or_malformed_settings_mean_all_on() {
        let s = ScannerSettings::default();
        assert!(s.disabled.is_empty());
        let parsed: ScannerSettings = serde_json::from_str("{}").unwrap();
        assert!(parsed.disabled.is_empty(), "missing key defaults to empty");
    }
}
