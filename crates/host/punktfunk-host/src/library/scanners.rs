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
    /// With a profile in view (`as`), who turned the source off. Only the box turns on what the
    /// box turned off.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(required = false)]
    pub off_by: Option<Layer>,
}

/// Whose file a value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    /// The box's library, which every profile sees.
    Box,
    /// The profile in view's own layer.
    Profile,
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

/// Whether a host that plays as `reach` leaves `provider`'s titles out of every surface.
fn left_out(provider: &str, reach: Reach) -> bool {
    PER_ACCOUNT_PROVIDERS.contains(&provider) && reach.own_host
}

/// Whether `e` is out of every library surface on a host that plays as `reach`: its source is
/// switched off (`off` from [`disabled_scanners`]), its provider's titles are one account's and
/// the host is a seat, the host is a Linux seat and `e` doesn't play there, or `e` starts the
/// operator's own command and the host is on a person's own account.
pub(crate) fn source_off(e: &CustomEntry, off: &HashSet<String>, reach: Reach) -> bool {
    source_id_for(e).is_some_and(|src| off.contains(src))
        || e.provider.as_deref().is_some_and(|p| left_out(p, reach))
        || (!plays_on_a_seat(e) && reach.linux_seat)
        || (starts_a_box_command(e) && reach.own_account)
}

/// Whether `e`'s launch is a program the operator wrote into the box's library, which must not
/// run as a person on their own account ([`crate::hooks::own_account_refusal`]). A kind the host
/// builds the command for itself names a title, not a program.
fn starts_a_box_command(e: &CustomEntry) -> bool {
    super::custom::privileged_field(e.launch.as_ref(), &[]).is_some()
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

/// Beside library.json (see `custom_path` for the rationale).
const FILE: &str = "library-scanners.json";

/// Absent or malformed file → all on (warn, do not fail the library read).
fn load_settings(view: &View) -> ScannerSettings {
    read_json_or_default(&view.path(FILE))
}

/// The source ids the box turned off, and those the profile in view turned off itself.
fn disabled_layers(view: &View) -> (HashSet<String>, HashSet<String>) {
    let ids = |s: ScannerSettings| s.disabled.into_iter().collect::<HashSet<_>>();
    let own = view
        .own_file(FILE)
        .map(|path| ids(read_json_or_default(&path)));
    (
        ids(load_settings(&View::default())),
        own.unwrap_or_default(),
    )
}

/// Source ids switched off in `view`, by the box or the profile. [`all_games`] filters each
/// entry on this set.
pub(crate) fn disabled_scanners(view: &View) -> HashSet<String> {
    let (by_box, by_profile) = disabled_layers(view);
    by_box.into_iter().chain(by_profile).collect()
}

/// Every source in `view` with its enable state: claimed stores, then
/// providers that have entries but never claimed a store (rom-manager, playnite), then
/// plugins with nothing published yet whose folder requests wait for the operator — the
/// source line is where those requests are shown. Sorted by id for the console. A seat lists no
/// per-account source, and a Linux seat only those with a title it plays.
pub fn list_scanners(view: &View) -> Vec<ScannerInfo> {
    let (by_box, by_profile) = disabled_layers(view);
    let claims = crate::library::claimed_stores();
    let catalog = crate::library::catalog();
    let seat = view.reach.linux_seat;
    let entries: Vec<&CustomEntry> = catalog
        .entries
        .iter()
        .filter(|e| !seat || plays_on_a_seat(e))
        .collect();

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
    // Each plugin's requests are in the grants file where it lives: a seat's own, or the box's.
    let open = crate::plugins::access::AccessStore::open;
    let snapshot = |dir| open(dir).snapshot().unwrap_or_default();
    let mut asked = snapshot(pf_paths::seat::library_dir());
    asked.retain(|s| !crate::plugins::manifest::seat_owns(&s.plugin));
    if crate::library::keeps_provider_rows() {
        let own = snapshot(pf_paths::config_dir()).into_iter();
        asked.extend(own.filter(|s| crate::plugins::manifest::seat_owns(&s.plugin)));
    }
    for s in asked {
        if !s.pending.is_empty() && !plugin_ids.iter().any(|(_, p)| *p == s.plugin) {
            plugin_ids.push((s.plugin.clone(), s.plugin));
        }
    }
    plugin_ids.retain(|(_, provider)| !left_out(provider, view.reach));
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
            let off_by = match (by_box.contains(&id), by_profile.contains(&id)) {
                (true, _) => Some(Layer::Box),
                (false, true) => Some(Layer::Profile),
                (false, false) => None,
            };
            ScannerInfo {
                label: store_label(&id),
                enabled: off_by.is_none(),
                origin: SourceOrigin::Plugin,
                provider: Some(provider),
                entries: Some(count),
                off_by: off_by.filter(|_| view.layer().is_some()),
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

/// Enable or disable one source in `view`'s layer. `None` (unknown or never reconciled) → mgmt
/// maps to 404. Persists and emits `library.changed` only when the layer's state changed
/// (repeated PUT is a no-op). A profile's `enabled` clears only its own switch. Load-edit-save
/// runs under one lock, and a file that does not parse is refused rather than replaced.
pub fn set_scanner_enabled(
    view: &View,
    id: &str,
    enabled: bool,
) -> Result<Option<Vec<ScannerInfo>>> {
    if !list_scanners(view).iter().any(|s| s.id == id) {
        return Ok(None);
    }
    let changed = {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut settings: ScannerSettings = read_json_for_write(&view.path(FILE))?;
        let was_disabled = settings.disabled.iter().any(|d| d == id);
        if enabled != was_disabled {
            false
        } else {
            if enabled {
                settings.disabled.retain(|d| d != id);
            } else {
                settings.disabled.push(id.to_string());
                settings.disabled.sort();
                settings.disabled.dedup();
            }
            save_json(&view.path(FILE), &serde_json::to_string_pretty(&settings)?)?;
            true
        }
    };
    if changed {
        crate::events::emit(crate::events::EventKind::LibraryChanged {
            source: id.to_string(),
        });
    }
    Ok(Some(list_scanners(view)))
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

    /// A person's own account starts no program the operator wrote into the box's library; a
    /// title the host builds the command for stays.
    #[test]
    fn only_a_command_tile_is_the_box_s_program() {
        let entry = |launch: serde_json::Value| -> CustomEntry {
            serde_json::from_value(serde_json::json!({"id": "r", "title": "T", "launch": launch}))
                .unwrap()
        };
        let box_command = |kind: &str, value: &str| {
            starts_a_box_command(&entry(serde_json::json!({"kind": kind, "value": value})))
        };
        assert!(box_command("command", "rsync ~/Games nas:/backup"));
        assert!(box_command("a-kind-nobody-listed", "x"));
        assert!(!box_command("steam_appid", "570"));
        assert!(!box_command("steam_ui", "bigpicture"));
        assert!(!box_command("emulator", "dolphin"));
        assert!(!box_command("heroic", "legendary:Fortnite"));
        assert!(!starts_a_box_command(&entry(serde_json::Value::Null)));
    }

    #[test]
    fn absent_or_malformed_settings_mean_all_on() {
        let s = ScannerSettings::default();
        assert!(s.disabled.is_empty());
        let parsed: ScannerSettings = serde_json::from_str("{}").unwrap();
        assert!(parsed.disabled.is_empty(), "missing key defaults to empty");
    }
}
