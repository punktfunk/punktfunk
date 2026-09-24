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

/// `library-scanners.json`: ids the operator turned off. Absent file = all on.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ScannerSettings {
    #[serde(default)]
    disabled: Vec<String>,
}

fn settings_path() -> PathBuf {
    // Same hardened config dir as library.json / hooks.json (see `custom_path` for the rationale).
    pf_paths::config_dir().join("library-scanners.json")
}

/// Absent or malformed file → all on (warn, do not fail the library read).
fn load_settings() -> ScannerSettings {
    match std::fs::read_to_string(settings_path()) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "library-scanners.json malformed — all scanners on");
            ScannerSettings::default()
        }),
        Err(_) => ScannerSettings::default(),
    }
}

fn save_settings(settings: &ScannerSettings) -> Result<()> {
    let dir = pf_paths::config_dir();
    pf_paths::create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(settings)?;
    // Write-then-rename like the catalog, so a crash mid-write never truncates the settings.
    let tmp = settings_path().with_extension("json.tmp");
    pf_paths::write_secret_file(&tmp, json.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, settings_path()).context("rename library-scanners.json")?;
    Ok(())
}

/// Disabled source ids. [`all_games`] filters each entry on this set.
pub(crate) fn disabled_scanners() -> HashSet<String> {
    load_settings().disabled.into_iter().collect()
}

/// Every source on this host with its enable state: claimed stores, then
/// providers that have entries but never claimed a store (rom-manager, playnite), then
/// plugins with nothing published yet whose folder requests wait for the operator — the
/// source line is where those requests are shown. Sorted by id for the console.
pub fn list_scanners() -> Vec<ScannerInfo> {
    let off = disabled_scanners();
    let claims = crate::library::claimed_stores();
    let entries = crate::library::load_custom();

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
    let access = crate::plugins::access::AccessStore::open(pf_paths::config_dir());
    for s in access.snapshot().unwrap_or_default() {
        if !s.pending.is_empty() && !plugin_ids.iter().any(|(_, p)| *p == s.plugin) {
            plugin_ids.push((s.plugin.clone(), s.plugin));
        }
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
    save_settings(&settings)?;
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

    #[test]
    fn absent_or_malformed_settings_mean_all_on() {
        let s = ScannerSettings::default();
        assert!(s.disabled.is_empty());
        let parsed: ScannerSettings = serde_json::from_str("{}").unwrap();
        assert!(parsed.disabled.is_empty(), "missing key defaults to empty");
    }
}
