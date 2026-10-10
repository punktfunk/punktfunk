//! Art & Metadata sources: plugins that fill art and [`GameMeta`] for entries other plugins
//! list. Design: planning `design/metadata-sources.md`.
//!
//! A source pushes its whole result (`PUT /library/metadata/{source}`). The host keeps one side
//! table per source in `library-metadata/` and merges it at read time. Provider rows are rebuilt
//! on every reconcile, so nothing is written onto them (the `hidden.rs` rule).
//!
//! Per art slot: the operator's pick, then sources set to replace, then the entry's own value,
//! then the other sources in order. Per meta field: the entry's own value, then sources in
//! order. A source never touches id, title, launch or detect, and a launcher tile takes nothing
//! from one. [`GameEntry::filled`] names where each borrowed value came from.

use super::*;
use std::sync::{Arc, Mutex, MutexGuard};

/// Longest URL a source or a pick may store.
const URL_MAX: usize = 2048;
/// Longest short text field: platform, developer, publisher, region.
const TEXT_MAX: usize = 256;
/// Bounds one entry's `description`, not the payload.
const DESCRIPTION_MAX: usize = 16 * 1024;
/// Most items in `genres` or `tags`, and the longest item.
const LIST_MAX: usize = 32;
const LIST_ITEM_MAX: usize = 64;
/// A library id is `<store>:<external_id>`; the external part is a file path at worst.
const ENTRY_ID_MAX: usize = 1024;
/// Most pairs in [`GameEntry::ids`], and the longest value.
const IDS_MAX: usize = 8;
const ID_VALUE_MAX: usize = 256;

/// [`GameEntry::filled`] value for the operator's own pick.
pub const PICK: &str = "pick";

/// How a source finds its games. A new source lands after the last one of its kind, exact first.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Matching {
    /// Only by [`GameEntry::ids`]; never guesses.
    Exact,
    /// Falls back to a title search.
    #[default]
    Search,
}

/// What one source says about one entry.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct MetadataFill {
    #[serde(default)]
    pub art: Artwork,
    #[serde(default)]
    pub meta: GameMeta,
}

/// One entry in a source's push.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct MetadataEntryInput {
    /// Library id, as `GET /library` lists it (`steam:570`, `custom:3f9a0c1b2d4e`).
    pub id: String,
    /// `http(s)` URLs only; the host fetches and keeps them like a provider's CDN art.
    #[serde(default)]
    pub art: Artwork,
    #[serde(default)]
    pub meta: GameMeta,
}

/// `PUT /library/metadata/{source}`: the source's whole result.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct MetadataInput {
    #[serde(default)]
    pub matching: Matching,
    #[serde(default)]
    pub entries: Vec<MetadataEntryInput>,
}

/// One Art & Metadata source as the console lists it, in the operator's order.
#[derive(Clone, Debug, Serialize, ToSchema)]
pub struct MetadataSourceInfo {
    /// The source's plugin id.
    #[schema(example = "steamgriddb")]
    pub id: String,
    pub matching: Matching,
    pub enabled: bool,
    /// "Use for every game": this source's art beats the entry's own. Art only.
    pub replace: bool,
    /// Entries the source has something for.
    pub entries: usize,
}

/// One row of `PUT /library/metadata`. The array's order is the new order.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct MetadataSourceUpdate {
    pub id: String,
    pub enabled: bool,
    pub replace: bool,
}

/// `PUT /library/picks/{id}`.
#[derive(Clone, Debug, Deserialize, ToSchema)]
pub struct ArtPickInput {
    /// `portrait`, `hero`, `logo` or `header`.
    #[schema(example = "portrait")]
    pub kind: String,
    /// An `http(s)` URL; `null` clears the pick.
    #[serde(default)]
    pub url: Option<String>,
}

/// `library-metadata/<source>.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Overlay {
    #[serde(default)]
    matching: Matching,
    #[serde(default)]
    entries: BTreeMap<String, MetadataFill>,
}

/// One row of `library-metadata.json`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SourceSetting {
    id: String,
    #[serde(default)]
    matching: Matching,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    #[serde(default)]
    replace: bool,
}

fn enabled_by_default() -> bool {
    true
}

/// `library-metadata.json`: every source, in the operator's order.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Settings {
    #[serde(default)]
    sources: Vec<SourceSetting>,
}

/// `library-picks.json`: library id → the art the operator chose.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Picks {
    #[serde(default)]
    picks: BTreeMap<String, Artwork>,
}

fn settings_path() -> PathBuf {
    pf_paths::seat::library_dir().join("library-metadata.json")
}

fn picks_path() -> PathBuf {
    pf_paths::seat::library_dir().join("library-picks.json")
}

/// The source id passed [`validate_provider_name`], so it is a safe file name.
fn overlay_path(source: &str) -> PathBuf {
    pf_paths::seat::library_dir()
        .join("library-metadata")
        .join(format!("{source}.json"))
}

/// Held across every load-modify-save in this module.
fn lock() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

type Stamp = (Option<SystemTime>, u64);

/// Parsed overlays by file, with the mtime and length they were read at.
static OVERLAYS: Mutex<BTreeMap<PathBuf, (Stamp, Arc<Overlay>)>> = Mutex::new(BTreeMap::new());

/// An overlay file's parse, re-read when its mtime or length moves. Every art request merges
/// its entry, so parsing megabytes per cover would dominate a grid load. Writers go through
/// [`save_overlay`], which drops the entry.
fn load_overlay(path: &Path) -> Arc<Overlay> {
    let Ok(md) = std::fs::metadata(path) else {
        return Arc::default();
    };
    let stamp = (md.modified().ok(), md.len());
    if let Some((s, o)) = OVERLAYS.lock().unwrap_or_else(|e| e.into_inner()).get(path) {
        if *s == stamp {
            return o.clone();
        }
    }
    let overlay = Arc::new(read_json_or_default::<Overlay>(path));
    OVERLAYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf(), (stamp, overlay.clone()));
    overlay
}

fn save_overlay(path: &Path, json: &str) -> Result<()> {
    save_json(path, json)?;
    // Two saves inside one mtime tick can leave the same stamp on different bytes.
    OVERLAYS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
    Ok(())
}

fn emit_changed(source: &str) {
    crate::events::emit(crate::events::EventKind::LibraryChanged {
        source: source.to_string(),
    });
}

/// An `http(s)` URL a source or pick may store. Local paths and `data:` stay provider-only.
pub(crate) fn valid_remote_url(v: &str) -> bool {
    (v.starts_with("https://") || v.starts_with("http://"))
        && v.len() <= URL_MAX
        && !v.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn valid_entry_id(id: &str) -> bool {
    id.len() <= ENTRY_ID_MAX
        && id
            .split_once(':')
            .is_some_and(|(s, x)| !s.is_empty() && !x.is_empty())
}

/// Drop the pairs a source could not match on: keys `[a-z0-9_]{1,16}`, values ≤ 256 chars
/// with no control characters, at most eight. Returns how many went.
pub fn sanitize_ids(ids: &mut BTreeMap<String, String>) -> usize {
    let before = ids.len();
    ids.retain(|k, v| {
        (1..=16).contains(&k.len())
            && k.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            && !v.is_empty()
            && v.len() <= ID_VALUE_MAX
            && !v.chars().any(char::is_control)
    });
    while ids.len() > IDS_MAX {
        ids.pop_last();
    }
    before - ids.len()
}

/// Drop every value the host would not store. Returns how many went.
fn sanitize_fill(f: &mut MetadataFill) -> usize {
    let mut dropped = 0;
    for kind in ArtKind::ALL {
        let slot = art_slot(&mut f.art, kind);
        if slot.as_deref().is_some_and(|v| !valid_remote_url(v)) {
            *slot = None;
            dropped += 1;
        }
    }
    let m = &mut f.meta;
    for (field, max) in [
        (&mut m.platform, TEXT_MAX),
        (&mut m.description, DESCRIPTION_MAX),
        (&mut m.developer, TEXT_MAX),
        (&mut m.publisher, TEXT_MAX),
        (&mut m.region, TEXT_MAX),
    ] {
        if field
            .as_deref()
            .is_some_and(|v| v.trim().is_empty() || v.len() > max)
        {
            *field = None;
            dropped += 1;
        }
    }
    for list in [&mut m.genres, &mut m.tags] {
        let before = list.len();
        list.retain(|v| !v.trim().is_empty() && v.len() <= LIST_ITEM_MAX);
        list.truncate(LIST_MAX);
        dropped += before - list.len();
    }
    dropped
}

fn fill_is_empty(f: &MetadataFill) -> bool {
    let m = &f.meta;
    ArtKind::ALL.iter().all(|k| art_field(&f.art, *k).is_none())
        && m.platform.is_none()
        && m.description.is_none()
        && m.developer.is_none()
        && m.publisher.is_none()
        && m.release_year.is_none()
        && m.genres.is_empty()
        && m.tags.is_empty()
        && m.region.is_none()
        && m.players.is_none()
}

/// Insert `source` where a new one of its kind belongs, or update its declared matching.
/// True when the settings changed.
fn place_source(sources: &mut Vec<SourceSetting>, source: &str, matching: Matching) -> bool {
    if let Some(s) = sources.iter_mut().find(|s| s.id == source) {
        let changed = s.matching != matching;
        s.matching = matching;
        return changed;
    }
    let rank = |m: Matching| matches!(m, Matching::Search) as u8;
    let pos = sources
        .iter()
        .rposition(|s| rank(s.matching) <= rank(matching))
        .map_or(0, |i| i + 1);
    sources.insert(
        pos,
        SourceSetting {
            id: source.to_string(),
            matching,
            enabled: true,
            replace: false,
        },
    );
    true
}

/// Replace `source`'s result. Returns (entries kept, values dropped). The caller validated the
/// id. Emits `library.changed` with the source when anything changed.
pub fn put_metadata(source: &str, input: MetadataInput) -> Result<(usize, usize)> {
    let mut dropped = 0;
    let mut entries = BTreeMap::new();
    for e in input.entries {
        if !valid_entry_id(&e.id) {
            dropped += 1;
            continue;
        }
        let mut fill = MetadataFill {
            art: e.art,
            meta: e.meta,
        };
        dropped += sanitize_fill(&mut fill);
        if !fill_is_empty(&fill) {
            entries.insert(e.id, fill);
        }
    }
    let kept = entries.len();
    let overlay = Overlay {
        matching: input.matching,
        entries,
    };
    let json = serde_json::to_string_pretty(&overlay)?;
    let _serial = lock();
    let path = overlay_path(source);
    let changed = std::fs::read_to_string(&path).map_or(true, |old| old != json);
    if changed {
        save_overlay(&path, &json)?;
    }
    let mut settings: Settings = read_json_or_default(&settings_path());
    let placed = place_source(&mut settings.sources, source, input.matching);
    if placed {
        save_json(&settings_path(), &serde_json::to_string_pretty(&settings)?)?;
    }
    if changed || placed {
        emit_changed(source);
    }
    Ok((kept, dropped))
}

/// Forget `source`: its result and its settings row. False when there was nothing to forget.
pub fn delete_metadata(source: &str) -> Result<bool> {
    let _serial = lock();
    let path = overlay_path(source);
    let had_file = path.exists();
    if had_file {
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    }
    let mut settings: Settings = read_json_or_default(&settings_path());
    let before = settings.sources.len();
    settings.sources.retain(|s| s.id != source);
    let had_row = settings.sources.len() != before;
    if had_row {
        save_json(&settings_path(), &serde_json::to_string_pretty(&settings)?)?;
    }
    if had_file || had_row {
        emit_changed(source);
    }
    Ok(had_file || had_row)
}

/// Every source, in the operator's order.
pub fn list_metadata_sources() -> Vec<MetadataSourceInfo> {
    read_json_or_default::<Settings>(&settings_path())
        .sources
        .into_iter()
        .map(|s| MetadataSourceInfo {
            entries: load_overlay(&overlay_path(&s.id)).entries.len(),
            id: s.id,
            matching: s.matching,
            enabled: s.enabled,
            replace: s.replace,
        })
        .collect()
}

/// Apply the console's list: its order first, then any source it did not name, as they were.
/// Unknown ids are ignored.
pub fn set_metadata_sources(updates: &[MetadataSourceUpdate]) -> Result<Vec<MetadataSourceInfo>> {
    {
        let _serial = lock();
        let mut settings: Settings = read_json_or_default(&settings_path());
        let mut next = Vec::with_capacity(settings.sources.len());
        for u in updates {
            if let Some(pos) = settings.sources.iter().position(|s| s.id == u.id) {
                let mut s = settings.sources.remove(pos);
                s.enabled = u.enabled;
                s.replace = u.replace;
                next.push(s);
            }
        }
        next.append(&mut settings.sources);
        settings.sources = next;
        save_json(&settings_path(), &serde_json::to_string_pretty(&settings)?)?;
    }
    emit_changed("manual");
    Ok(list_metadata_sources())
}

/// Set or clear the operator's art for one slot of `library_id`. The caller checked the URL.
/// Returns the entry's picks after the change.
pub fn set_art_pick(library_id: &str, kind: ArtKind, url: Option<String>) -> Result<Artwork> {
    let picked = {
        let _serial = lock();
        let mut picks: Picks = read_json_or_default(&picks_path());
        let art = picks.picks.entry(library_id.to_string()).or_default();
        *art_slot(art, kind) = url;
        let picked = art.clone();
        if ArtKind::ALL
            .iter()
            .all(|k| art_field(&picked, *k).is_none())
        {
            picks.picks.remove(library_id);
        }
        save_json(&picks_path(), &serde_json::to_string_pretty(&picks)?)?;
        picked
    };
    emit_changed("manual");
    Ok(picked)
}

/// Every enabled source's result and the operator's picks, read once per catalog read.
pub(crate) struct Fills {
    sources: Vec<(SourceSetting, Arc<Overlay>)>,
    picks: BTreeMap<String, Artwork>,
}

impl Fills {
    pub(crate) fn load() -> Self {
        let sources = read_json_or_default::<Settings>(&settings_path())
            .sources
            .into_iter()
            .filter(|s| s.enabled)
            .map(|s| {
                let overlay = load_overlay(&overlay_path(&s.id));
                (s, overlay)
            })
            .collect();
        Self {
            sources,
            picks: read_json_or_default::<Picks>(&picks_path()).picks,
        }
    }

    pub(crate) fn apply(&self, g: &mut GameEntry) {
        let found: Vec<Contribution> = self
            .sources
            .iter()
            .filter_map(|(s, o)| {
                o.entries.get(&g.id).map(|fill| Contribution {
                    source: &s.id,
                    replace: s.replace,
                    fill,
                })
            })
            .collect();
        fill_entry(g, self.picks.get(&g.id), &found);
    }
}

/// One enabled source's fill for the entry being merged, in order.
struct Contribution<'a> {
    source: &'a str,
    replace: bool,
    fill: &'a MetadataFill,
}

fn present(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| !s.trim().is_empty())
}

/// Merge picks and source fills into `g` (precedence in the module docs), recording each
/// borrowed value in `g.filled`.
fn fill_entry(g: &mut GameEntry, pick: Option<&Artwork>, found: &[Contribution]) {
    let launcher = g.role == GameRole::Launcher;
    for kind in ArtKind::ALL {
        let from = |replace: bool| {
            found
                .iter()
                .filter(|c| c.replace == replace)
                .find_map(|c| art_field(&c.fill.art, kind).map(|v| (c.source, v)))
        };
        let own = art_field(&g.art, kind).filter(|v| !v.trim().is_empty());
        let borrowed = match pick.and_then(|p| art_field(p, kind)) {
            Some(v) => Some((PICK, v)),
            None if launcher => None,
            None => from(true).or_else(|| own.is_none().then(|| from(false)).flatten()),
        };
        if let Some((source, v)) = borrowed {
            *art_slot(&mut g.art, kind) = Some(v);
            g.filled.insert(kind.name().to_string(), source.to_string());
        }
    }
    if launcher {
        return;
    }
    let (m, filled) = (&mut g.meta, &mut g.filled);
    let mut text = |own: &mut Option<String>, name: &str, get: fn(&GameMeta) -> &Option<String>| {
        if present(own) {
            return;
        }
        if let Some(c) = found.iter().find(|c| present(get(&c.fill.meta))) {
            *own = get(&c.fill.meta).clone();
            filled.insert(name.to_string(), c.source.to_string());
        }
    };
    text(&mut m.platform, "platform", |m| &m.platform);
    text(&mut m.description, "description", |m| &m.description);
    text(&mut m.developer, "developer", |m| &m.developer);
    text(&mut m.publisher, "publisher", |m| &m.publisher);
    text(&mut m.region, "region", |m| &m.region);
    let list = |own: &mut Vec<String>, name: &str, get: fn(&GameMeta) -> &Vec<String>| {
        if own.is_empty() {
            if let Some(c) = found.iter().find(|c| !get(&c.fill.meta).is_empty()) {
                *own = get(&c.fill.meta).clone();
                return Some((name.to_string(), c.source.to_string()));
            }
        }
        None
    };
    let genres = list(&mut m.genres, "genres", |m| &m.genres);
    let tags = list(&mut m.tags, "tags", |m| &m.tags);
    if m.release_year.is_none() {
        if let Some(c) = found.iter().find(|c| c.fill.meta.release_year.is_some()) {
            m.release_year = c.fill.meta.release_year;
            filled.insert("release_year".into(), c.source.to_string());
        }
    }
    if m.players.is_none() {
        if let Some(c) = found.iter().find(|c| c.fill.meta.players.is_some()) {
            m.players = c.fill.meta.players;
            filled.insert("players".into(), c.source.to_string());
        }
    }
    filled.extend(genres.into_iter().chain(tags));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> GameEntry {
        GameEntry {
            id: id.into(),
            store: "steam".into(),
            title: "Hades".into(),
            art: Artwork::default(),
            role: GameRole::Game,
            icon: None,
            launch: None,
            provider: None,
            detect: DetectSpec::default(),
            on_window: OnWindow::default(),
            stats: None,
            ids: BTreeMap::new(),
            filled: BTreeMap::new(),
            install: None,
            meta: GameMeta::default(),
        }
    }

    fn fill(portrait: Option<&str>, logo: Option<&str>, developer: Option<&str>) -> MetadataFill {
        MetadataFill {
            art: Artwork {
                portrait: portrait.map(str::to_string),
                logo: logo.map(str::to_string),
                ..Artwork::default()
            },
            meta: GameMeta {
                developer: developer.map(str::to_string),
                ..GameMeta::default()
            },
        }
    }

    /// Own art stays; a source fills only the gaps, and `filled` names it.
    #[test]
    fn a_source_fills_gaps_only() {
        let mut g = entry("steam:1145360");
        g.art.portrait = Some("own.jpg".into());
        let f = fill(
            Some("https://s/p.png"),
            Some("https://s/l.png"),
            Some("Supergiant"),
        );
        fill_entry(
            &mut g,
            None,
            &[Contribution {
                source: "sgdb",
                replace: false,
                fill: &f,
            }],
        );
        assert_eq!(g.art.portrait.as_deref(), Some("own.jpg"));
        assert_eq!(g.art.logo.as_deref(), Some("https://s/l.png"));
        assert_eq!(g.meta.developer.as_deref(), Some("Supergiant"));
        assert_eq!(g.filled.get("logo").map(String::as_str), Some("sgdb"));
        assert_eq!(g.filled.get("developer").map(String::as_str), Some("sgdb"));
        assert!(
            !g.filled.contains_key("portrait"),
            "own values are not borrowed"
        );
    }

    /// Pick → replace source → own → fill source, per slot; the first source in order wins.
    #[test]
    fn precedence_runs_pick_replace_own_fill() {
        let a = fill(Some("https://a/p.png"), Some("https://a/l.png"), Some("A"));
        let b = fill(Some("https://b/p.png"), Some("https://b/l.png"), Some("B"));
        let found = [
            Contribution {
                source: "a",
                replace: false,
                fill: &a,
            },
            Contribution {
                source: "b",
                replace: true,
                fill: &b,
            },
        ];
        let mut g = entry("steam:1");
        g.art.portrait = Some("own.jpg".into());
        g.art.logo = Some("own-logo.png".into());
        let pick = Artwork {
            logo: Some("https://pick/l.png".into()),
            ..Artwork::default()
        };
        fill_entry(&mut g, Some(&pick), &found);
        assert_eq!(g.art.logo.as_deref(), Some("https://pick/l.png"));
        assert_eq!(g.filled["logo"], PICK);
        assert_eq!(
            g.art.portrait.as_deref(),
            Some("https://b/p.png"),
            "replace beats own"
        );
        assert_eq!(g.filled["portrait"], "b");
        assert_eq!(
            g.meta.developer.as_deref(),
            Some("A"),
            "replace is art only; order decides meta"
        );
    }

    #[test]
    fn a_launcher_takes_picks_but_no_source_fills() {
        let mut g = entry("steam:ui");
        g.role = GameRole::Launcher;
        let f = fill(Some("https://s/p.png"), None, Some("Valve"));
        let pick = Artwork {
            hero: Some("https://pick/h.png".into()),
            ..Artwork::default()
        };
        fill_entry(
            &mut g,
            Some(&pick),
            &[Contribution {
                source: "s",
                replace: true,
                fill: &f,
            }],
        );
        assert!(g.art.portrait.is_none());
        assert!(g.meta.developer.is_none());
        assert_eq!(g.art.hero.as_deref(), Some("https://pick/h.png"));
    }

    #[test]
    fn empty_own_values_count_as_gaps() {
        let mut g = entry("steam:2");
        g.art.portrait = Some("  ".into());
        g.meta.developer = Some(String::new());
        let f = fill(Some("https://s/p.png"), None, Some("Dev"));
        fill_entry(
            &mut g,
            None,
            &[Contribution {
                source: "s",
                replace: false,
                fill: &f,
            }],
        );
        assert_eq!(g.art.portrait.as_deref(), Some("https://s/p.png"));
        assert_eq!(g.meta.developer.as_deref(), Some("Dev"));
    }

    #[test]
    fn sanitize_drops_what_the_host_would_not_store() {
        let mut f = MetadataFill {
            art: Artwork {
                portrait: Some("file:///etc/passwd".into()),
                hero: Some("data:image/png;base64,AAAA".into()),
                logo: Some("https://ok/l.png".into()),
                header: Some("https://has space/x.png".into()),
            },
            meta: GameMeta {
                developer: Some("x".repeat(TEXT_MAX + 1)),
                publisher: Some("Pub".into()),
                genres: vec![
                    "Action".into(),
                    String::new(),
                    "y".repeat(LIST_ITEM_MAX + 1),
                ],
                ..GameMeta::default()
            },
        };
        assert_eq!(sanitize_fill(&mut f), 6);
        assert_eq!(f.art.logo.as_deref(), Some("https://ok/l.png"));
        assert!(f.art.portrait.is_none() && f.art.hero.is_none() && f.art.header.is_none());
        assert!(f.meta.developer.is_none());
        assert_eq!(f.meta.genres, vec!["Action".to_string()]);
    }

    #[test]
    fn ids_keep_only_matchable_pairs() {
        let mut ids: BTreeMap<String, String> = [
            ("steam", "570"),
            ("libretro", "Nintendo - SNES/Zelda (USA)"),
            ("Bad-Key", "x"),
            ("empty", ""),
            ("ctl", "a\nb"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(sanitize_ids(&mut ids), 3);
        assert_eq!(ids.len(), 2);
        let mut many: BTreeMap<String, String> = (0..12)
            .map(|i| (format!("k{i:02}"), "v".to_string()))
            .collect();
        sanitize_ids(&mut many);
        assert_eq!(many.len(), IDS_MAX);
    }

    /// A new source lands after the last of its kind; exact sources rank first.
    #[test]
    fn new_sources_land_exact_first() {
        let mut s = Vec::new();
        assert!(place_source(&mut s, "steamgriddb", Matching::Search));
        assert!(place_source(&mut s, "libretro", Matching::Exact));
        assert!(place_source(&mut s, "igdb", Matching::Search));
        let ids: Vec<_> = s.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["libretro", "steamgriddb", "igdb"]);
        assert!(
            !place_source(&mut s, "libretro", Matching::Exact),
            "a re-push is not a move"
        );
    }

    #[test]
    fn entry_ids_must_name_a_store() {
        assert!(valid_entry_id("steam:570"));
        assert!(valid_entry_id("heroic:legendary:fc0b"));
        assert!(!valid_entry_id("steam:"));
        assert!(!valid_entry_id("nocolon"));
        assert!(!valid_entry_id(&format!("s:{}", "x".repeat(ENTRY_ID_MAX))));
    }

    /// The console and the plugin kit reject early by these same cases.
    #[test]
    fn shared_id_and_url_vectors_hold() {
        let raw = include_str!("../../../../../clients/shared/library-id-vectors.json");
        let v: serde_json::Value = serde_json::from_str(raw).expect("vectors parse");
        let cases = |key: &str| {
            v[key].as_array().expect("case list").iter().map(|c| {
                let fill = c["fill"].as_str().unwrap_or("");
                let count = c["count"].as_u64().unwrap_or(0) as usize;
                let value = format!("{}{}", c["value"].as_str().unwrap(), fill.repeat(count));
                (value, c["valid"].as_bool().unwrap())
            })
        };
        for (id, valid) in cases("entry_ids") {
            assert_eq!(valid_entry_id(&id), valid, "entry id {id:?}");
        }
        for (url, valid) in cases("urls") {
            assert_eq!(valid_remote_url(&url), valid, "url {url:?}");
        }
    }

    /// A rewrite of the same length inside one mtime tick keeps the stamp; the save drops the
    /// cached parse, so readers still see the new bytes.
    #[test]
    fn a_same_stamp_rewrite_reads_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stamp.json");
        let doc =
            |url: &str| format!(r#"{{"entries":{{"steam:1":{{"art":{{"portrait":"{url}"}}}}}}}}"#);
        let read = || load_overlay(&path).entries["steam:1"].art.portrait.clone();
        save_overlay(&path, &doc("https://a/1.png")).unwrap();
        let tick = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(read().as_deref(), Some("https://a/1.png"));
        save_overlay(&path, &doc("https://a/2.png")).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(tick).unwrap();
        drop(file);
        assert_eq!(read().as_deref(), Some("https://a/2.png"));
    }

    /// The persisted shapes are what an operator may hand-edit — pin them.
    #[test]
    fn settings_and_picks_parse_their_documented_shape() {
        let s: Settings = serde_json::from_str(
            r#"{"sources":[{"id":"libretro","matching":"exact"},{"id":"steamgriddb","replace":true,"enabled":false}]}"#,
        )
        .expect("parses");
        assert!(s.sources[0].enabled, "enabled defaults on");
        assert_eq!(s.sources[1].matching, Matching::Search);
        assert!(s.sources[1].replace && !s.sources[1].enabled);
        let p: Picks =
            serde_json::from_str(r#"{"picks":{"steam:70":{"portrait":"https://x/p.png"}}}"#)
                .expect("parses");
        assert_eq!(
            p.picks["steam:70"].portrait.as_deref(),
            Some("https://x/p.png")
        );
    }
}
