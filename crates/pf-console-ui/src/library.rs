//! Library model — everything the overlay shares that is not Skia.
//!
//! Games, phase, incoming art, generation and fetch epoch live in [`LibraryShared`].
//! Fetch threads write; the renderer drains per frame. The sections and favorites
//! settings helpers sit alongside. Cursor maths is [`crate::grid`], the card transform
//! [`crate::coverflow`], the palettes [`crate::palette`]. Rendering is `skia_overlay`.

use skia_safe::{ConditionallySend, Image};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LibraryView {
    Shelf,
    #[default]
    Grid,
}

impl LibraryView {
    /// Persisted `library_view`. Unset or unknown (a newer client's name) is the grid.
    pub fn parse(s: &str) -> LibraryView {
        match s {
            "shelf" => LibraryView::Shelf,
            _ => LibraryView::Grid,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            LibraryView::Shelf => "shelf",
            LibraryView::Grid => "grid",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LibraryView::Shelf => "Shelf",
            LibraryView::Grid => "Grid",
        }
    }

    pub const ALL: [LibraryView; 2] = [LibraryView::Shelf, LibraryView::Grid];
}

// --- The shared binary↔overlay model ------------------------------------------------------

#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LibraryPhase {
    Loading,
    Error {
        title: String,
        body: String,
        can_retry: bool,
    },
    Empty,
    Ready,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct LibraryGame {
    pub id: String,
    pub title: String,
    pub store: String,
    /// Opens the launcher itself, not a title. Host `role`, reduced by [`pf_client_core::library::GameEntry::is_launcher`].
    pub launcher: bool,
    /// Brand mark token, already validated by [`pf_client_core::library::GameEntry::icon_token`]. Empty or unknown draws nothing.
    pub icon: String,
    /// Host free-form display string (`"PC"`, `"PS2"`, …). `None` until [`crate::collate`] assigns a bucket.
    pub platform: Option<String>,
    /// What the launch hold says about a title beyond its name. The shelf draws none of it —
    /// a tile is a poster — so all three default, and a host or a bridge that says nothing
    /// leaves the hold with the title and the store, which is what it showed before.
    #[serde(default)]
    pub developer: Option<String>,
    #[serde(default)]
    pub year: Option<u16>,
    #[serde(default)]
    pub genres: Vec<String>,
    /// Host play stats, for the Recent and Most played sorts.
    #[serde(default)]
    pub stats: Option<pf_client_core::library::GameStats>,
    /// Already up on the host — pick resumes. From `/api/v1/status` via [`LibraryShared::set_running`].
    ///
    /// Host state, not catalog state: not on `GameEntry`, not persisted. A disk shelf cannot
    /// claim a title is running because it was last time. `false` on older hosts and while
    /// `/status` is in flight; the badge may appear a frame late rather than hold the catalog.
    pub running: bool,
    /// Running, and this device launched it: the host lets it end the title. Same source.
    #[serde(default)]
    pub endable: bool,
}

impl LibraryGame {
    /// In the shelf's leading band: the desktop tile, then the launchers — the tiles that
    /// open something rather than play a title. [`GridShape`]'s split is this run's length,
    /// so cursor math, the section heading and the renderer must all ask here.
    pub fn leads(&self) -> bool {
        self.launcher || self.id == DESKTOP_ID
    }
}

/// The console sorts its own reduced model; the policy is `pf_client_core::collate`.
impl pf_client_core::collate::Collatable for LibraryGame {
    fn id(&self) -> &str {
        &self.id
    }
    fn title(&self) -> &str {
        &self.title
    }
    fn store(&self) -> &str {
        &self.store
    }
    fn platform(&self) -> Option<&str> {
        self.platform.as_deref()
    }
    fn is_launcher(&self) -> bool {
        self.launcher
    }
    fn last_played_ms(&self) -> u64 {
        self.stats.map_or(0, |s| s.last_played_unix_ms)
    }
    fn play_time_ms(&self) -> u64 {
        self.stats.map_or(0, |s| s.play_time_ms)
    }
}

/// Observation vs memory, and whether a memory is still being fetched.
///
/// Separate states because each needs its own shelf copy. A boolean would say "waking"
/// while nothing is happening.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum Stale {
    No,
    /// Served from the disk cache while the host is asked. No packet is implied.
    Checking,
    /// Served from the disk cache while the host is being woken and re-asked.
    Waking,
    /// Disk cache; the host never answered. Not an error: these are still the titles to pick from.
    Offline,
}

impl Stale {
    pub(crate) fn note(self) -> Option<&'static str> {
        match self {
            Stale::No => None,
            Stale::Checking => Some("Last known library \u{2014} checking the host\u{2026}"),
            Stale::Waking => Some("Last known library \u{2014} waking the host\u{2026}"),
            Stale::Offline => Some("Last known library \u{2014} the host didn't answer"),
        }
    }
}

struct Shared {
    phase: LibraryPhase,
    games: Vec<LibraryGame>,
    /// Disk cache vs live host. Live [`LibraryShared::set_games`] resets this to [`Stale::No`].
    stale: Stale,
    /// Every poster fetched for this epoch, encoded, by title id. Kept for the fetch, not
    /// queued: whichever screen is up decodes what it lacks, and a cover a screen evicted
    /// or never drew comes back from here. Cleared by [`LibraryShared::begin_fetch`].
    art: HashMap<String, Arc<[u8]>>,
    /// Posters a host decoded on its own thread, waiting to be adopted. Separate from
    /// [`Shared::art`] because taking one costs nothing: the work is already done.
    decoded_in: VecDeque<(String, DecodedPoster)>,
    /// The scale the shelf caches art at, published for hosts that decode off-thread so they
    /// size it the way this crate would. `None` until a shelf has drawn once.
    art_scale: Option<f64>,
    /// Bumped on phase/games changes so the renderer re-syncs its snapshot.
    generation: u64,
    /// Bumped once per fetch, by [`LibraryShared::begin_fetch`].
    ///
    /// Distinguishes "this shelf's list" from "the previous host's, still here" without
    /// catching `Loading`. A warm cache publishes `Ready` inside one 60 Hz frame, so a
    /// phase edge can be missed; a counter cannot.
    fetch_epoch: u64,
    /// Each launched title's host-side state string (`launching`, `running`, …) and whether a
    /// `window` is still to come, by library id — the launch hold's answer. Replaced whole on
    /// every `/status` read.
    states: std::collections::HashMap<String, (String, bool)>,
    /// Bumped on every `/status` read, changed or not: the launch hold paces its next poll
    /// on an answer landing, not on the answer being different.
    status_gen: u64,
}

pub(crate) struct LibrarySnapshot {
    pub phase: LibraryPhase,
    pub games: Vec<LibraryGame>,
    pub stale: Stale,
    pub generation: u64,
}

/// One poster, decoded and ready to draw, on its way from a worker thread to the shell.
///
/// Skia's handles are only conditionally `Send` — safe to move while nothing else holds a
/// reference, which a freshly decoded image satisfies. [`crate::decode_poster_off_thread`] is
/// the only way to make one, so that condition is checked once, there, rather than trusted.
pub struct DecodedPoster(skia_safe::Sendable<Image>);

/// Decode a poster on a thread that is not drawing — see
/// [`crate::screens::library::decode_poster_off_thread`], which this forwards to so the sizing
/// policy stays with the screen that owns the cache.
pub fn decode_poster_off_thread(bytes: &[u8], k: f64) -> Option<DecodedPoster> {
    crate::screens::library::decode_poster_off_thread(bytes, k)
}

impl DecodedPoster {
    pub(crate) fn new(image: Image) -> Option<Self> {
        image.wrap_send().ok().map(DecodedPoster)
    }

    pub(crate) fn into_image(self) -> Image {
        self.0.into_inner()
    }
}

impl std::fmt::Debug for DecodedPoster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DecodedPoster")
    }
}

/// Binary write handle / overlay read handle. Fetch threads push; the renderer drains per frame.
#[derive(Clone)]
pub struct LibraryShared(Arc<Mutex<Shared>>);

impl Default for LibraryShared {
    fn default() -> Self {
        LibraryShared(Arc::new(Mutex::new(Shared {
            phase: LibraryPhase::Loading,
            games: Vec::new(),
            stale: Stale::No,
            art: HashMap::new(),
            decoded_in: VecDeque::new(),
            art_scale: None,
            generation: 0,
            fetch_epoch: 0,
            states: std::collections::HashMap::new(),
            status_gen: 0,
        })))
    }
}

impl LibraryShared {
    /// A fetch is starting: `Loading`, and the epoch advances.
    ///
    /// Must go through here, not `set_phase(Loading)`. A cache that answers before the next
    /// frame would otherwise leave the epoch unchanged and the shelf on the previous host.
    pub fn begin_fetch(&self) {
        let mut s = self.0.lock().unwrap();
        s.phase = LibraryPhase::Loading;
        // Previous host's stale note is not this fetch's; a cached render re-declares it.
        s.stale = Stale::No;
        // The previous host's posters are not this fetch's.
        s.art.clear();
        s.decoded_in.clear();
        s.fetch_epoch += 1;
        s.generation += 1;
    }

    /// Fetch the model is on. A shelf records this at push; a later difference means a new fetch
    /// owns the list. `pub` because the worker that does the fetching lives in the shell crate.
    pub fn fetch_epoch(&self) -> u64 {
        self.0.lock().unwrap().fetch_epoch
    }

    pub fn set_phase(&self, phase: LibraryPhase) {
        let mut s = self.0.lock().unwrap();
        s.phase = phase;
        s.generation += 1;
    }

    /// Host titles → carousel (empty = empty scene). Clears cached staleness.
    ///
    /// Launchers move to the front, host title order kept within each group. Grouping here
    /// so cursor math, the art pump, and [`GridShape`] all see the same prefix.
    pub fn set_games(&self, games: Vec<LibraryGame>) {
        self.put_games(games, Stale::No);
    }

    /// Disk-cache catalog while the host is still being asked. Live fetch stays in flight.
    /// A shell that sends a wake says so with [`Self::set_stale`]`(Waking)`.
    pub fn set_games_cached(&self, games: Vec<LibraryGame>) {
        self.put_games(games, Stale::Checking);
    }

    /// Shelf copy about a cached catalog, catalog unchanged. No-op on a live shelf, so a late
    /// abandoned-fetch give-up cannot mark a fresh library stale.
    pub fn set_stale(&self, stale: Stale) {
        let mut s = self.0.lock().unwrap();
        if s.stale == stale || s.stale == Stale::No {
            return;
        }
        s.stale = stale;
        s.generation += 1;
    }

    fn put_games(&self, mut games: Vec<LibraryGame>, stale: Stale) {
        // Empty is the CATALOG's verdict, taken before the desktop tile joins: a host with
        // no plugins keeps its empty copy, and gets the tile beside it.
        let empty = games.is_empty();
        games.insert(0, desktop_tile());
        order(&mut games);
        let mut s = self.0.lock().unwrap();
        s.phase = if empty {
            LibraryPhase::Empty
        } else {
            LibraryPhase::Ready
        };
        s.games = games;
        s.stale = stale;
        s.generation += 1;
    }

    /// The host's `/status` `games[]`, read after the catalog: which titles are up (the
    /// Resume badge, re-ordered) and each one's state (the launch hold). An empty list
    /// clears every badge (older or unreachable host).
    ///
    /// The badge side is a no-op — no generation bump — when nothing changed. This is polled.
    pub fn set_running(&self, games: &[pf_client_core::library::RunningGame]) {
        let mut s = self.0.lock().unwrap();
        s.status_gen += 1;
        s.states = games
            .iter()
            .filter_map(|g| Some((g.app_id.clone()?, (g.state.clone(), g.awaiting_window))))
            .collect();
        let mut changed = false;
        for g in &mut s.games {
            let mine = |r: &&pf_client_core::library::RunningGame| {
                r.is_up() && r.app_id.as_deref() == Some(g.id.as_str())
            };
            let now = games.iter().any(|r| mine(&r));
            let endable = games.iter().filter(mine).any(|r| r.endable);
            if g.running != now || g.endable != endable {
                g.running = now;
                g.endable = endable;
                changed = true;
            }
        }
        if !changed {
            return;
        }
        let mut games = std::mem::take(&mut s.games);
        order(&mut games);
        s.games = games;
        s.generation += 1;
    }

    /// The host's state string for one launched title, and whether it will report `window`
    /// next, from the last `/status` read; `None` when the host lists nothing for it (no lease
    /// yet, or the launch never resolved).
    pub(crate) fn launch_state(&self, id: &str) -> Option<(String, bool)> {
        self.0.lock().unwrap().states.get(id).cloned()
    }

    /// How many `/status` reads have landed — see `status_gen`.
    pub(crate) fn status_gen(&self) -> u64 {
        self.0.lock().unwrap().status_gen
    }

    pub fn push_art(&self, id: String, bytes: Vec<u8>) {
        self.0.lock().unwrap().art.insert(id, bytes.into());
    }

    /// A poster a host already decoded, off the thread that draws.
    ///
    /// The reason this exists: on a 2020 TV one full-size PNG cover costs ~90 ms, which is five
    /// frames. Decoded here it is a move and a hash insert, so a shelf fills without the frame
    /// loop stopping for each cover. [`Self::push_art`] stays for hosts with nowhere else to
    /// decode — they get the old behaviour, budgeted per frame.
    pub fn push_decoded(&self, id: String, poster: DecodedPoster) {
        self.0.lock().unwrap().decoded_in.push_back((id, poster));
    }

    /// The scale a host should decode at, once a shelf has published one. `None` before that —
    /// a host with nothing to go on should push encoded bytes and let the shelf size them.
    #[must_use]
    pub fn art_scale(&self) -> Option<f64> {
        self.0.lock().unwrap().art_scale
    }

    pub(crate) fn set_art_scale(&self, k: f64) {
        self.0.lock().unwrap().art_scale = Some(k);
    }

    /// Every poster decoded since the last call. Unbounded on purpose: adopting one is a move,
    /// so there is no frame budget to spend and holding them back only delays the picture.
    pub(crate) fn drain_decoded(&self) -> Vec<(String, DecodedPoster)> {
        self.0.lock().unwrap().decoded_in.drain(..).collect()
    }

    pub(crate) fn generation(&self) -> u64 {
        self.0.lock().unwrap().generation
    }

    pub(crate) fn snapshot(&self) -> LibrarySnapshot {
        let s = self.0.lock().unwrap();
        LibrarySnapshot {
            phase: s.phase.clone(),
            games: s.games.clone(),
            stale: s.stale,
            generation: s.generation,
        }
    }

    /// The first `max` of `ids` that have bytes, in that order. Nothing is consumed: a screen
    /// asks for what it lacks, on-screen titles first, and decodes at its own pace.
    pub(crate) fn art_for<'a>(
        &self,
        ids: impl IntoIterator<Item = &'a str>,
        max: usize,
    ) -> Vec<(String, Arc<[u8]>)> {
        let s = self.0.lock().unwrap();
        ids.into_iter()
            .filter_map(|id| s.art.get(id).map(|b| (id.to_string(), b.clone())))
            .take(max)
            .collect()
    }
}

/// Shelf display order, the one path every writer uses.
///
/// Launchers first (the [`GridShape`] prefix). Running titles lead within their group.
/// `sort_by_key` is stable, so host title order survives inside each band.
fn order(games: &mut [LibraryGame]) {
    games.sort_by_key(|g| (g.id != DESKTOP_ID, !g.launcher, !g.running));
}

/// The desktop tile's id and mark, and the store→label table. All live in `pf-client-core`
/// now, so the GTK and WinUI dialogs read the same ones; re-exported because the screens name
/// them through this module.
pub use pf_client_core::library::{initials, store_label, DESKTOP_ICON, DESKTOP_ID};

/// Every shelf leads with the host's own desktop, so Library is never a dead end for the
/// desktop-only user and a plugin-less host is still one press from streaming. Model state
/// only: the disk cache stores wire `GameEntry`s and never sees this.
fn desktop_tile() -> LibraryGame {
    LibraryGame {
        id: DESKTOP_ID.into(),
        title: "Desktop".into(),
        store: String::new(),
        launcher: false,
        icon: DESKTOP_ICON.into(),
        platform: None,
        developer: None,
        year: None,
        genres: Vec::new(),
        stats: None,
        running: false,
        endable: false,
    }
}

// The Games tab's section layout, favorites and play captions, shared with the desktop
// shell through one settings file.
pub use pf_client_core::library_layout::{
    ago, favorites, sections, stats_line, stored_sections, toggle_favorite, Section,
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Poster bytes stay for the fetch: any screen takes what it lacks, in the order it asks,
    /// as often as it asks; the next fetch starts clean.
    #[test]
    fn art_stays_for_the_fetch_and_serves_in_asked_order() {
        let shared = LibraryShared::default();
        for i in 0..6 {
            shared.push_art(format!("g{i}"), vec![i as u8]);
        }
        let ids =
            |got: Vec<(String, Arc<[u8]>)>| got.into_iter().map(|(id, _)| id).collect::<Vec<_>>();
        assert_eq!(
            ids(shared.art_for(["g4", "g1", "g9"], 8)),
            ["g4", "g1"],
            "an id that never arrived is not an error"
        );
        assert_eq!(
            ids(shared.art_for(["g4", "g1"], 8)),
            ["g4", "g1"],
            "not consumed"
        );
        assert_eq!(shared.art_for(["g0", "g1", "g2"], 2).len(), 2, "bounded");
        shared.begin_fetch();
        assert!(
            shared.art_for(["g0", "g1"], 9).is_empty(),
            "a new fetch starts clean"
        );
    }

    /// Persisted view name is a file format. Unknown → shelf.
    #[test]
    fn library_view_parses_leniently() {
        assert_eq!(LibraryView::parse("grid"), LibraryView::Grid);
        assert_eq!(LibraryView::parse("shelf"), LibraryView::Shelf);
        assert_eq!(LibraryView::parse("coverwall"), LibraryView::Grid);
        assert_eq!(LibraryView::parse(""), LibraryView::Grid);
        assert_eq!(LibraryView::default(), LibraryView::Grid);
        for v in LibraryView::ALL {
            assert_eq!(LibraryView::parse(v.id()), v, "{} round-trips", v.label());
        }
    }

    /// Launchers lead; host title order survives within each group. `launcher_count()` is prefix `0..n`.
    #[test]
    fn set_games_groups_launchers_first_and_keeps_title_order() {
        let g = |title: &str, launcher: bool| LibraryGame {
            id: format!("steam:{title}"),
            title: title.to_string(),
            store: "steam".into(),
            launcher,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
            endable: false,
        };
        let shared = LibraryShared::default();
        shared.set_games(vec![
            g("Celeste", false),
            g("Big Picture", true),
            g("Portal 2", false),
            g("Heroic", true),
        ]);
        let snap = shared.snapshot();
        assert!(matches!(snap.phase, LibraryPhase::Ready));
        assert_eq!(snap.stale, Stale::No, "a live fetch is not a memory");
        let titles: Vec<&str> = snap.games.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Desktop", "Big Picture", "Heroic", "Celeste", "Portal 2"]
        );
        assert_eq!(
            snap.games.iter().take_while(|g| g.leads()).count(),
            3,
            "the lead band is the desktop tile plus both launchers"
        );
    }

    /// Running titles lead within their group; the launcher prefix [`GridShape`] counts stays intact.
    #[test]
    fn running_titles_lead_without_breaking_the_launcher_prefix() {
        let g = |title: &str, launcher: bool| LibraryGame {
            id: format!("steam:{title}"),
            title: title.to_string(),
            store: "steam".into(),
            launcher,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
            endable: false,
        };
        let shared = LibraryShared::default();
        shared.set_games(vec![
            g("Celeste", false),
            g("Big Picture", true),
            g("Portal 2", false),
            g("Heroic", true),
            g("Tunic", false),
        ]);
        let up = running(&["steam:Portal 2", "steam:Heroic"], "running");
        shared.set_running(&up);
        let snap = shared.snapshot();
        let titles: Vec<&str> = snap.games.iter().map(|g| g.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Desktop",
                "Heroic",
                "Big Picture",
                "Portal 2",
                "Celeste",
                "Tunic"
            ],
            "running first inside each group; the lead band is still the prefix"
        );
        assert_eq!(
            snap.games.iter().take_while(|g| g.leads()).count(),
            3,
            "the lead band GridShape depends on is intact"
        );
        assert!(snap.games[1].running && snap.games[3].running);

        // Same set: no generation bump. This is polled.
        let gen_before = snap.generation;
        shared.set_running(&up);
        assert_eq!(shared.snapshot().generation, gen_before);

        shared.set_running(&[]);
        let after = shared.snapshot();
        assert!(after.games.iter().all(|g| !g.running));
        assert!(after.generation > gen_before, "a real change does re-sync");
    }

    fn running(ids: &[&str], state: &str) -> Vec<pf_client_core::library::RunningGame> {
        ids.iter()
            .map(|id| pf_client_core::library::RunningGame {
                app_id: Some((*id).to_string()),
                title: String::new(),
                state: state.to_string(),
                awaiting_window: false,
                session_id: None,
                endable: false,
            })
            .collect()
    }

    /// The launch hold reads each title's state, and paces on reads landing — so a read
    /// that changes no badge still counts, and `launching` is up for the badge but not
    /// running for the hold.
    #[test]
    fn status_reads_keep_state_and_count_even_when_nothing_changed() {
        let shared = LibraryShared::default();
        shared.set_games(vec![LibraryGame {
            id: "steam:Celeste".into(),
            title: "Celeste".into(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
            endable: false,
        }]);
        assert_eq!(shared.status_gen(), 0);
        shared.set_running(&running(&["steam:Celeste"], "launching"));
        assert_eq!(shared.status_gen(), 1);
        assert_eq!(
            shared
                .launch_state("steam:Celeste")
                .map(|(s, _)| s)
                .as_deref(),
            Some("launching")
        );
        assert!(
            shared.snapshot().games[1].running,
            "launching is up on the shelf, behind the desktop tile"
        );
        let badges = shared.snapshot().generation;
        shared.set_running(&running(&["steam:Celeste"], "running"));
        assert_eq!(shared.status_gen(), 2);
        assert_eq!(shared.snapshot().generation, badges, "no badge moved");
        assert_eq!(
            shared
                .launch_state("steam:Celeste")
                .map(|(s, _)| s)
                .as_deref(),
            Some("running")
        );
        shared.set_running(&[]);
        assert_eq!(shared.launch_state("steam:Celeste"), None);
    }

    /// Cached catalog is `Ready` + stale, never an error. Live `set_games` clears the flag.
    #[test]
    fn a_cached_catalog_is_ready_and_stale_until_the_host_answers() {
        let g = |t: &str| LibraryGame {
            id: format!("steam:{t}"),
            title: t.to_string(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: false,
            endable: false,
        };
        let shared = LibraryShared::default();
        shared.set_games_cached(vec![g("Celeste"), g("Tunic")]);
        let cached = shared.snapshot();
        assert!(matches!(cached.phase, LibraryPhase::Ready));
        assert_eq!(
            cached.stale,
            Stale::Checking,
            "a cached shelf claims no wake"
        );
        assert!(cached.stale.note().is_some());
        shared.set_stale(Stale::Waking);
        assert_eq!(shared.snapshot().stale, Stale::Waking);
        // The retry window closed with no answer: same titles, different words.
        shared.set_stale(Stale::Offline);
        assert_eq!(shared.snapshot().stale, Stale::Offline);
        shared.set_games(vec![g("Celeste"), g("Tunic"), g("Hades")]);
        let live = shared.snapshot();
        assert_eq!(
            live.stale,
            Stale::No,
            "the host answered — these are observed now"
        );
        assert!(live.stale.note().is_none());
        assert_eq!(live.games.len(), 4, "three titles behind the desktop tile");
        // A late give-up from an abandoned fetch must not mark a fresh library stale.
        shared.set_stale(Stale::Offline);
        assert_eq!(shared.snapshot().stale, Stale::No);
    }

    /// A launcher-less library keeps the host's order behind the one tile we add.
    #[test]
    fn set_games_leaves_a_launcher_less_library_alone() {
        let shared = LibraryShared::default();
        shared.set_games(
            ["Celeste", "Portal 2", "Tunic"]
                .iter()
                .map(|t| LibraryGame {
                    id: format!("steam:{t}"),
                    title: (*t).to_string(),
                    store: "steam".into(),
                    launcher: false,
                    icon: String::new(),
                    platform: None,
                    developer: None,
                    year: None,
                    genres: Vec::new(),
                    stats: None,
                    running: false,
                    endable: false,
                })
                .collect(),
        );
        let titles: Vec<String> = shared
            .snapshot()
            .games
            .iter()
            .map(|g| g.title.clone())
            .collect();
        assert_eq!(titles, ["Desktop", "Celeste", "Portal 2", "Tunic"]);
    }

    /// The tile is model state, so it leads whatever the catalog says — including a
    /// catalog with nothing in it, which keeps its Empty verdict for the shelf's copy.
    #[test]
    fn the_desktop_tile_leads_every_shelf_including_an_empty_one() {
        let shared = LibraryShared::default();
        shared.set_games(Vec::new());
        let snap = shared.snapshot();
        assert!(matches!(snap.phase, LibraryPhase::Empty));
        assert_eq!(snap.games.len(), 1);
        assert_eq!(snap.games[0].id, DESKTOP_ID);
        assert!(snap.games[0].leads());

        // A running title re-orders the games; the tile is not one of them.
        shared.set_games(vec![LibraryGame {
            id: "steam:Celeste".into(),
            title: "Celeste".into(),
            store: "steam".into(),
            launcher: false,
            icon: String::new(),
            platform: None,
            developer: None,
            year: None,
            genres: Vec::new(),
            stats: None,
            running: true,
            endable: false,
        }]);
        assert_eq!(shared.snapshot().games[0].id, DESKTOP_ID);
    }

    /// The tile has no cover and never gets one, so its mark is the whole card. A name the set
    /// does not carry falls through to the monogram, silently and on every shell at once.
    #[test]
    fn the_desktop_tile_names_a_mark_the_icon_set_ships() {
        assert!(crate::icons::by_name(&desktop_tile().icon).is_some());
    }

    /// Collections must never offer a "Desktop" group, and a one-store library must not
    /// look browsable because of it — but the unfiltered shelf still leads with the tile.
    #[test]
    fn collections_never_group_the_desktop_tile() {
        use crate::collate::{collate, filtered, worth_browsing, GroupBy, SortKey};

        let shared = LibraryShared::default();
        shared.set_games(
            ["Celeste", "Tunic"]
                .iter()
                .map(|t| LibraryGame {
                    id: format!("steam:{t}"),
                    title: (*t).to_string(),
                    store: "steam".into(),
                    launcher: false,
                    icon: String::new(),
                    platform: Some("PC".into()),
                    developer: None,
                    year: None,
                    genres: Vec::new(),
                    stats: None,
                    running: false,
                    endable: false,
                })
                .collect(),
        );
        let games = shared.snapshot().games;
        assert!(
            !worth_browsing(&games),
            "one platform plus the tile is still one platform"
        );
        for group in collate(&games, SortKey::Title, Some(GroupBy::Platform)) {
            assert!(
                !group.games.iter().any(|&i| games[i].id == DESKTOP_ID),
                "the tile reached a collection"
            );
        }
        assert_eq!(
            filtered(&games, SortKey::Title, None).first(),
            Some(&0),
            "the unfiltered shelf still leads with the tile"
        );
    }
}
