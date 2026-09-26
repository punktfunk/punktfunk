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
mod hidden;
mod launch;
mod metadata;
mod scanners;
mod stats;

pub use art::*;
pub use custom::*;
pub use detect::*;
pub use hidden::*;
pub use launch::*;
pub use metadata::*;
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
    !t.is_empty()
        && t.len() <= ICON_TOKEN_MAX
        && t.starts_with(|c: char| c.is_ascii_lowercase())
        && t.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
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
    pub fn wants_focus(&self) -> bool {
        self.focus.unwrap_or(true)
    }

    /// Full-screen it. Default off — a title that wanted a window keeps one.
    pub fn wants_fullscreen(&self) -> bool {
        self.fullscreen.unwrap_or(false)
    }

    /// Carry it onto the streamed head. Default on.
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
    pub stats: Option<GameStats>,
    /// Catalog ids a metadata source matches on (`steam`, `gog`, `libretro`, `sgdb` → value),
    /// set by the plugin that lists the entry. Not sent to paired clients.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub ids: BTreeMap<String, String>,
    /// Where each borrowed value came from: art slot or meta field → source id, or `pick`
    /// ([`PICK`]). Values the entry carried itself are absent. Not sent to paired clients.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub filled: BTreeMap<String, String>,
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
    #[serde(skip_serializing_if = "is_not_hidden")]
    pub hidden: bool,
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

/// Every *enabled* source's titles, sorted by title. Hidden entries are omitted.
///
/// Both gates run at read time and never mutate stored state. Source toggles
/// (`scanners.rs` / `library-scanners.json`) hide a source from this grid, native
/// clients, `/applist`, and launch resolution — the plugin may keep reconciling.
/// Per-entry hides (`hidden.rs`) apply here so every surface agrees;
/// [`all_games_for_operator`] is the one exception. Operator-typed custom entries
/// carry no source and always contribute.
pub fn all_games() -> Vec<GameEntry> {
    let hidden = hidden_ids();
    let mut games = collect_games();
    games.retain(|g| !hidden.contains(&g.id));
    games
}

/// The library including hidden titles, each flagged.
///
/// Only the console list on the operator lane calls this: a hidden entry has to
/// be visible somewhere or it could never be brought back. Paired clients,
/// the GameStream app list, and launch resolution use [`all_games`].
pub fn all_games_for_operator() -> Vec<OperatorGameEntry> {
    let hidden = hidden_ids();
    collect_games()
        .into_iter()
        .map(|entry| OperatorGameEntry {
            hidden: hidden.contains(&entry.id),
            entry,
        })
        .collect()
}

/// Merge every enabled source and the custom entries, sorted by title, each
/// carrying its play stats and what Art & Metadata sources fill. Split out so the
/// two public views differ only in how they apply the hidden set.
fn collect_games() -> Vec<GameEntry> {
    let off = disabled_scanners();
    let stats = game_stats();
    let fills = Fills::load();
    // Manual entries always contribute; a provider's follow the operator's source toggle.
    let mut games: Vec<GameEntry> = load_custom()
        .into_iter()
        .filter(|e| !source_id_for(e).is_some_and(|src| off.contains(src)))
        .map(GameEntry::from)
        .collect();
    for g in &mut games {
        g.stats = stats.get(&g.id).copied();
        fills.apply(g);
    }
    games.sort_by_key(|g| g.title.to_lowercase());
    games
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
            ids: BTreeMap::new(),
            filled: BTreeMap::new(),
            meta: GameMeta::default(),
        }
    }

    /// Pin the operator-view wire: a normal entry plus one extra key, omitted when visible.
    #[test]
    fn operator_entry_flattens_and_omits_hidden_when_false() {
        let visible = OperatorGameEntry {
            entry: entry("steam:70", "Half-Life"),
            hidden: false,
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
        };
        let v = serde_json::to_value(&hidden).expect("serializes");
        assert_eq!(v["hidden"], true);
        assert_eq!(v["id"], "steam:70", "flatten still applies when hidden");
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
