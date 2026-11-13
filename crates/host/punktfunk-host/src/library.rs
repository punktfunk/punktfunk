//! One [`GameEntry`] grid over every source of titles on this host.
//!
//! Every source is a plugin. Plugins reconcile titles into the stored catalog over the
//! provider API, each claiming its store so entries keep stable `<store>:<external_id>`
//! ids (`custom.rs`, design D2). The operator-typed **custom** store lives in the same
//! catalog and is not a plugin.
//!
//! A plugin publishes a validated [`LaunchSpec`]; the host builds the command
//! (`launch.rs`, design D1). Source toggles live in `scanners.rs`. Artwork rides on
//! the entries; [`art`] proxies local files so a client never sees an unreachable path.
//! Art & Metadata sources fill what an entry lacks at read time (`metadata.rs`).
//!
//! This module is read-mostly metadata. Launching a chosen title is `launch.rs`.

pub(crate) use anyhow::{Context, Result};
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use sha2::{Digest, Sha256};
pub(crate) use std::collections::{BTreeMap, HashSet};
pub(crate) use std::path::{Path, PathBuf};
pub(crate) use std::time::{SystemTime, UNIX_EPOCH};
pub(crate) use utoipa::ToSchema;

mod art;
mod custom;
mod detect;
pub mod downloads;
mod hidden;
mod launch;
mod metadata;
mod overrides;
mod profile;
mod scanners;
mod stats;

pub use art::*;
pub use custom::*;
pub use detect::*;
pub use hidden::*;
pub use launch::*;
pub use metadata::*;
pub use overrides::*;
pub use profile::*;
pub use scanners::*;
pub use stats::*;

/// Cover art. The client prefers `portrait` for a grid and falls back to `header`
/// when a title has no 600×900 capsule (common for older Steam apps).
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct Artwork {
    /// Steam `library_600x900.jpg`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portrait: Option<String>,
    /// Steam `library_hero.jpg`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hero: Option<String>,
    /// Steam `logo.png`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logo: Option<String>,
    /// Steam `header.jpg` — the universal fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

/// How the host launches a title. Open-ended so new stores slot in:
/// `steam_appid` → `steam steam://rungameid/<value>`; `command` → run `<value>`
/// nested in a gamescope session.
#[derive(Default, Clone, Debug, Serialize, Deserialize, ToSchema)]
pub struct LaunchSpec {
    /// `"steam_appid"` or `"command"`.
    #[schema(example = "steam_appid")]
    pub kind: String,
    /// Appid for `steam_appid`, the shell command for `command`, or — for `exec` — the name of a
    /// template in the publishing plugin's manifest.
    pub value: String,
    /// Values for an `exec` template's parameters. Each is checked against the character class
    /// the manifest declares for it, and a path must sit inside the paths that manifest names.
    /// A list rather than a map: a map of strings generates as an untyped object in the SDK.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<LaunchArg>>,
}

/// One `{param}` value for an `exec` template.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct LaunchArg {
    /// The parameter's name in the template.
    pub name: String,
    pub value: String,
}

/// Optional display metadata, `#[serde(flatten)]`-ed into [`GameEntry`].
///
/// Values are free-form strings, not enums — emulation sources (RomM, EmuDeck,
/// Playnite) each have their own vocabulary and the host does not normalize it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct GameMeta {
    /// `"PS2"`, `"Xbox 360"`, `"SNES"`, … Installed-store scanners stamp `"PC"`;
    /// `GET /library?platform=` filters on it (case-insensitive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "PS2")]
    pub platform: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Year of first release — the granularity metadata sources agree on.
    #[serde(
        default,
        deserialize_with = "lenient_uint",
        skip_serializing_if = "Option::is_none"
    )]
    #[schema(example = 2001)]
    pub release_year: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(
        default,
        deserialize_with = "lenient_uint",
        skip_serializing_if = "Option::is_none"
    )]
    pub players: Option<u8>,
}

/// A number that does not fit the field (`players: 300`, `2001.5`, a string) reads as absent.
/// Display metadata must not refuse a whole reconcile.
fn lenient_uint<'de, D, T>(d: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: TryFrom<u64>,
{
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(v.and_then(|v| v.as_u64()).and_then(|n| T::try_from(n).ok()))
}

/// Presentation hint: ordinary title vs the launcher itself (Steam Big Picture,
/// Heroic, Playnite fullscreen). A launcher entry launches, leases, and lists
/// like a game (design D4). Serde-default `game`; skipped when default so the
/// wire is unchanged for entries that don't opt in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum GameRole {
    #[default]
    Game,
    Launcher,
}

impl GameRole {
    pub(crate) fn is_game(&self) -> bool {
        matches!(self, Self::Game)
    }
}

/// Cap on [`GameEntry::icon`]. Fits a slug like `epic-games`; too short to carry a payload.
const ICON_TOKEN_MAX: usize = 32;

/// Whether `t` is a brand-icon token: `[a-z][a-z0-9-]{0,31}`.
///
/// Shape only — a host registry would reject tokens a newer client already draws.
/// The alphabet makes `../`, a URL, a `data:` payload and a NUL unrepresentable:
/// plugins control the field and clients interpolate it into names and paths.
pub fn is_icon_token(t: &str) -> bool {
    crate::slug::is_kebab(t, ICON_TOKEN_MAX, true)
}

/// Reject a malformed [`GameEntry::icon`] token. `Ok(())` when absent.
pub fn validate_icon(icon: Option<&str>) -> std::result::Result<(), String> {
    match icon {
        Some(t) if !is_icon_token(t) => Err(format!(
            "`icon` must be a brand token matching [a-z][a-z0-9-]{{0,{}}} (got {t:?})",
            ICON_TOKEN_MAX - 1
        )),
        _ => Ok(()),
    }
}

/// Per-entry window placement, applied once when the game's own window first
/// reaches the screen. Absent keys take the defaults below; `workspace` alone
/// falls back to the host's `launch_workspace` display policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OnWindow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::vdisplay::policy::WorkspacePlacement>,
    /// Raise it. Default on: the player launched it, so it is what they want
    /// in front — a launcher that steals focus back leaves them on the desk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<bool>,
    /// Make it full-screen. Default off: most games set their own mode, and
    /// forcing it fights a title that wanted a window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fullscreen: Option<bool>,
    /// Move it onto the streamed head if it opened elsewhere. Default on: a
    /// game the player cannot see is the whole failure this stage exists for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_to_stream_output: Option<bool>,
}

impl OnWindow {
    /// Nothing set: the key stays out of `library.json` and off the wire.
    pub fn is_empty(&self) -> bool {
        self.workspace.is_none()
            && self.focus.is_none()
            && self.fullscreen.is_none()
            && self.move_to_stream_output.is_none()
    }

    /// Raise the game's first window. Default on.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux places it"))]
    pub fn wants_focus(&self) -> bool {
        self.focus.unwrap_or(true)
    }

    /// Full-screen it. Default off — a title that wanted a window keeps one.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux places it"))]
    pub fn wants_fullscreen(&self) -> bool {
        self.fullscreen.unwrap_or(false)
    }

    /// Carry it onto the streamed head. Default on.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux places it"))]
    pub fn wants_stream_output(&self) -> bool {
        self.move_to_stream_output.unwrap_or(true)
    }

    /// Does this launch get a workspace of its own? The entry decides; absent,
    /// the host-wide default does. Backends that cannot place ignore both.
    pub fn own_workspace(&self) -> bool {
        use crate::vdisplay::policy::WorkspacePlacement;
        self.workspace
            .unwrap_or_else(|| crate::vdisplay::policy::prefs().launch_workspace())
            == WorkspacePlacement::Own
    }
}

/// One title in the unified library, regardless of store.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct GameEntry {
    /// Stable, store-qualified id: `steam:<appid>` or `custom:<id>`.
    #[schema(example = "steam:570")]
    pub id: String,
    /// Which store surfaced it: `"steam"` or `"custom"`.
    #[schema(example = "steam")]
    pub store: String,
    pub title: String,
    pub art: Artwork,
    #[serde(default, skip_serializing_if = "GameRole::is_game")]
    pub role: GameRole,
    /// Brand-mark token (`steam`, `heroic`) — never bytes or a URL. See [`is_icon_token`].
    ///
    /// The art proxy serves raster only ([`art::local_art_bytes`] refuses SVG as
    /// script-capable XML), so the mark stays a name the client already ships.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "steam")]
    pub icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch: Option<LaunchSpec>,
    /// Plugin that owns this entry. `None` only for operator-typed custom entries.
    /// `GET /library?provider=` filters on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Process match for a running title ([`DetectSpec`]). Never serialized: it names
    /// local paths, so it stays out of catalog JSON and the OpenAPI schema.
    #[serde(skip)]
    #[schema(ignore)]
    pub detect: DetectSpec,
    /// Which workspace this title's windows open on, where the compositor can
    /// place them ([`OnWindow`]).
    #[serde(default, skip_serializing_if = "OnWindow::is_empty")]
    pub on_window: OnWindow,
    /// Play stats, once this host has launched the title (`stats.rs`). Joined at read
    /// time from `library-stats.json`, never stored on the entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<EntryStats>,
    /// The stream settings this host sets for the title (`overrides.rs`), resolved: its own, a
    /// shared override's with that one's `name`, or its source's. Joined at read time.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<GameOverridesSchema>)]
    pub overrides: Option<punktfunk_core::game_overrides::GameOverrides>,
    /// Catalog ids a metadata source matches on (`steam`, `gog`, `libretro`, `sgdb` → value),
    /// set by the plugin that lists the entry. Not sent to paired clients.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub ids: BTreeMap<String, String>,
    /// Where each borrowed value came from: art slot or meta field → source id, or `pick`
    /// ([`PICK`]). Values the entry carried itself are absent. Not sent to paired clients.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub filled: BTreeMap<String, String>,
    /// Whether the title's files are on this host. Absent means they are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<downloads::Install>,
    /// One of the profile in view's own titles (`own:` id). Its seat host keeps how it starts;
    /// anywhere else it is a name and a picture, and starts nothing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    pub own: bool,
    #[serde(flatten)]
    pub meta: GameMeta,
}

/// A library entry plus the operator's view of it — whether they hid it.
///
/// A field on [`GameEntry`] would leak onto every lane: `GET /library` returns
/// `Vec<GameEntry>` except on the operator's lane, so a hidden flag cannot
/// reach a paired client. `flatten` keeps the wire a plain entry plus one key.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct OperatorGameEntry {
    #[serde(flatten)]
    pub entry: GameEntry,
    /// Set by [`set_entry_hidden`]. Omitted when false so the shape only grows for hidden titles.
    /// With a profile in view (`as`), the profile hid it: a title the box hides is not listed.
    #[serde(skip_serializing_if = "is_not_hidden")]
    pub hidden: bool,
    /// With a profile in view, the values on this row that are the profile's own.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(required = false)]
    pub profile: Option<ProfileValues>,
}

fn is_not_hidden(hidden: &bool) -> bool {
    !*hidden
}

/// Which [`Artwork`] field an art request names — the `<kind>` in
/// `GET /library/art/<id>/<kind>`, and the GameStream cover-proxy preference order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtKind {
    Portrait,
    Hero,
    Logo,
    Header,
}

impl ArtKind {
    pub const ALL: [Self; 4] = [Self::Portrait, Self::Hero, Self::Logo, Self::Header];

    /// The field name, as `parse` reads it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Portrait => "portrait",
            Self::Hero => "hero",
            Self::Logo => "logo",
            Self::Header => "header",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "portrait" => Some(Self::Portrait),
            "hero" => Some(Self::Hero),
            "logo" => Some(Self::Logo),
            "header" => Some(Self::Header),
            _ => None,
        }
    }
}

/// [`all_games_for`] this host's own view.
pub fn all_games() -> Vec<GameEntry> {
    all_games_for(&View::here())
}

/// Every *enabled* source's titles in `view`, sorted by title. Hidden entries are omitted.
///
/// Both gates run at read time and never mutate stored state. Source toggles
/// (`scanners.rs` / `library-scanners.json`) hide a source from this grid, native
/// clients, `/applist`, and launch resolution — the plugin may keep reconciling.
/// Per-entry hides (`hidden.rs`), the box's and the profile's, apply here so every surface
/// agrees; [`all_games_for_operator`] is the one exception. Operator-typed custom entries
/// carry no source and always contribute.
pub fn all_games_for(view: &View) -> Vec<GameEntry> {
    let (by_box, by_profile) = hidden_layers(view);
    sorted_games_for(view)
        .iter()
        .filter(|g| !by_box.contains(&g.id) && !by_profile.contains(&g.id))
        .cloned()
        .collect()
}

/// Title `id` as [`all_games_for`] lists it in `view`, with one entry cloned, not the list.
pub fn find_game_for(view: &View, id: &str) -> Option<GameEntry> {
    let game = sorted_games_for(view).iter().find(|g| g.id == id)?.clone();
    let (by_box, by_profile) = hidden_layers(view);
    (!by_box.contains(id) && !by_profile.contains(id)).then_some(game)
}

/// Whether `view`'s library lists title `id`, hidden or not.
pub(crate) fn lists(view: &View, id: &str) -> bool {
    sorted_games_for(view).iter().any(|g| g.id == id)
}

/// Whether `view`'s library lists a title of `source`, hidden or not.
pub(crate) fn lists_source(view: &View, source: &str) -> bool {
    let games = sorted_games_for(view);
    games.iter().any(|g| hidden::store_of(&g.id) == source)
}

/// The library in `view` including hidden titles, each flagged.
///
/// Only the console list on the operator lane calls this: a hidden entry has to
/// be visible somewhere or it could never be brought back. Paired clients,
/// the GameStream app list, and launch resolution use [`all_games`]. With a profile in view the
/// flag is the profile's, and a title the box hides is left out: only the box brings it back.
pub fn all_games_for_operator(view: &View) -> Vec<OperatorGameEntry> {
    let (by_box, by_profile) = hidden_layers(view);
    let own = OwnValues::load(view);
    sorted_games_for(view)
        .iter()
        .filter_map(|entry| operator_row(entry, view, &by_box, &by_profile, &own))
        .collect()
}

/// `entry` as the operator's list shows it in `view`; `None` for a title the box hides there.
pub(crate) fn operator_row(
    entry: &GameEntry,
    view: &View,
    by_box: &HashSet<String>,
    by_profile: &HashSet<String>,
    own: &OwnValues,
) -> Option<OperatorGameEntry> {
    let box_hides = by_box.contains(&entry.id);
    if box_hides && view.layer().is_some() {
        return None;
    }
    Some(OperatorGameEntry {
        hidden: box_hides || by_profile.contains(&entry.id),
        profile: own.of(&entry.id),
        entry: entry.clone(),
    })
}

/// A title's place in the list: its search fold (case, width, kana script), then id so equal
/// titles keep one order. Articles and accents stay: folding those is the clients' Title sort.
/// The paged route cuts its pages by this key, so the list has to be sorted by it.
pub(crate) fn sort_key(g: &GameEntry) -> (String, String) {
    (punktfunk_core::search::fold(&g.title), g.id.clone())
}

/// Counts this process's writes to the library files. Two saves inside one mtime tick can
/// leave the same stamp on different bytes; the count still moves.
static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What a built list was read from: the config dir, this process's write count, and the
/// mtime and length of every library file, which is what moves when someone else edits one.
type Inputs = (
    PathBuf,
    u64,
    Vec<(std::ffi::OsString, Option<SystemTime>, u64)>,
);

/// Every file the list is built from is named `library*` in the library dir, or sits in its
/// `library-metadata` folder or the view's own layer; a seat that reads the box's library adds
/// its own play stats and own titles. A file outside that rule would leave a stale list.
fn inputs(view: &View) -> Inputs {
    let dir = pf_paths::seat::library_dir();
    let own = pf_paths::config_dir();
    let mut folders = vec![(dir.clone(), "library"), (dir.join("library-metadata"), "")];
    if own != dir {
        folders.push((own, "library"));
    }
    if let Some(layer) = view.own_dir() {
        folders.push((layer, ""));
    }
    let mut stamps = Vec::new();
    for (folder, prefix) in folders {
        for e in std::fs::read_dir(folder).into_iter().flatten().flatten() {
            if !e.file_name().to_string_lossy().starts_with(prefix) {
                continue;
            }
            if let Some(md) = e.metadata().ok().filter(|md| md.is_file()) {
                stamps.push((e.path().into_os_string(), md.modified().ok(), md.len()));
            }
        }
    }
    stamps.sort();
    let writes = WRITES.load(std::sync::atomic::Ordering::Acquire);
    (dir, writes, stamps)
}

/// [`sorted_games_for`] this host's own view.
#[cfg(test)]
pub(crate) fn sorted_games() -> std::sync::Arc<Vec<GameEntry>> {
    sorted_games_for(&View::here())
}

/// Lists kept at once: this host's own view and one per profile, with room to spare.
const VIEWS_KEPT: usize = 16;

/// The whole library in `view`, hidden titles included, sorted by [`sort_key`]. Built once and
/// kept until an input moves: a page of a large library must not cost the whole of it.
pub(crate) fn sorted_games_for(view: &View) -> std::sync::Arc<Vec<GameEntry>> {
    type Built = std::sync::Mutex<Vec<(View, Inputs, std::sync::Arc<Vec<GameEntry>>)>>;
    static BUILT: Built = std::sync::Mutex::new(Vec::new());
    // Read before the build: a write that lands in between leaves this key behind the list,
    // which costs the next caller a rebuild and never serves an old list.
    let key = inputs(view);
    let kept = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((.., games)) = kept.iter().find(|(v, k, _)| v == view && *k == key) {
        return games.clone();
    }
    drop(kept);
    let games = std::sync::Arc::new(collect_games(view));
    let mut kept = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    kept.retain(|(v, ..)| v != view);
    if kept.len() >= VIEWS_KEPT {
        kept.remove(0);
    }
    kept.push((view.clone(), key, games.clone()));
    games
}

/// Merge every enabled source, the custom entries and the profile in view's own titles in `view`,
/// sorted by [`sort_key`], each carrying its play stats, its game overrides and what Art & Metadata
/// sources fill, plus the host's own [`GAME_MODE_ID`] tile. Split out so the two public views
/// differ only in how they apply the hidden set.
fn collect_games(view: &View) -> Vec<GameEntry> {
    let off = disabled_scanners(view);
    let stats = game_stats();
    let overrides = Overrides::load(view);
    let fills = Fills::load(view);
    // Manual entries always contribute; a provider's follow its toggle and [`source_off`]. The
    // profile's own titles are its account's: no rule for the box's rows applies to them.
    let mut games: Vec<GameEntry> = load_custom()
        .into_iter()
        .filter(|e| !source_off(e, &off, view.reach))
        .chain(own_entries(view))
        .map(GameEntry::from)
        .collect();
    for g in &mut games {
        g.stats = stats.get(&g.id).map(|t| t.entry(None));
        g.overrides = overrides.resolve(&g.id);
        fills.apply(g);
    }
    games.extend(game_mode_entry(
        crate::vdisplay::managed_session_available(),
        view.reach.contract,
    ));
    games.sort_by_cached_key(sort_key);
    games
}

/// The library id of the host's Game Mode tile: Steam's gamepad UI in the box's own gamescope
/// session. The handshake routes its connect to gamescope ([`game_mode_entry`]).
pub(crate) const GAME_MODE_ID: &str = "host:gamemode";

/// The [`GAME_MODE_ID`] launcher, on a box that runs a gamescope session (SteamOS,
/// `gamescope-session-plus`). Not on a contract seat (`contract_seat`): it has one virtual
/// screen and no Game Mode of its own; the owner's host at the desk keeps the tile. Built here,
/// never stored, so nothing edits it; a hide of it is ignored. Clients word the title themselves.
pub(crate) fn game_mode_entry(gamescope_session: bool, contract_seat: bool) -> Option<GameEntry> {
    (gamescope_session && !contract_seat).then(|| GameEntry {
        id: GAME_MODE_ID.into(),
        store: "host".into(),
        title: "Game Mode".into(),
        art: Artwork::default(),
        role: GameRole::Launcher,
        icon: Some("steam".into()),
        launch: Some(LaunchSpec {
            kind: "steam_ui".into(),
            value: "bigpicture".into(),
            args: None,
        }),
        provider: None,
        detect: DetectSpec::default(),
        on_window: OnWindow::default(),
        stats: None,
        // A launcher of the host's own: a game override would set a mode for Steam's UI.
        overrides: None,
        ids: BTreeMap::new(),
        filled: BTreeMap::new(),
        install: None,
        own: false,
        meta: GameMeta::default(),
    })
}

/// A reader's load: absent or malformed → the default. A bad file must cost its own contents,
/// not the library. A writer loads with [`read_json_for_write`] instead.
fn read_json_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
            tracing::warn!(file = %path.display(), error = %e, "library file malformed — ignored");
            T::default()
        }),
        Err(_) => T::default(),
    }
}

/// A writer's load: absent → the default, a file that exists but does not read or parse → an
/// error, so the save that follows never replaces what it could not read.
fn read_json_for_write<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(anyhow::anyhow!("read {}: {e}", path.display())),
    }
}

/// Owner-only ([`pf_paths::replace_secret_file`]), like hooks.json: `library.json` carries the
/// `prep`/`launch` commands the host runs.
fn save_json(path: &Path, json: &str) -> Result<()> {
    let saved = pf_paths::replace_secret_file(path, json.as_bytes());
    wrote();
    saved.with_context(|| format!("replace {}", path.display()))
}

fn wrote() {
    WRITES.fetch_add(1, std::sync::atomic::Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn out_of_range_meta_numbers_read_as_absent() {
        let m: GameMeta =
            serde_json::from_str(r#"{"release_year":2001.5,"players":300,"publisher":"A"}"#)
                .unwrap();
        assert_eq!((m.release_year, m.players), (None, None));
        assert_eq!(m.publisher.as_deref(), Some("A"));
        let m: GameMeta = serde_json::from_str(r#"{"release_year":2001,"players":4}"#).unwrap();
        assert_eq!((m.release_year, m.players), (Some(2001), Some(4)));
    }

    #[test]
    fn a_writer_load_refuses_a_file_it_cannot_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library-hidden.json");
        let absent: Result<BTreeMap<String, String>> = read_json_for_write(&path);
        assert!(absent.unwrap().is_empty(), "no file is the default");
        std::fs::write(&path, b"{ not json").unwrap();
        let bad: Result<BTreeMap<String, String>> = read_json_for_write(&path);
        assert!(bad.unwrap_err().to_string().starts_with("parse "));
        let lenient: BTreeMap<String, String> = read_json_or_default(&path);
        assert!(lenient.is_empty(), "a reader still falls back");
        assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
        std::fs::write(&path, br#"{"a":"b"}"#).unwrap();
        let ok: BTreeMap<String, String> = read_json_for_write(&path).unwrap();
        assert_eq!(ok["a"], "b");
    }

    fn entry(id: &str, title: &str) -> GameEntry {
        GameEntry {
            id: id.into(),
            store: id.split_once(':').map_or("custom", |(s, _)| s).into(),
            title: title.into(),
            art: Artwork::default(),
            role: GameRole::default(),
            icon: None,
            launch: None,
            provider: None,
            detect: DetectSpec::default(),
            on_window: OnWindow::default(),
            stats: None,
            overrides: None,
            ids: BTreeMap::new(),
            filled: BTreeMap::new(),
            install: None,
            own: false,
            meta: GameMeta::default(),
        }
    }

    #[test]
    fn host_order_reads_kana_by_sound_and_width_variants_as_their_normal_form() {
        let mut games: Vec<GameEntry> = [
            "モンスターハンター",
            "ｾﾞﾙﾀﾞ無双",
            "ぷよぷよテトリス",
            "Ｆｉｎａｌ",
            "The Witcher",
            "ゼルダの伝説",
            "final",
            "原神",
            "あつまれ どうぶつの森",
        ]
        .iter()
        .enumerate()
        .map(|(i, t)| entry(&format!("custom:{i}"), t))
        .collect();
        games.sort_by_cached_key(sort_key);
        let titles: Vec<&str> = games.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Ｆｉｎａｌ",
                "final",
                "The Witcher",
                "あつまれ どうぶつの森",
                "ゼルダの伝説",
                "ｾﾞﾙﾀﾞ無双",
                "ぷよぷよテトリス",
                "モンスターハンター",
                "原神",
            ]
        );
    }

    /// Pin the operator-view wire: a normal entry plus one extra key, omitted when visible.
    #[test]
    fn operator_entry_flattens_and_omits_hidden_when_false() {
        let visible = OperatorGameEntry {
            entry: entry("steam:70", "Half-Life"),
            hidden: false,
            profile: None,
        };
        let v = serde_json::to_value(&visible).expect("serializes");
        assert_eq!(v["id"], "steam:70", "the entry's fields stay at top level");
        assert_eq!(v["title"], "Half-Life");
        assert!(
            v.get("hidden").is_none(),
            "a visible entry must not carry the key at all: {v}"
        );

        let hidden = OperatorGameEntry {
            entry: entry("steam:70", "Half-Life"),
            hidden: true,
            profile: None,
        };
        let v = serde_json::to_value(&hidden).expect("serializes");
        assert_eq!(v["hidden"], true);
        assert_eq!(v["id"], "steam:70", "flatten still applies when hidden");
    }

    /// The Game Mode tile is a Steam Big Picture launcher, on a box with a gamescope session
    /// and a host that is no contract seat.
    #[test]
    fn the_game_mode_tile_needs_a_gamescope_session_and_no_contract_seat() {
        assert!(game_mode_entry(false, false).is_none());
        assert!(game_mode_entry(true, true).is_none());
        let g = game_mode_entry(true, false).expect("offered");
        assert_eq!((g.id.as_str(), g.role), (GAME_MODE_ID, GameRole::Launcher));
        let l = g.launch.expect("it launches");
        assert_eq!(
            (l.kind.as_str(), l.value.as_str()),
            ("steam_ui", "bigpicture")
        );
    }

    /// Both views share `collect_games`; they must agree on which ids exist and
    /// differ only in visibility.
    #[test]
    fn hidden_filter_is_the_only_difference_between_the_two_views() {
        let games = vec![
            entry("steam:70", "Half-Life"),
            entry("lutris:4", "Syndicate"),
            entry("custom:abc", "Chrono Trigger"),
        ];
        let hidden: HashSet<String> = ["lutris:4".to_string()].into_iter().collect();

        let operator: Vec<OperatorGameEntry> = games
            .iter()
            .cloned()
            .map(|entry| OperatorGameEntry {
                hidden: hidden.contains(&entry.id),
                entry,
                profile: None,
            })
            .collect();
        let played: Vec<GameEntry> = games
            .into_iter()
            .filter(|g| !hidden.contains(&g.id))
            .collect();

        assert_eq!(operator.len(), 3, "the operator sees every title");
        assert_eq!(played.len(), 2, "a player does not see the hidden one");
        assert!(
            !played.iter().any(|g| g.id == "lutris:4"),
            "the hidden id must be absent, not merely flagged"
        );
        assert_eq!(
            operator.iter().filter(|r| r.hidden).count(),
            1,
            "exactly the hidden one is flagged"
        );
    }
}
