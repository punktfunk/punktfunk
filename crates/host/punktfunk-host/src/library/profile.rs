//! A profile's own layer of the library: the titles it hides, the sources it turns off, its game
//! overrides, art picks and session settings, in `library-profiles/<profile id>/` under the
//! library dir, each in the shape of the box's file. Nothing in it runs.
//!
//! A [`View`] is whose library a call reads or writes: the box's layer alone, or a profile's on
//! top of it, with the [`Reach`] of the host that profile plays on. A layer adds and narrows,
//! never widens: what the box hides or turns off stays so. The owner's layer is the box's. With no
//! profile in view every loader reads the box's files alone; a contract seat's own view is its
//! profile's. The box host writes the layers; a seat host reads its own from its library copy.
//! A seat host's own titles reach its layer as names and pictures alone ([`OwnTitle`]).
//! Pin: `mgmt::tests::profile_view`.

use super::*;
use std::sync::{Arc, Mutex};

/// The folder under the library dir that holds one folder per profile.
const PROFILES_DIR: &str = "library-profiles";

/// A host-minted profile id: 12 lowercase hex. Only such an id becomes a path.
pub(crate) fn is_profile_id(id: &str) -> bool {
    id.len() == 12 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// What a host that plays the box's library lists of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Reach {
    /// It reads the box's library from a host of its own: one account's sources are the owner's.
    pub(crate) own_host: bool,
    /// That host follows the seat contract: one virtual screen, no Game Mode of its own.
    pub(crate) contract: bool,
    /// That host is a Linux seat: it plays as its own user and runs no plugin.
    pub(crate) linux_seat: bool,
    /// That host runs on a person's own account: no program the operator wrote runs there.
    pub(crate) own_account: bool,
}

impl Reach {
    /// This process's.
    pub(crate) fn here() -> Self {
        Self {
            own_host: pf_paths::seat::box_library_dir().is_some(),
            contract: pf_paths::seat::role().follows_contract(),
            linux_seat: linux_seat(),
            own_account: pf_paths::seat::own_account(),
        }
    }

    /// A seat's host, on a person's own account when `own_account`.
    pub(crate) fn seat(own_account: bool) -> Self {
        Self {
            own_host: true,
            contract: true,
            linux_seat: cfg!(target_os = "linux"),
            own_account,
        }
    }
}

/// Whose library a call reads or writes.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct View {
    /// The profile whose layer sits on the box's; a checked id. `None`: the box's alone.
    layer: Option<String>,
    pub(crate) reach: Reach,
}

impl View {
    /// No profile named: this host's own. A contract seat's is its profile's.
    pub fn here() -> Self {
        Self {
            layer: seat_profile(),
            reach: Reach::here(),
        }
    }

    /// Profile `id` on a host that plays as `reach`. `owner`: the box's own session, whose layer
    /// is the box's.
    pub(crate) fn of(id: &str, owner: bool, reach: Reach) -> Self {
        Self {
            layer: (!owner && is_profile_id(id)).then(|| id.to_string()),
            reach,
        }
    }

    /// A session's on this host, as its profile resolved.
    pub(crate) fn session(profile: &crate::profiles::Resolved) -> Self {
        Self::of(&profile.id, profile.owner, Reach::here())
    }

    pub(crate) fn layer(&self) -> Option<&str> {
        self.layer.as_deref()
    }

    /// The profile's layer; `None` with no profile in view.
    pub(crate) fn own_dir(&self) -> Option<PathBuf> {
        let id = self.layer.as_ref()?;
        Some(pf_paths::seat::library_dir().join(PROFILES_DIR).join(id))
    }

    /// `name` in the profile's layer; `None` with no profile in view.
    pub(crate) fn own_file(&self, name: &str) -> Option<PathBuf> {
        self.own_dir().map(|dir| dir.join(name))
    }

    /// `name` in this view's own layer, or in the box's library dir.
    pub(crate) fn path(&self, name: &str) -> PathBuf {
        self.own_file(name)
            .unwrap_or_else(|| pf_paths::seat::library_dir().join(name))
    }

    /// The box's own view of the files, for the layer under this one.
    pub(crate) fn base(&self) -> Self {
        Self {
            layer: None,
            reach: self.reach,
        }
    }
}

/// A contract seat's own profile: the one the box's `profiles.json` in its trust dir names for
/// its seat, as [`crate::profiles::Profiles::load_box`] reads it. `None` on any other host.
fn seat_profile() -> Option<String> {
    if !pf_paths::seat::role().follows_contract() {
        return None;
    }
    let path = pf_paths::seat::trust_dir()?.join("profiles.json");
    let seat = pf_paths::seat::seat_id().ok().flatten()?;
    type Kept = Mutex<Option<(PathBuf, Arc<crate::profiles::Profiles>)>>;
    static KEPT: Kept = Mutex::new(None);
    let profiles = {
        let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
        match kept.as_ref() {
            Some((p, profiles)) if *p == path => profiles.clone(),
            _ => {
                let profiles = Arc::new(crate::profiles::Profiles::load_box(
                    path.clone(),
                    Some(seat),
                ));
                *kept = Some((path, profiles.clone()));
                profiles
            }
        }
    };
    let id = profiles.pick(None, None).ok()?.id;
    is_profile_id(&id).then_some(id)
}

/// The values on a row that the profile in view (`as`) set itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, ToSchema)]
pub struct ProfileValues {
    /// `overrides` is the profile's: its entry for the game, or for the game's source.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    pub overrides: bool,
    /// The art slots the profile picked: `portrait`, `hero`, `logo`, `header`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(required = false)]
    pub picks: Vec<String>,
}

/// The profile's own overrides and picks, read once per list.
#[derive(Default)]
pub(crate) struct OwnValues {
    overrides: Option<Overrides>,
    picks: BTreeMap<String, Artwork>,
}

impl OwnValues {
    /// Nothing with no profile in view.
    pub(crate) fn load(view: &View) -> Self {
        if view.layer().is_none() {
            return Self::default();
        }
        Self {
            overrides: Some(Overrides::load(view)),
            picks: own_picks(view),
        }
    }

    /// `None` when the profile set nothing for `id`.
    pub(crate) fn of(&self, id: &str) -> Option<ProfileValues> {
        let overrides = self
            .overrides
            .as_ref()
            .is_some_and(|o| o.own_entry(id).is_some());
        let picks: Vec<String> = self.picks.get(id).map_or_else(Vec::new, |art| {
            ArtKind::ALL
                .iter()
                .filter(|k| art_field(art, **k).is_some())
                .map(|k| k.name().to_string())
                .collect()
        });
        (overrides || !picks.is_empty()).then_some(ProfileValues { overrides, picks })
    }
}

/// A profile's own titles as its seat host published them, in its layer.
const OWN_TITLES: &str = "own-titles.json";
/// Most own titles one seat publishes: an answer that lists more is refused, not cut short.
const OWN_TITLES_MAX: usize = 500;
/// The largest answer: the management API's own request body limit.
const OWN_TITLES_BYTES: usize = 2 * 1024 * 1024;
/// Longest title or platform kept, as for an Art & Metadata source's short text.
const OWN_TEXT_MAX: usize = 256;

/// One of a profile's own titles as its seat host publishes it to the box: what a shelf shows,
/// never what starts it. Built field by field, so a field added to [`CustomEntry`] stays on the
/// seat until it is added here.
#[derive(Clone, Debug, Default, Serialize, Deserialize, ToSchema)]
pub struct OwnTitle {
    /// `own:` and 12 lowercase hex, minted by the seat host.
    #[schema(example = "own:3f9a0c1b2d4e")]
    pub id: String,
    pub title: String,
    /// `http(s)` URLs alone: a cover on the seat's disk stays there.
    #[serde(default)]
    pub art: Artwork,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "PC")]
    pub platform: Option<String>,
}

impl OwnTitle {
    /// `e` as the box may see it.
    pub(crate) fn of(e: &CustomEntry) -> Self {
        let mut art = Artwork::default();
        for kind in ArtKind::ALL {
            *art_slot(&mut art, kind) = art_field(&e.art, kind).filter(|u| valid_remote_url(u));
        }
        Self {
            id: e.id.clone(),
            title: e.title.clone(),
            art,
            platform: e.meta.platform.clone(),
        }
    }
}

impl From<OwnTitle> for CustomEntry {
    /// A published title as the box lists it: with no launch, nothing starts it there.
    fn from(t: OwnTitle) -> Self {
        CustomEntry {
            id: t.id,
            title: t.title,
            art: t.art,
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
            ids: BTreeMap::new(),
            install: None,
            meta: GameMeta {
                platform: t.platform,
                ..GameMeta::default()
            },
        }
    }
}

/// `own-titles.json`.
#[derive(Default, Serialize, Deserialize)]
struct OwnTitles {
    #[serde(default)]
    titles: Vec<OwnTitle>,
}

/// The titles `view`'s profile's seat published; none with no profile in view.
pub(crate) fn own_titles(view: &View) -> Vec<OwnTitle> {
    let Some(path) = view.own_file(OWN_TITLES) else {
        return Vec::new();
    };
    let mut titles = read_json_or_default::<OwnTitles>(&path).titles;
    titles.retain(|t| is_own_id(&t.id));
    titles
}

/// What this host's `GET /library/own` answers: each entry it keeps of its own, as the box may
/// see it. A host that keeps none answers none.
pub fn published_own_titles() -> Vec<OwnTitle> {
    if !keeps_own_titles() {
        return Vec::new();
    }
    own_entries(&View::here())
        .iter()
        .map(OwnTitle::of)
        .collect()
}

/// A seat host's `GET /library/own` answer, `body`, as the box keeps it for `profile`, checked as
/// an Art & Metadata source's push is: a row with a bad id or title, a cover that isn't `http(s)`
/// and a bad platform are dropped. An answer that isn't a list of titles, is too large, lists
/// more than [`OWN_TITLES_MAX`] or names an id twice is refused, and logged.
pub fn checked_own_titles(
    profile: &str,
    body: &[u8],
) -> std::result::Result<Vec<OwnTitle>, String> {
    let checked = check_own_titles(body);
    if let Err(why) = &checked {
        tracing::warn!(profile, reason = %why, "seat's own titles refused: the stored ones stay");
    }
    checked
}

fn check_own_titles(body: &[u8]) -> std::result::Result<Vec<OwnTitle>, String> {
    if body.len() > OWN_TITLES_BYTES {
        return Err(format!("an answer of {} bytes", body.len()));
    }
    let rows: Vec<OwnTitle> =
        serde_json::from_slice(body).map_err(|e| format!("not a list of titles: {e}"))?;
    if rows.len() > OWN_TITLES_MAX {
        return Err(format!("{} titles", rows.len()));
    }
    let text = |s: &str| {
        !s.trim().is_empty() && s.len() <= OWN_TEXT_MAX && !s.chars().any(char::is_control)
    };
    let mut seen = HashSet::new();
    let mut kept = Vec::with_capacity(rows.len());
    for mut t in rows {
        if !seen.insert(t.id.clone()) {
            return Err(format!("{} twice", t.id));
        }
        if !is_own_id(&t.id) || !text(&t.title) {
            continue;
        }
        for kind in ArtKind::ALL {
            let slot = art_slot(&mut t.art, kind);
            if slot.as_deref().is_some_and(|u| !valid_remote_url(u)) {
                *slot = None;
            }
        }
        t.platform = t.platform.filter(|p| text(p));
        kept.push(t);
    }
    Ok(kept)
}

/// Stores `titles` as profile `profile`'s own, in its layer, on the host that writes the library:
/// whatever profile a row or the seat named, these are `profile`'s. No titles removes the file.
/// `true` when they changed, which emits `library.changed`.
pub fn store_own_titles(profile: &str, titles: Vec<OwnTitle>) -> Result<bool> {
    anyhow::ensure!(
        pf_paths::seat::box_library_dir().is_none(),
        "store own titles on a host that reads the box's library"
    );
    let path = View::of(profile, false, Reach::default())
        .own_file(OWN_TITLES)
        .with_context(|| format!("profile id {profile:?}"))?;
    let changed = if titles.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                wrote();
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(e).with_context(|| format!("remove {}", path.display())),
        }
    } else {
        let json = serde_json::to_string_pretty(&OwnTitles { titles })?;
        let same = std::fs::read_to_string(&path).is_ok_and(|old| old == json);
        if !same {
            save_json(&path, &json)?;
        }
        !same
    };
    if changed {
        crate::events::emit(crate::events::EventKind::LibraryChanged {
            source: "manual".into(),
        });
    }
    Ok(changed)
}

/// Removes profile `id`'s layer, on the host that writes the library.
pub fn forget_profile(id: &str) {
    if pf_paths::seat::box_library_dir().is_some() || !is_profile_id(id) {
        return;
    }
    let dir = pf_paths::seat::library_dir().join(PROFILES_DIR).join(id);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => wrote(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(dir = %dir.display(), error = %e, "profile library layer not removed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_minted_id_is_a_profile_id() {
        assert!(is_profile_id("a1b2c3d4e5f6"));
        for bad in [
            "",
            "A1B2C3D4E5F6",
            "a1b2c3d4e5f",
            "a1b2c3d4e5f60",
            "../../../etc",
            "a1b2c3/4e5f6",
            "a1b2c3d4e5g6",
            "operator",
            "a1b2c3d4e5f\0",
        ] {
            assert!(!is_profile_id(bad), "{bad:?}");
        }
    }

    /// The owner's layer is the box's, and an id that is no minted one never names a folder.
    #[test]
    fn a_view_has_a_layer_only_for_a_minted_id_that_is_not_the_owner() {
        let reach = Reach::default();
        assert_eq!(View::of("a1b2c3d4e5f6", true, reach).layer(), None);
        assert_eq!(View::of("../../x", false, reach).layer(), None);
        assert_eq!(View::of("operator", false, reach).layer(), None);
        let p = View::of("a1b2c3d4e5f6", false, reach);
        assert_eq!(p.layer(), Some("a1b2c3d4e5f6"));
        let path = p.path("library-hidden.json");
        assert!(path.ends_with("library-profiles/a1b2c3d4e5f6/library-hidden.json"));
        assert_eq!(p.base().layer(), None);
        assert!(View::default().own_file("x").is_none());
    }

    #[test]
    fn an_own_id_is_own_and_12_lowercase_hex() {
        assert!(is_own_id("own:0123456789ab"));
        for bad in [
            "own:",
            "own:0123456789AB",
            "own:0123456789a",
            "own:0123456789abc",
            "own:../../etc/x",
            "custom:0123456789ab",
            "0123456789ab",
            "steam:570",
        ] {
            assert!(!is_own_id(bad), "{bad:?}");
        }
    }

    /// An entry with every field set leaves the seat as its id, title, platform and remote art.
    #[test]
    fn an_own_title_is_a_name_and_a_picture_alone() {
        let full = serde_json::json!({
            "id": "own:0123456789ab", "title": "Doom",
            "art": {"portrait": "/home/anna/covers/doom.png", "hero": "https://cdn.example/h.jpg",
                    "logo": "https://cdn.example/l.png", "header": "C:\\covers\\doom.jpg"},
            "launch": {"kind": "command", "value": "gzdoom -iwad doom.wad",
                       "args": [{"name": "rom", "value": "/roms/doom.wad"}]},
            "prep": [{"do": "xrandr --output X", "undo": "xrandr --auto", "elevated": true}],
            "provider": "romm", "external_id": "doom-1", "store": "romm",
            "role": "launcher", "icon": "steam",
            "detect": {"install_dir": "/games/doom", "exe": "/games/doom/gzdoom",
                       "process_name": "gzdoom", "steam_appid": 2280,
                       "env_marker": {"key": "DOOM", "value": "1"}},
            "on_window": {"workspace": "current", "focus": true, "fullscreen": true,
                          "move_to_stream_output": true},
            "audio": {"sessions": "owner"}, "ids": {"steam": "2280"},
            "install": {"state": "missing", "size_bytes": 1, "target": "/games"},
            "platform": "PC", "description": "Rip and tear.", "developer": "id",
            "publisher": "id", "release_year": 1993, "genres": ["FPS"], "tags": ["classic"],
            "region": "World", "players": 4,
        });
        let entry: CustomEntry = serde_json::from_value(full.clone()).unwrap();
        let stored = serde_json::to_value(&entry).unwrap();
        let keys = |v: &serde_json::Value| -> std::collections::BTreeSet<String> {
            v.as_object().unwrap().keys().cloned().collect()
        };
        assert_eq!(keys(&stored), keys(&full), "every field is set");
        let published = serde_json::to_value(OwnTitle::of(&entry)).unwrap();
        let want = serde_json::json!({
            "id": "own:0123456789ab", "title": "Doom", "platform": "PC",
            "art": {"hero": "https://cdn.example/h.jpg", "logo": "https://cdn.example/l.png"},
        });
        assert_eq!(published, want);
    }

    /// What the box keeps of a seat's answer: a garbage answer is refused whole, a bad row or
    /// value is dropped, and nothing but the four fields survives.
    #[test]
    fn the_box_checks_a_seat_s_answer_as_a_source_s() {
        let check = |v: serde_json::Value| check_own_titles(v.to_string().as_bytes());
        let row = |id: &str| serde_json::json!({"id": id, "title": "T"});
        for garbage in [
            serde_json::json!({"titles": []}),
            serde_json::json!([{"id": "own:0123456789ab"}]),
            serde_json::json!([row("own:0123456789ab"), row("own:0123456789ab")]),
            serde_json::Value::Array(
                (0..=OWN_TITLES_MAX)
                    .map(|i| row(&format!("own:{i:012x}")))
                    .collect(),
            ),
        ] {
            assert!(check(garbage).is_err());
        }
        assert!(check_own_titles(b"\xff not json").is_err());
        let big = vec![b' '; OWN_TITLES_BYTES + 1];
        assert!(check_own_titles(&big).is_err());

        let answer = serde_json::json!([
            {"id": "own:0123456789ab", "title": "Doom", "platform": "x".repeat(OWN_TEXT_MAX + 1),
             "art": {"portrait": "/home/anna/p.png", "hero": "https://cdn.example/h.jpg"},
             "launch": {"kind": "command", "value": "rm -rf ~"}, "prep": [{"do": "true"}],
             "profile": "0f0f0f0f0f0f"},
            {"id": "steam:570", "title": "Dota"},
            {"id": "custom:0123456789ab", "title": "Theirs"},
            {"id": "own:00000000000a", "title": " "},
            {"id": "own:00000000000b", "title": "Bad\u{0}title"},
            {"id": "own:00000000000c", "title": "Quake", "platform": "PC"},
        ]);
        let kept = serde_json::to_value(check(answer).unwrap()).unwrap();
        let want = serde_json::json!([
            {"id": "own:0123456789ab", "title": "Doom", "art": {"hero": "https://cdn.example/h.jpg"}},
            {"id": "own:00000000000c", "title": "Quake", "art": {}, "platform": "PC"},
        ]);
        assert_eq!(kept, want);
        assert!(check(serde_json::json!([])).unwrap().is_empty());
    }
}
