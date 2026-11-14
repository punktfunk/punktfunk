//! The Library destination: one shelf at a time — a paired host, or a host with one of its
//! pinned presets — laid out in the sections this device keeps (`library_rows`). Picking a
//! title starts a stream carrying `--launch id`; the host launches it during the connect.
//!
//! The catalog comes from the disk first, then the host, over `pf-client-core::library` (mTLS
//! against the host's management API, pre-classified errors, the 3-worker art pipeline). Poster
//! bytes land in the shared `art_cache`, which the `Image` loads them from; a poster the cache
//! could not keep shows from the fetched bytes. The fetch and the art stream complete on worker
//! threads into the root's [`LibraryState`]; `Shared::library_gen` stops a superseded fetch from
//! publishing.
//!
//! The rows are one virtualized list, so a library of thousands builds what is on screen.

use super::connect::{initiate, initiate_launch, initiate_launch_waking, initiate_waking};
use super::hosts::{self, CardPreset, MenuRows};
use super::lucide;
use super::style::*;
use super::{AppCtx, AsyncSetState, Dest, Msg, Screen, Shell, Svc, Target};
use pf_client_core::collate::{GroupBy, SortKey};
use pf_client_core::hide;
use pf_client_core::library::{self, initials, store_label, GameEntry, LibraryError, DESKTOP_ID};
use pf_client_core::library_layout as layout;
use pf_client_core::library_rows::{self as rows, Caption, Row, Tile};
use pf_client_core::orchestrate::HostTarget;
use pf_client_core::trust::{KnownHosts, Settings};
use pf_fluent::{Palette, Token};
use pf_l10n::{client, common, desktop};
use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use windows_reactor::*;

/// Poster metrics: the narrowest a grid column gets, a row tile's width, the gap, and the 2:3
/// portrait ratio.
const POSTER_MIN_WIDTH: f64 = 150.0;
const BAND_POSTER_WIDTH: f64 = 150.0;
const POSTER_GAP: f64 = 14.0;
const POSTER_RATIO: f64 = 1.5;
/// A Desktops tile.
const DESKTOP_WIDTH: f64 = 200.0;
const DESKTOP_HEIGHT: f64 = 104.0;
/// How long arriving posters gather before the shelf is told: about ten renders a second at
/// most, however fast the workers stream.
const ART_BATCH: std::time::Duration = std::time::Duration::from_millis(100);
/// A hidden title's tile: there to bring back, not to play.
const HIDDEN_OPACITY: f64 = 0.55;

/// One shelf: a paired host's library, streamed with its binding or one of its pinned presets.
#[derive(Clone)]
pub(crate) struct Shelf {
    /// The host's record, then `/preset id` for a pinned shelf: what `library_shelf` keeps.
    pub(crate) key: String,
    /// "Desk", or "Desk · Work" for a pinned shelf.
    label: String,
    /// The host, with a pinned shelf's preset as the one-off its titles launch with.
    pub(crate) target: Target,
    /// The saved pick's id: the library is the one that profile plays.
    profile: Option<String>,
}

/// A shelf's key from its host and preset, the same wherever a shelf is named.
pub(crate) fn shelf_key(host: &HostTarget, preset: Option<&str>) -> String {
    let record = host.id.clone().unwrap_or_else(|| match &host.fp_hex {
        Some(fp) if !fp.is_empty() => fp.clone(),
        _ => format!("{}:{}", host.addr, host.port),
    });
    match preset.filter(|p| !p.is_empty()) {
        Some(preset) => format!("{record}/{preset}"),
        None => record,
    }
}

/// Every shelf there is: each paired host, then each preset pinned to it that still exists.
pub(crate) fn shelves(known: &KnownHosts, presets: &[CardPreset]) -> Vec<Shelf> {
    let mut out = Vec::new();
    for k in known.hosts.iter().filter(|k| k.paired) {
        let host = HostTarget::from(k);
        let profile = k.profile.as_ref().map(|p| p.id.clone());
        out.push(Shelf {
            key: shelf_key(&host, None),
            label: k.name.clone(),
            target: Target {
                host: host.clone(),
                ..Target::default()
            },
            profile: profile.clone(),
        });
        for id in &k.pinned_presets {
            let Some(p) = presets.iter().find(|p| &p.id == id) else {
                continue;
            };
            out.push(Shelf {
                key: shelf_key(&host, Some(id)),
                label: format!("{} \u{b7} {}", k.name, p.name),
                target: Target {
                    host: host.clone(),
                    preset: Some(id.clone()),
                    ..Target::default()
                },
                profile: profile.clone(),
            });
        }
    }
    out
}

/// The shelf to show: the first of `wanted` that exists (the picked one, the remembered one,
/// the default host's), else the first there is.
pub(crate) fn pick<'a>(shelves: &'a [Shelf], wanted: &[&str]) -> Option<&'a Shelf> {
    wanted
        .iter()
        .find_map(|key| shelves.iter().find(|s| s.key == *key))
        .or(shelves.first())
}

/// The shelf the Library rests on: `picked` this run, else the one this device remembers,
/// else the default host's, else the first.
pub(crate) fn resting<'a>(
    shelves: &'a [Shelf],
    picked: &str,
    known: &KnownHosts,
) -> Option<&'a Shelf> {
    let settings = Settings::load();
    let remembered = settings
        .extra
        .get(pf_client_core::settings::LIBRARY_SHELF_KEY)
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let default = pf_client_core::start::default_host(&settings, known)
        .map(|i| shelf_key(&HostTarget::from(&known.hosts[i]), None))
        .unwrap_or_default();
    pick(shelves, &[picked, remembered, &default])
}

#[derive(Clone, PartialEq, Default)]
pub(crate) enum LibraryPhase {
    #[default]
    Loading,
    /// The host did not answer with a catalog and none is remembered. `repair`: this device's
    /// pairing is what it refused.
    Failed {
        why: String,
        repair: bool,
    },
    Ready,
}

/// Why the shelf on screen is the remembered one.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Stale {
    Checking,
    Offline,
    Unpaired,
}

/// The page's whole thread-driven state: the catalog, where it came from, and poster art as
/// it lands (game id → [`Poster`]).
#[derive(Clone, Default)]
pub(crate) struct LibraryState {
    /// The shelf this is for.
    pub(crate) shelf: String,
    pub(crate) phase: LibraryPhase,
    pub(crate) games: Arc<Vec<GameEntry>>,
    pub(crate) stale: Option<Stale>,
    art: Art,
    /// What the host has up (`/status`), by game id: `true` when this device launched it and
    /// may end it.
    running: HashMap<String, bool>,
    /// The profile the shelf plays as, where the box lets this device hide its titles.
    pub(crate) hides: Option<Hides>,
}

/// A profile this device may hide titles for ([`hide::offer`]): its id and its name.
#[derive(Clone, PartialEq)]
pub(crate) struct Hides {
    profile: String,
    name: String,
}

/// Hide for me on the open shelf: what the host took since the shelf loaded, and the hidden
/// titles while they show in its place. Another shelf starts it afresh.
#[derive(Clone, Default)]
pub(crate) struct HideUi {
    /// Hidden from the shelf since it loaded.
    gone: Vec<String>,
    /// Shown again since it loaded, and not on the list it loaded.
    back: Vec<GameEntry>,
    view: Option<HiddenView>,
}

/// The hidden titles in the shelf's place.
#[derive(Clone)]
pub(crate) enum HiddenView {
    Loading,
    Failed(String),
    /// The titles, and each one's cached poster.
    Listed(Arc<Vec<GameEntry>>, Art),
}

/// A change to [`HideUi`], made on the UI thread.
pub(crate) type HideEdit = Box<dyn FnOnce(&mut HideUi) + Send>;

impl HideUi {
    /// The hidden titles show in the shelf's place.
    pub(crate) fn viewing(&self) -> bool {
        self.view.is_some()
    }

    /// Back to the shelf.
    pub(crate) fn close_view(&mut self) {
        self.view = None;
    }

    /// The host hid (`hidden`) or showed `game`.
    fn took(&mut self, game: GameEntry, hidden: bool) {
        if hidden {
            self.back.retain(|g| g.id != game.id);
            self.gone.push(game.id);
            return;
        }
        if let Some(HiddenView::Listed(list, _)) = &mut self.view {
            Arc::make_mut(list).retain(|g| g.id != game.id);
        }
        let len = self.gone.len();
        self.gone.retain(|id| *id != game.id);
        if self.gone.len() == len && !self.back.iter().any(|g| g.id == game.id) {
            self.back.push(game);
        }
    }

    /// `loaded` as it stands now: without what left it, with what came back. The same list
    /// when nothing moved.
    pub(crate) fn shelf(&self, loaded: &Arc<Vec<GameEntry>>) -> Arc<Vec<GameEntry>> {
        if self.gone.is_empty() && self.back.is_empty() {
            return loaded.clone();
        }
        Arc::new(
            (loaded.iter())
                .filter(|g| !self.gone.contains(&g.id))
                .chain(&self.back)
                .cloned()
                .collect(),
        )
    }
}

/// End game's two moments: the title awaiting a yes, and what the host said. Root state: a
/// menu click and a worker thread set it.
#[derive(Clone, PartialEq, Default)]
pub(crate) struct EndGameUi {
    /// `(id, title)` the confirmation asks about.
    pub(crate) ask: Option<(String, String)>,
    pub(crate) said: Option<String>,
}

/// Show the shelf `target` names: a card's Browse library, Start in, a host's page.
pub(crate) fn open_library(svc: &Svc, target: Target) {
    svc.open_shelf
        .call(shelf_key(&target.host, target.preset.as_deref()));
}

/// The host's API as this device reaches `target`'s management lane, with its pin.
fn host_api(ctx: &AppCtx, target: &Target) -> library::HostApi {
    let pin = (target.host.fp_hex.as_deref()).and_then(crate::trust::parse_hex32);
    let mgmt = target.host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT);
    library::HostApi::new(
        &target.host.addr,
        mgmt,
        target.host.port,
        &ctx.identity,
        pin,
    )
}

/// What a fetch that failed says, in a sentence naming the host.
fn failure(e: &LibraryError, host: &str) -> String {
    match e {
        LibraryError::PinMismatch => desktop::desktop_lib_failed_pin(host),
        LibraryError::NotPaired => desktop::desktop_lib_failed_not_paired(host),
        LibraryError::Http(code) => desktop::desktop_lib_failed_http(host, code),
        LibraryError::Unreachable(_) => desktop::desktop_lib_failed_unreachable(host),
    }
}

/// Where a tile's poster loads from.
#[derive(Clone)]
pub(crate) enum Poster {
    /// The shared cache's file, replaced only by rename, so whole or absent.
    File(String),
    /// The fetched bytes, held because the cache could not keep them.
    Bytes(EncodedImage),
}

/// Posters by game id.
pub(crate) type Art = Arc<HashMap<String, Poster>>;

/// The first candidate poster the shared cache holds.
fn cached_art(candidates: &[String]) -> Option<Poster> {
    let path = candidates
        .iter()
        .find_map(|u| pf_client_core::art_cache::path_of(u))?;
    Some(Poster::File(path.display().to_string()))
}

/// Seed `art` with the posters the cache holds, and list the rest to fetch.
fn seed_art(
    games: &[GameEntry],
    base: &str,
    art: &mut HashMap<String, Poster>,
) -> VecDeque<(String, Vec<String>)> {
    let mut jobs = VecDeque::new();
    for g in games {
        if art.contains_key(&g.id) {
            continue;
        }
        let candidates = g.art.poster_candidates(base);
        match cached_art(&candidates) {
            Some(poster) => drop(art.insert(g.id.clone(), poster)),
            None if !candidates.is_empty() => jobs.push_back((g.id.clone(), candidates)),
            None => {}
        }
    }
    jobs
}

/// Load `shelf` off the UI thread, publishing into root state: the remembered catalog first,
/// then the host's answer, what it has up, and posters as the workers stream them in. Returns
/// what to show at once: `keep` while the host answers again (a reload of the shelf on screen),
/// else the loading state. A newer call bumps `Shared::library_gen`, and a superseded worker
/// stops publishing.
pub(crate) fn start_fetch(
    ctx: &Arc<AppCtx>,
    shelf: &Shelf,
    set_library: &AsyncSetState<LibraryState>,
    keep: Option<&LibraryState>,
) -> LibraryState {
    let generation = ctx.shared.library_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let mut state = match keep {
        Some(shown) if shown.phase == LibraryPhase::Ready => LibraryState {
            stale: None,
            ..shown.clone()
        },
        _ => LibraryState {
            shelf: shelf.key.clone(),
            ..LibraryState::default()
        },
    };
    let first = state.clone();
    let (shared, identity, set) = (
        ctx.shared.clone(),
        ctx.identity.clone(),
        set_library.clone(),
    );
    let auto_wake = ctx.settings.lock().unwrap().auto_wake;
    let (target, profile) = (shelf.target.clone(), shelf.profile.clone());
    let api = host_api(ctx, &target);
    std::thread::Builder::new()
        .name("pf-library".into())
        .spawn(move || {
            let publish = |state: &LibraryState| {
                if shared.library_gen.load(Ordering::SeqCst) == generation {
                    set.call(state.clone());
                }
            };
            let host = &target.host;
            let pin = host.fp_hex.as_deref().and_then(crate::trust::parse_hex32);
            let fp = host.fp_hex.as_deref().filter(|f| !f.is_empty());
            let mgmt = host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT);
            let base = library::base_url(&host.addr, mgmt);
            let pick = profile.as_deref();

            // The remembered shelf shows while the host answers.
            if state.games.is_empty() {
                let cached = fp
                    .and_then(|fp| pf_client_core::library_cache::load(fp, pick))
                    .filter(|c| !c.games.is_empty());
                if let Some(cached) = cached {
                    seed_art(&cached.games, &base, Arc::make_mut(&mut state.art));
                    state.games = Arc::new(cached.games);
                    state.stale = Some(Stale::Checking);
                    state.phase = LibraryPhase::Ready;
                    publish(&state);
                }
            }
            // A sleeping host gets a knock while its remembered shelf is on screen.
            if auto_wake && !host.mac.is_empty() {
                crate::wol::wake(&host.mac, host.addr.parse().ok());
            }

            let fetched = library::fetch_games(&host.addr, mgmt, host.port, &identity, pin, pick);
            let (games, served) = match fetched {
                Ok(fetched) => fetched,
                Err(e) => {
                    let repair = matches!(e, LibraryError::PinMismatch | LibraryError::NotPaired);
                    if state.games.is_empty() {
                        state.phase = LibraryPhase::Failed {
                            why: failure(&e, &host.name),
                            repair,
                        };
                    } else {
                        state.stale = Some(match repair {
                            true => Stale::Unpaired,
                            false => Stale::Offline,
                        });
                    }
                    return publish(&state);
                }
            };
            // The connect that launches a title reads its game overrides from here, under the
            // profile the catalog was fetched as.
            if let Some(fp) = fp {
                pf_client_core::library_cache::store(fp, served, &games);
            }
            let jobs = seed_art(&games, &base, Arc::make_mut(&mut state.art));
            state.games = Arc::new(games);
            state.stale = None;
            state.phase = LibraryPhase::Ready;
            publish(&state);

            // After the titles: a slow `/status` must not hold the shelf back.
            state.running.clear();
            for g in library::fetch_running(&host.addr, mgmt, host.port, &identity, pin) {
                if let Some(id) = g.app_id.clone().filter(|_| g.is_up()) {
                    *state.running.entry(id).or_default() |= g.endable;
                }
            }
            // The pick's own enumerate row says whether this device may hide its titles.
            state.hides = pick.and_then(|pick| {
                let listed = pf_client_core::profiles::fetch_enumerate(
                    &host.addr, mgmt, host.port, &identity, pin,
                );
                let listed = listed.ok().flatten();
                let row = hide::offer(listed.as_deref(), Some(pick))?;
                Some(Hides {
                    profile: row.id.clone(),
                    name: row.display_name.clone(),
                })
            });
            publish(&state);

            if jobs.is_empty() {
                return;
            }
            let urls: HashMap<String, Vec<String>> = jobs.iter().cloned().collect();
            let rx = library::spawn_art_fetch(api, jobs);
            // A poster loads from the cache file, so the page holds no bytes; one the cache could
            // not keep shows from the bytes. A publish re-renders the shelf, so the posters that
            // land in one ART_BATCH go out together.
            while let Ok(first) = rx.recv_blocking() {
                std::thread::sleep(ART_BATCH);
                if shared.library_gen.load(Ordering::SeqCst) != generation {
                    return; // superseded: stop touching state
                }
                for (id, bytes) in
                    std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok()))
                {
                    let poster = urls.get(&id).and_then(|c| cached_art(c));
                    let poster = poster.unwrap_or_else(|| Poster::Bytes(EncodedImage::new(bytes)));
                    Arc::make_mut(&mut state.art).insert(id, poster);
                }
                publish(&state);
            }
        })
        .ok();
    first
}

/// Hide (`hidden`) or show `game` for the shelf's profile on a worker. The status line says
/// what the host answered; where it took it, the page and the cached catalog follow.
fn set_hidden(cx: &Rc<RowCx>, game: GameEntry, hidden: bool) {
    let Some(hides) = cx.hides.clone() else {
        return;
    };
    let api = host_api(&cx.svc.ctx, &cx.shelf.target);
    let fp = cx.shelf.target.host.fp_hex.clone();
    let generation = cx.svc.ctx.shared.library_gen.load(Ordering::SeqCst);
    let (svc, set_hide) = (cx.svc.clone(), cx.set_hide.clone());
    let _ = std::thread::Builder::new()
        .name("punktfunk-hide".into())
        .spawn(move || {
            let outcome = hide::set_hidden(&api, &hides.profile, &game.id, hidden);
            let notice = outcome.notice(hidden, &game.title, &hides.name);
            if outcome != hide::Outcome::Done {
                return svc.report(Err(notice));
            }
            if let Some(fp) = fp {
                // A title shown again goes back into the cached catalog as it was.
                let back = (!hidden).then_some(&game);
                let profile = Some(hides.profile.as_str());
                pf_client_core::library_cache::note_hidden(&fp, profile, &game.id, back);
            }
            if svc.ctx.shared.library_gen.load(Ordering::SeqCst) == generation {
                set_hide.call(Box::new(move |h: &mut HideUi| h.took(game, hidden)));
            }
            svc.report(Ok(notice));
        });
}

/// Show what the shelf's profile hides in its place, asked of the host now.
fn open_hidden(svc: &Svc, set_hide: &AsyncSetState<HideEdit>, shelf: &Shelf, hides: &Hides) {
    set_hide.call(Box::new(|h: &mut HideUi| {
        h.view = Some(HiddenView::Loading)
    }));
    let api = host_api(&svc.ctx, &shelf.target);
    let generation = svc.ctx.shared.library_gen.load(Ordering::SeqCst);
    let host = &shelf.target.host;
    let base = library::base_url(
        &host.addr,
        host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
    );
    let (ctx, set_hide, profile) = (svc.ctx.clone(), set_hide.clone(), hides.profile.clone());
    let _ = std::thread::Builder::new()
        .name("punktfunk-hidden".into())
        .spawn(move || {
            let view = match hide::fetch_hidden_from(&api, &profile) {
                Ok(entries) => {
                    let mut art = HashMap::new();
                    seed_art(&entries, &base, &mut art);
                    HiddenView::Listed(Arc::new(entries), Arc::new(art))
                }
                Err(e) => HiddenView::Failed(e.to_string()),
            };
            // A view closed meanwhile stays closed.
            if ctx.shared.library_gen.load(Ordering::SeqCst) == generation {
                set_hide.call(Box::new(move |h: &mut HideUi| {
                    if h.view.is_some() {
                        h.view = Some(view);
                    }
                }));
            }
        });
}

/// What the list shows: the titles its rows index, each one's cached poster, and the rows.
type Listing = (Arc<Vec<GameEntry>>, Art, Vec<Row>);

/// A paired host as the Desktops section shows it.
struct Desk {
    name: String,
    os: String,
    online: bool,
    /// What the host has up; empty when nothing.
    playing: String,
    target: Target,
}

/// What a row's tiles read and act through. One per render, shared by the rows the list
/// builds as they scroll into view.
struct RowCx {
    svc: Svc,
    pal: Palette,
    shelf: Shelf,
    /// The titles the rows index: the shelf, or the hidden ones while they show.
    games: Arc<Vec<GameEntry>>,
    art: Art,
    running: HashMap<String, bool>,
    favorites: Vec<String>,
    hides: Option<Hides>,
    /// The rows are the hidden titles: a click shows one again.
    hidden_view: bool,
    desks: Vec<Desk>,
    /// The width of one grid column.
    tile_w: f64,
    now_ms: u64,
    set_end_game: AsyncSetState<EndGameUi>,
    set_hide: AsyncSetState<HideEdit>,
    set_details: AsyncSetState<Option<String>>,
    bump: AsyncSetState<()>,
}

/// What a row of a title's menu does.
#[derive(Clone, Copy)]
enum TitleAct {
    Play,
    Favorite,
    Hide,
    Show,
    Details,
    CopyLink,
    Shortcut,
    End,
}

impl RowCx {
    /// Stream the shelf's host with no launch: its desktop, or the title it already runs. A
    /// shelf is often a memory of a host that has since gone to sleep, so a MAC wakes it first.
    fn connect(&self) {
        let (svc, target) = (&self.svc, self.shelf.target.clone());
        match target.host.mac.is_empty() {
            true => initiate(&svc.ctx, target, &svc.set_screen, &svc.set_status),
            false => initiate_waking(&svc.ctx, target, &svc.set_screen, &svc.set_status),
        }
    }

    fn play(&self, id: &str) {
        // The desktop is the host, not one of its titles; a title that runs is resumed.
        if id == DESKTOP_ID || self.running.contains_key(id) {
            return self.connect();
        }
        let (svc, target, id) = (&self.svc, self.shelf.target.clone(), id.to_string());
        let (ss, st) = (&svc.set_screen, &svc.set_status);
        match target.host.mac.is_empty() {
            true => initiate_launch(&svc.ctx, target, id, ss, st),
            false => initiate_launch_waking(&svc.ctx, target, id, ss, st),
        }
    }

    /// One title's `punktfunk://` link: this shelf's host with the game's own `launch=`, and
    /// the shelf's preset. `None` when the host has left the store while the page was open.
    fn link(&self, id: &str) -> Option<String> {
        let t = &self.shelf.target;
        pf_client_core::deeplink::saved_host_link(
            &KnownHosts::load(),
            t.host.fp_hex.as_deref(),
            &t.host.addr,
            t.host.port,
            t.preset.as_deref().filter(|p| !p.is_empty()),
            Some(id),
        )
    }

    fn act(self: &Rc<Self>, act: TitleAct, g: &GameEntry) {
        let svc = &self.svc;
        match act {
            TitleAct::Play => self.play(&g.id),
            TitleAct::Favorite => {
                let fp = self.shelf.target.host.fp_hex.clone().unwrap_or_default();
                let mut on = false;
                Settings::update(|s| {
                    on = layout::toggle_favorite(s, &fp, &g.id);
                    true
                });
                svc.note(match on {
                    true => client::lib_favorite_added(&g.title),
                    false => client::lib_favorite_removed(&g.title),
                });
                self.bump.call(());
            }
            TitleAct::Hide => set_hidden(self, g.clone(), true),
            TitleAct::Show => set_hidden(self, g.clone(), false),
            TitleAct::Details => self.set_details.call(Some(g.id.clone())),
            TitleAct::CopyLink => match self.link(&g.id) {
                Some(url) => {
                    pf_client_core::clipboard::set_text(&url);
                    svc.note(client::hostcard_link_copied().to_string());
                }
                None => svc
                    .set_status
                    .call(client::lib_host_not_saved().to_string()),
            },
            TitleAct::Shortcut => {
                let label = client::lib_shortcut_label(&g.title, &self.shelf.label);
                let made = self
                    .link(&g.id)
                    .ok_or_else(|| client::lib_host_not_saved().to_string())
                    .and_then(|url| {
                        crate::deeplink::write_shortcut(&label, &url)
                            .map_err(desktop::desktop_shortcut_failed)
                    });
                match made {
                    Ok(p) => tracing::info!(path = %p.display(), "shortcut written"),
                    Err(sentence) => svc.set_status.call(sentence),
                }
            }
            TitleAct::End => self.set_end_game.call(EndGameUi {
                ask: Some((g.id.clone(), g.title.clone())),
                said: None,
            }),
        }
    }

    /// The rows of a title's menu, from its state now: End game only while the host can end
    /// it, Hide for its profile only where the box lets this device. A hidden title's menu
    /// only brings it back.
    fn menu_rows(&self, g: &GameEntry) -> MenuRows<TitleAct> {
        let mut m = MenuRows::new();
        if let Some(hides) = self.hides.as_ref().filter(|_| self.hidden_view) {
            m.push(client::lib_show_for(&hides.name), TitleAct::Show);
            return m;
        }
        let up = self.running.get(&g.id).copied();
        m.push(
            match up {
                Some(_) => client::lib_resume(),
                None => common::common_play(),
            },
            TitleAct::Play,
        );
        m.push(
            match self.favorites.contains(&g.id) {
                true => client::lib_favorite_remove(),
                false => client::lib_favorite_add(),
            },
            TitleAct::Favorite,
        );
        if let Some(hides) = &self.hides {
            m.push(client::lib_hide_for(&hides.name), TitleAct::Hide);
        }
        m.push(client::lib_title_details(), TitleAct::Details);
        m.separator();
        m.push(client::hostcard_copy_link(), TitleAct::CopyLink);
        m.push(
            desktop::desktop_hostcard_create_shortcut(),
            TitleAct::Shortcut,
        );
        // Only on a title this device launched and the host still runs.
        if up == Some(true) {
            m.separator();
            m.push(client::lib_end_game(), TitleAct::End);
        }
        m
    }

    fn menu(self: &Rc<Self>, g: &GameEntry) -> Menu {
        let (cx, game) = (self.clone(), g.clone());
        self.menu_rows(g)
            .build(self.bump.clone(), move |act: &TitleAct| cx.act(*act, &game))
    }
}

/// A poster's face: the artwork, or what stands in for it. A launcher rarely has poster art,
/// and one drawn like an art-less game reads as "a cover that failed to load": its brand mark
/// IS the poster, and failing that it names its launcher.
fn poster_face(cx: &RowCx, g: Option<&GameEntry>, width: f64) -> View {
    let (pal, height) = (&cx.pal, width * POSTER_RATIO);
    let placeholder = |face: View| -> View {
        Border::new()
            .content(face)
            .background(pal.get(Token::SubtleFill))
            .height(height)
            .into()
    };
    let centred = |label: TextBlock| -> View {
        label
            .foreground(pal.get(Token::SecondaryText))
            .horizontal_alignment(HorizontalAlignment::Center)
            .vertical_alignment(VerticalAlignment::Center)
            .into()
    };
    let Some(g) = g else {
        // The desktop's mark is not a brand, so it comes from the shell's UI icon set.
        return placeholder(centred(
            lucide::mark(library::DESKTOP_ICON).font_size(width * 0.3),
        ));
    };
    let cached = cx.art.get(&g.id).and_then(|poster| {
        let image = match poster {
            Poster::File(path) => Image::new().source_file(path).ok()?,
            Poster::Bytes(bytes) => Image::new().source_data(bytes.clone()),
        };
        Some(image.stretch(Stretch::UniformToFill).height(height))
    });
    if let Some(image) = cached {
        return image.into();
    }
    match super::launcher_icons::image(g.icon_token()) {
        // `Uniform`, not `UniformToFill`: filling a 2:3 frame would crop a mark to a strip.
        Some(mark) => placeholder(
            Image::new()
                .source_data(mark)
                .stretch(Stretch::Uniform)
                .margin(uniform(width * 0.28))
                .into(),
        ),
        None if g.is_launcher() => placeholder(centred(
            text(store_label(&g.store))
                .font_size(18.0)
                .font_weight(FontWeight::SEMI_BOLD),
        )),
        None => placeholder(centred(
            text(initials(&g.title))
                .font_size(28.0)
                .font_weight(FontWeight::SEMI_BOLD),
        )),
    }
}

/// One poster tile `width` wide: the face with its badges, the title under it, and a caption
/// line where the row has one. A button, so it takes focus and Enter; its menu opens on a
/// right-click and from the overflow button, a SIBLING so a click on it never also plays.
/// The overflow sits beside the title: a button's menu opens above it, and from there it
/// covers this tile's cover rather than the page header.
fn poster(cx: &Rc<RowCx>, tile: Tile, caption: Caption, width: f64) -> View {
    let pal = &cx.pal;
    let game = match tile {
        Tile::Game(i) => cx.games.get(i),
        Tile::Desktop => None,
    };
    let title = match game {
        Some(g) => g.title.clone(),
        None => client::lib_desktop().to_string(),
    };
    // The button pads 11 px each side of its content.
    let face_w = width - 22.0;
    let mut layers: Vec<View> = vec![poster_face(cx, game, face_w)];
    let on_art = game.is_some_and(|g| cx.art.contains_key(&g.id));
    if let Some(g) = game {
        // `Pill::Info` rather than a solid accent fill: white-on-bright is unreadable here.
        let kind = match (on_art, g.is_launcher()) {
            (true, _) => Pill::OnArt,
            (false, true) => Pill::Info,
            (false, false) => Pill::Neutral,
        };
        layers.push(
            pill(pal, store_label(&g.store), kind)
                .horizontal_alignment(HorizontalAlignment::Left)
                .vertical_alignment(VerticalAlignment::Top)
                .margin(uniform(6.0))
                .into(),
        );
        if cx.running.contains_key(&g.id) {
            layers.push(
                pill(pal, client::lib_badge_running(), Pill::Good)
                    .horizontal_alignment(HorizontalAlignment::Left)
                    .vertical_alignment(VerticalAlignment::Bottom)
                    .margin(uniform(6.0))
                    .into(),
            );
        }
        if cx.favorites.contains(&g.id) && !cx.hidden_view {
            // Veiled on a placeholder too: white alone vanishes on its light fill.
            layers.push(
                Border::new()
                    .content(lucide::mark("heart").font_size(12.0).foreground(ART_INK))
                    .background(ART_SCRIM)
                    .corner_radius(11.0)
                    .padding(uniform(5.0))
                    .horizontal_alignment(HorizontalAlignment::Right)
                    .vertical_alignment(VerticalAlignment::Bottom)
                    .margin(uniform(6.0))
                    .into(),
            );
        }
    }
    let framed = Border::new()
        .content(Grid::new().children(layers))
        .corner_radius(6.0)
        .border_brush(match game.is_some_and(GameEntry::is_launcher) {
            true => ThemeBrush::Accent,
            false => ThemeBrush::CardStroke,
        })
        .border_thickness(uniform(1.0));
    let mut below: Vec<View> = vec![
        framed.into(),
        text(&title)
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap)
            .text_trimming(TextTrimming::CharacterEllipsis)
            .max_lines(2)
            // Room for the overflow button beside it.
            .margin(edges(
                2.0,
                6.0,
                if game.is_some() { 26.0 } else { 2.0 },
                0.0,
            ))
            .into(),
    ];
    // Every tile of a captioned row keeps the line, so its rows are one height.
    if caption != Caption::None {
        let said = game.and_then(|g| caption.text(g, cx.now_ms));
        below.push(
            muted(pal, said.as_deref().unwrap_or("\u{a0}"))
                .font_size(11.0)
                .text_trimming(TextTrimming::CharacterEllipsis)
                .margin(edges(2.0, 1.0, 2.0, 0.0))
                .into(),
        );
    }
    // What the host sets for the title, one line.
    if let Some(line) = game
        .and_then(|g| g.overrides.as_ref())
        .and_then(library::host_line)
    {
        below.push(
            muted(pal, line)
                .font_size(11.0)
                .text_trimming(TextTrimming::CharacterEllipsis)
                .margin(edges(2.0, 1.0, 2.0, 0.0))
                .into(),
        );
    }
    let click = {
        let (cx, game) = (cx.clone(), game.cloned());
        move || match &game {
            // A hidden title plays nowhere: a click shows it again.
            Some(g) if cx.hidden_view => cx.act(TitleAct::Show, g),
            Some(g) => cx.play(&g.id),
            None => cx.connect(),
        }
    };
    let name = match game {
        Some(_) => client::lib_title_launch_a11y(&title),
        None => client::lib_desktop_stream().to_string(),
    };
    let tile = Button::new()
        .content(vstack(below).margin(edges(0.0, 6.0, 0.0, 4.0)))
        .style(ButtonStyle::Subtle)
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .horizontal_content_alignment(HorizontalAlignment::Stretch)
        .vertical_alignment(VerticalAlignment::Top)
        .automation_name(&name)
        .on_click(click)
        .tooltip(&title);
    let mut layers: Vec<View> = vec![tile];
    if let Some(g) = game {
        let more = client::lib_title_more_a11y(&g.title);
        // Level with the title's first line: the tile's 11 px of padding and the face's
        // border above it.
        let top = face_w * POSTER_RATIO + 12.0;
        layers.push(
            mark_button("ellipsis", &more)
                .style(ButtonStyle::Subtle)
                .horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Top)
                .margin(edges(0.0, top, 2.0, 0.0))
                .menu(cx.menu(g))
                .tooltip(client::lib_title_more()),
        );
    }
    let tile = Grid::new()
        .children(layers)
        .width(width)
        .opacity(if cx.hidden_view { HIDDEN_OPACITY } else { 1.0 });
    match game {
        Some(g) => tile.menu(cx.menu(g)),
        None => tile.into(),
    }
}

/// One Desktops tile: the host's mark and whether it answers, its name, and Desktop or the
/// title it has up. A click streams that host's desktop.
fn desktop_tile(cx: &Rc<RowCx>, i: usize) -> View {
    let pal = &cx.pal;
    let Some(d) = cx.desks.get(i) else {
        return Border::new().into();
    };
    let second = match d.playing.is_empty() {
        true => muted(pal, client::lib_desktop()),
        false => {
            text(client::lib_resume_title(&d.playing)).foreground(pal.get(Token::SystemSuccess))
        }
    };
    let body = Grid::new()
        .children((
            host_tile(pal, &d.name, &d.os, true, 28.0)
                .opacity(if d.online { 1.0 } else { 0.55 })
                .horizontal_alignment(HorizontalAlignment::Left)
                .vertical_alignment(VerticalAlignment::Top),
            presence_dot(pal, d.online)
                .horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Top)
                .margin(edges(0.0, 4.0, 0.0, 0.0)),
            vstack((
                text(&d.name)
                    .font_size(15.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .text_trimming(TextTrimming::CharacterEllipsis),
                second
                    .font_size(12.0)
                    .text_trimming(TextTrimming::CharacterEllipsis),
            ))
            .spacing(2.0)
            .vertical_alignment(VerticalAlignment::Bottom),
        ))
        .height(DESKTOP_HEIGHT - 28.0);
    let click = {
        let (svc, target, online) = (cx.svc.clone(), d.target.clone(), d.online);
        move || hosts::connect_saved(&svc, target.clone(), online)
    };
    card_button(body, &client::lib_desktop_stream_host(&d.name), click)
        .width(DESKTOP_WIDTH)
        .into()
}

/// A band: tiles side by side, scrolling sideways when they outrun the page.
fn band(tiles: Vec<View>, gap: f64) -> View {
    ScrollViewer::new()
        .content(
            hstack(tiles)
                .spacing(gap)
                .margin(edges(0.0, 0.0, 0.0, 10.0)),
        )
        .horizontal_scroll_bar_visibility(ScrollBarVisibility::Auto)
        .vertical_scroll_bar_visibility(ScrollBarVisibility::Disabled)
        .into()
}

/// One row of the list. A poster's button pads itself, so posters sit edge to edge and the
/// gap between two is their paddings.
fn row_view(cx: &Rc<RowCx>, row: &Row) -> View {
    match row {
        Row::Heading(label) => text(label)
            .font_weight(FontWeight::SEMI_BOLD)
            .margin(edges(11.0, 14.0, 0.0, 2.0))
            .into(),
        Row::Desktops(names) => Border::new()
            .content(band(
                (0..names.len()).map(|i| desktop_tile(cx, i)).collect(),
                POSTER_GAP,
            ))
            .margin(edges(11.0, 4.0, 0.0, 0.0))
            .into(),
        Row::Band { tiles, caption } => band(
            tiles
                .iter()
                .map(|t| poster(cx, *t, *caption, BAND_POSTER_WIDTH + 22.0))
                .collect(),
            0.0,
        ),
        Row::Posters { tiles, caption } => hstack(
            tiles
                .iter()
                .map(|t| poster(cx, *t, *caption, cx.tile_w))
                .collect::<Vec<View>>(),
        )
        .into(),
    }
}

/// Each row's key, stable while the row is the same row: a heading by its words, a band by
/// the heading over it, a grid row by its first title. The list keeps a row's tiles through a
/// change around it.
fn row_keys(rows: &[Row], games: &[GameEntry]) -> Vec<String> {
    let mut over = "";
    let first = |tiles: &[Tile]| match tiles.first() {
        Some(Tile::Game(i)) => games.get(*i).map_or("", |g| g.id.as_str()),
        _ => "desktop",
    };
    rows.iter()
        .enumerate()
        .map(|(i, row)| match row {
            Row::Heading(label) => {
                over = label;
                // A band's heading and a grid group's may share their words.
                match rows.get(i + 1) {
                    Some(Row::Posters { .. }) => format!("heading:grid:{label}"),
                    _ => format!("heading:{label}"),
                }
            }
            Row::Desktops(_) => "desktops".to_string(),
            Row::Band { .. } => format!("band:{over}"),
            Row::Posters { tiles, .. } => format!("row:{over}:{}", first(tiles)),
        })
        .collect()
}

/// The shelf chips: one per shelf, the picked one in the accent.
fn shelf_chips(all: &[Shelf], picked: &str, set_shelf: &AsyncSetState<String>) -> View {
    let chips: Vec<View> = all
        .iter()
        .map(|s| {
            let (set, key) = (set_shelf.clone(), s.key.clone());
            let chip = button(text(&s.label)).on_click(move || set.call(key.clone()));
            match s.key == picked {
                true => chip.style(ButtonStyle::Accent).into(),
                false => chip.into(),
            }
        })
        .collect();
    ScrollViewer::new()
        .content(hstack(chips).spacing(8.0))
        .horizontal_scroll_bar_visibility(ScrollBarVisibility::Hidden)
        .vertical_scroll_bar_visibility(ScrollBarVisibility::Disabled)
        .into()
}

/// The groupings the flyout offers, in its order.
const GROUPINGS: [Option<GroupBy>; 3] = [None, Some(GroupBy::Platform), Some(GroupBy::Store)];

/// The Sort and group flyout. Sort is the device's `library_sort`, shared with the console;
/// Group is this run's.
fn arrange_button(shell: &Shell) -> View {
    let (set_sort, set_group) = (shell.set(Msg::LibrarySort), shell.set(Msg::LibraryGroup));
    let sort = choice_group(
        client::lib_sort(),
        SortKey::ALL.iter().map(|k| k.label().to_string()).collect(),
        SortKey::ALL.iter().position(|k| *k == shell.library_sort),
        move |i| {
            let Some(key) = SortKey::ALL.get(i).copied() else {
                return;
            };
            Settings::update(|s| {
                s.library_sort = key.id().to_string();
                true
            });
            set_sort.call(key);
        },
    );
    let group = choice_group(
        client::lib_group(),
        [
            client::lib_group_none(),
            client::lib_group_platform(),
            client::lib_group_store(),
        ]
        .map(String::from)
        .to_vec(),
        GROUPINGS.iter().position(|g| *g == shell.library_group),
        move |i| {
            if let Some(group) = GROUPINGS.get(i).copied() {
                set_group.call(group);
            }
        },
    );
    arrange_flyout(client::lib_sort_group(), vec![sort, group])
}

/// The Customize flyout: the sections this device keeps, each with its switch and its place.
/// Collections is the console's, so it has no row here.
fn customize_button(shell: &Shell, stored: &str) -> View {
    let pal = &shell.pal;
    let sections = layout::sections(stored);
    let bump = shell.set(|()| Msg::LibraryBump);
    let write = move |edited: Vec<(layout::Section, bool)>| {
        let stored = layout::stored_sections(&edited);
        Settings::update(|s| {
            s.library_sections = stored;
            true
        });
        bump.call(());
    };
    let drawn: Vec<usize> = (0..sections.len())
        .filter(|i| sections[*i].0 != layout::Section::Collections)
        .collect();
    let mut body: Vec<View> = vec![text(client::player_library_sections())
        .font_weight(FontWeight::SEMI_BOLD)
        .into()];
    for (at, &i) in drawn.iter().enumerate() {
        let (section, on) = sections[i];
        let toggle = {
            let (write, sections) = (write.clone(), sections.clone());
            move |on: bool| {
                let mut edited = sections.clone();
                edited[i].1 = on;
                write(edited)
            }
        };
        // Up and down trade places with the drawn neighbour.
        let step = |to: Option<&usize>, mark: &str, name: &str| -> View {
            let b = mark_button(mark, name).style(ButtonStyle::Subtle);
            match to {
                Some(&j) => {
                    let (write, sections) = (write.clone(), sections.clone());
                    b.on_click(move || {
                        let mut edited = sections.clone();
                        edited.swap(i, j);
                        write(edited)
                    })
                    .tooltip(name)
                }
                None => b.is_enabled(false).into(),
            }
        };
        let up = step(
            at.checked_sub(1).and_then(|a| drawn.get(a)),
            "chevron-up",
            client::lib_sections_move_up(),
        );
        let down = step(
            drawn.get(at + 1),
            "chevron-down",
            client::lib_sections_move_down(),
        );
        body.push(
            Grid::new()
                .children((
                    text(section.label())
                        .grid_column(0)
                        .vertical_alignment(VerticalAlignment::Center),
                    Border::new()
                        .content(switch(pal, section.label(), on, toggle))
                        .grid_column(1)
                        .vertical_alignment(VerticalAlignment::Center),
                    Border::new().content(up).grid_column(2),
                    Border::new().content(down).grid_column(3),
                ))
                .columns([
                    GridLength::STAR,
                    GridLength::Auto,
                    GridLength::Auto,
                    GridLength::Auto,
                ])
                .column_spacing(8.0)
                .into(),
        );
    }
    body.push(
        button(client::lib_sections_restore())
            .is_enabled(!stored.is_empty())
            .on_click(move || write(Vec::new()))
            .horizontal_alignment(HorizontalAlignment::Left)
            .margin(edges(0.0, 6.0, 0.0, 0.0))
            .into(),
    );
    mark_button("sliders-horizontal", client::lib_customize())
        .flyout_with(
            Flyout::rich(vstack(body).spacing(8.0).width(300.0))
                .placement(FlyoutPlacement::BottomEdgeAlignedRight),
        )
        .tooltip(client::lib_customize())
}

/// The title bar's search box, while the Library shows.
pub(crate) fn search_box(shell: &Shell) -> View {
    let set = shell.set(Msg::LibrarySearch);
    AutoSuggestBox::new()
        .text(&shell.library_search)
        .placeholder_text(client::lib_search_titles())
        .query_icon(Symbol::Find)
        .on_text_changed(move |t: Rc<str>| set.call(t.to_string()))
        .element_ref(&shell.search)
        .width(320.0)
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}

pub(crate) fn library_page(shell: &Shell) -> View {
    let svc = shell.svc();
    let pal = &svc.pal;
    let known = KnownHosts::load();
    let all = shelves(&known, &CardPreset::catalog());
    let state = &shell.library;
    let Some(shelf) = all.iter().find(|s| s.key == state.shelf) else {
        // No paired host, so no shelf: the way to one is the host list.
        let hosts = button(client::lib_no_host_show_hosts()).on_click({
            let sd = svc.set_dest.clone();
            move || sd.call(Dest::Hosts)
        });
        return notice_page(
            pal,
            None,
            client::lib_no_host_title(),
            client::lib_no_host_body(),
            vec![hosts
                .horizontal_alignment(HorizontalAlignment::Center)
                .into()],
        );
    };
    let settings = Settings::load();
    let set_hide = shell.set(Msg::Hide);
    let set_end_game = shell.set(Msg::EndGame);
    let hidden = state.hides.as_ref().zip(shell.hide.view.as_ref());
    let fp = shelf.target.host.fp_hex.as_deref().unwrap_or_default();
    let favorites = layout::favorites(&settings, fp);

    // The grid's columns from the page's width. A poster's button pads 11 px each side, so
    // the row runs that far into the gutter and its faces line up with the page.
    let content_w = (shell.content_width() - 2.0 * GUTTER + 22.0).max(POSTER_MIN_WIDTH);
    let columns = ((content_w / (POSTER_MIN_WIDTH + POSTER_GAP)).floor() as usize).max(2);
    let tile_w = (content_w / columns as f64).floor();

    // The header: the title, then the page's actions.
    let mut actions: Vec<View> = vec![arrange_button(shell)];
    if let Some(hides) = state.hides.as_ref().filter(|_| hidden.is_none()) {
        let (svc, sh, shelf, hides) = (svc.clone(), set_hide.clone(), shelf.clone(), hides.clone());
        let name = client::lib_hidden_for(&hides.name);
        actions.push(
            mark_button("eye-off", &name)
                .on_click(move || open_hidden(&svc, &sh, &shelf, &hides))
                .tooltip(name),
        );
    }
    actions.push(customize_button(shell, &settings.library_sections));
    let reload = shell.set(|()| Msg::Reload);
    actions.push(
        mark_button("refresh-cw", client::lib_reload())
            .on_click({
                let reload = reload.clone();
                move || reload.call(())
            })
            .tooltip(client::lib_reload()),
    );
    let title = match hidden {
        Some((hides, _)) => client::lib_hidden_for(&hides.name),
        None => client::lib_title().to_string(),
    };
    let mut top: Vec<View> = vec![page_header(&title, actions)];
    if all.len() > 1 && hidden.is_none() {
        top.push(shelf_chips(&all, &shelf.key, &shell.set(Msg::Shelf)));
    }
    let bar = |message: &str| -> View {
        InfoBar::new()
            .message(message)
            .severity(InfoBarSeverity::Informational)
            .is_open(true)
            .is_closable(false)
            .into()
    };
    // The shelf is the remembered one: why, and never as an error.
    if let Some(stale) = state.stale.filter(|_| hidden.is_none()) {
        top.push(bar(match stale {
            Stale::Checking => client::lib_stale_checking(),
            Stale::Offline => client::lib_stale_offline(),
            Stale::Unpaired => desktop::desktop_lib_stale_unpaired(),
        }));
    }
    if let Some(said) = &shell.end_game.said {
        top.push(bar(said));
    }

    let pair_again = || -> View {
        let (svc, target) = (svc.clone(), shelf.target.clone());
        icon_button("lock", client::lib_pair_again())
            .style(ButtonStyle::Accent)
            .on_click(move || {
                *svc.ctx.shared.target.lock().unwrap() = target.clone();
                svc.set_status.call(String::new());
                svc.set_screen.call(Screen::Pair);
            })
            .horizontal_alignment(HorizontalAlignment::Center)
            .into()
    };
    let retry = |again: Box<dyn Fn()>| -> View {
        icon_button("refresh-cw", common::common_retry())
            .style(ButtonStyle::Accent)
            .on_click(again)
            .horizontal_alignment(HorizontalAlignment::Center)
            .into()
    };

    // What the rows index, and the rows.
    let search = shell.library_search.trim();
    let nothing = || (Arc::default(), Arc::default(), Vec::new());
    let (games, art, listed): Listing = match (hidden, &state.phase) {
        (Some((hides, HiddenView::Listed(games, art))), _) => {
            let heading = client::lib_hidden_for(&hides.name);
            let listed = rows::hidden(games, shell.library_sort, search, columns, heading);
            (games.clone(), art.clone(), listed)
        }
        (None, LibraryPhase::Ready) => {
            let games = shell.shelf_games.clone();
            let listed = rows::rows(&rows::Layout {
                games: &games,
                sections: &layout::sections(&settings.library_sections),
                favorites: &favorites,
                sort: shell.library_sort,
                group: shell.library_group,
                search,
                columns,
                desktops: (known.hosts.iter())
                    .filter(|k| k.paired)
                    .map(|k| k.name.clone())
                    .collect(),
            });
            (games, state.art.clone(), listed)
        }
        _ => nothing(),
    };
    let has_titles = listed
        .iter()
        .any(|r| matches!(r, Row::Band { .. } | Row::Posters { .. }));

    // What stands in the list's place while there is nothing to list.
    let instead: Option<View> = match (hidden, &state.phase) {
        (Some((_, HiddenView::Loading)), _) | (None, LibraryPhase::Loading) => {
            Some(busy_page(pal, client::lib_loading(), "", Vec::new()))
        }
        (Some((hides, HiddenView::Failed(why))), _) => {
            let (svc, sh, shelf, hides) = (
                svc.clone(),
                set_hide.clone(),
                shelf.clone(),
                (*hides).clone(),
            );
            let again = move || open_hidden(&svc, &sh, &shelf, &hides);
            Some(notice_page(
                pal,
                None,
                client::lib_load_failed(),
                why,
                vec![retry(Box::new(again))],
            ))
        }
        (Some((hides, HiddenView::Listed(..))), _) if listed.is_empty() => Some(notice_page(
            pal,
            None,
            &client::lib_hidden_none(&hides.name),
            "",
            Vec::new(),
        )),
        (None, LibraryPhase::Failed { why, repair }) => {
            let reload = reload.clone();
            Some(notice_page(
                pal,
                None,
                client::lib_load_failed(),
                why,
                vec![match repair {
                    true => pair_again(),
                    false => retry(Box::new(move || reload.call(()))),
                }],
            ))
        }
        _ => None,
    };
    // The shelf listed nothing to play: say which nothing.
    if instead.is_none() && hidden.is_none() && !has_titles {
        let said = match search.is_empty() {
            true => client::lib_empty().to_string(),
            false => client::lib_no_match_body(search),
        };
        top.push(muted(pal, said).text_wrapping(TextWrapping::Wrap).into());
    }
    if state.stale == Some(Stale::Unpaired) && hidden.is_none() {
        top.push(
            Border::new()
                .content(pair_again())
                .horizontal_alignment(HorizontalAlignment::Left)
                .into(),
        );
    }

    let cx = Rc::new(RowCx {
        svc: svc.clone(),
        pal: *pal,
        shelf: shelf.clone(),
        games,
        art,
        running: state.running.clone(),
        favorites,
        hides: state.hides.clone(),
        hidden_view: hidden.is_some(),
        desks: (known.hosts.iter())
            .filter(|k| k.paired)
            .map(|k| {
                let online = shell.probed.get(&k.card_key()).copied().unwrap_or(false);
                Desk {
                    name: k.name.clone(),
                    os: k.os.clone(),
                    online,
                    playing: match online {
                        true => library::now_playing(&k.fp_hex),
                        false => String::new(),
                    },
                    target: Target {
                        host: HostTarget::from(k),
                        ..Target::default()
                    },
                }
            })
            .collect(),
        tile_w,
        now_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        set_end_game: set_end_game.clone(),
        set_hide,
        set_details: shell.set(Msg::Details),
        bump: shell.set(|()| Msg::LibraryBump),
    });

    let body: View = match instead {
        Some(view) => view,
        None => {
            let keys = Rc::new(row_keys(&listed, &cx.games));
            let revision = {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                keys.hash(&mut h);
                h.finish()
            };
            // The sections on show, in order: every row but a grid's.
            let sections = {
                let mut h = std::collections::hash_map::DefaultHasher::new();
                for k in keys.iter().filter(|k| !k.starts_with("row:")) {
                    k.hash(&mut h);
                }
                h.finish()
            };
            let (listed, row_cx, key_of) = (Rc::new(listed), cx.clone(), keys.clone());
            let source = VirtualSource::new(
                revision,
                keys.len(),
                move |i| key_of[i].clone(),
                move |i| row_view(&row_cx, &listed[i]),
            );
            let list = ScrollViewer::new()
                .content(
                    Border::new()
                        .content(ItemsRepeater::new().virtual_source(source))
                        .padding(edges(GUTTER - 11.0, 0.0, GUTTER - 11.0, 32.0)),
                )
                .horizontal_scroll_bar_visibility(ScrollBarVisibility::Disabled);
            // A section that comes, goes or moves mounts a new list. Diffed into the old one, a
            // section that left and came back leaves the repeater in a layout cycle on a long
            // scroll, which crashes the app. A Grid: the scroller needs a bounded height.
            Grid::new()
                .keyed_children([KeyedView::new(format!("list:{sections:x}"), list)])
                .into()
        }
    };
    let details: View =
        match (shell.details.as_ref()).and_then(|id| cx.games.iter().find(|g| &g.id == id)) {
            Some(g) => details_sheet(&cx, g, &known),
            None => Border::new().into(),
        };
    Grid::new()
        .children((
            Border::new()
                .content(vstack(top).spacing(12.0))
                .padding(edges(GUTTER, 20.0, GUTTER, 8.0)),
            Border::new().content(body).grid_row(1),
            Border::new().content(details).grid_row_span(2),
        ))
        .rows([GridLength::Auto, GridLength::STAR])
        .content_dialog(end_game_dialog(
            &svc,
            shelf,
            &shell.end_game,
            set_end_game,
            reload,
        ))
}

/// A title's details: its poster and facts, what it does, and the preset its launches use.
fn details_sheet(cx: &Rc<RowCx>, g: &GameEntry, known: &KnownHosts) -> View {
    let pal = &cx.pal;
    let close = {
        let set = cx.set_details.clone();
        move || set.call(None)
    };
    let fact = |line: String| -> View {
        muted(pal, line)
            .font_size(13.0)
            .text_wrapping(TextWrapping::Wrap)
            .into()
    };
    let mut facts: Vec<View> = vec![text(&g.title)
        .font_size(20.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .text_wrapping(TextWrapping::Wrap)
        .into()];
    let origin: Vec<String> = [
        Some(store_label(&g.store).to_string()),
        g.platform.clone(),
        g.release_year.map(|y| y.to_string()),
    ]
    .into_iter()
    .flatten()
    .filter(|p| !p.is_empty())
    .collect();
    facts.extend(
        [
            (!origin.is_empty()).then(|| origin.join(" \u{b7} ")),
            g.developer.clone(),
            (!g.genres.is_empty()).then(|| g.genres.join(", ")),
            g.overrides.as_ref().and_then(library::host_line),
            (g.stats.as_ref()).and_then(|s| layout::stats_line(s, cx.now_ms)),
        ]
        .into_iter()
        .flatten()
        .map(fact),
    );
    let act = |act: TitleAct, closes: bool| {
        let (cx, game, close) = (cx.clone(), g.clone(), close.clone());
        move || {
            if closes {
                close();
            }
            cx.act(act, &game)
        }
    };
    let up = cx.running.get(&g.id).copied();
    let favorite = match cx.favorites.contains(&g.id) {
        true => (client::lib_favorite_remove(), ButtonStyle::Accent),
        false => (client::lib_favorite_add(), ButtonStyle::Default),
    };
    let mut acts: Vec<View> = vec![
        icon_button(
            "play",
            match up {
                Some(_) => client::lib_resume(),
                None => common::common_play(),
            },
        )
        .style(ButtonStyle::Accent)
        .on_click(act(TitleAct::Play, true))
        .into(),
        mark_button("heart", favorite.0)
            .style(favorite.1)
            .on_click(act(TitleAct::Favorite, false))
            .tooltip(favorite.0),
        mark_button("link", client::hostcard_copy_link())
            .on_click(act(TitleAct::CopyLink, false))
            .tooltip(client::hostcard_copy_link()),
    ];
    if up == Some(true) {
        acts.push(
            button(client::lib_end_game())
                .on_click(act(TitleAct::End, true))
                .into(),
        );
    }

    let mut body: Vec<View> = vec![
        Grid::new()
            .children((
                Border::new()
                    .content(poster_face(cx, Some(g), 132.0))
                    .corner_radius(6.0)
                    .width(132.0)
                    .grid_column(0)
                    .vertical_alignment(VerticalAlignment::Top),
                vstack(facts).spacing(4.0).grid_column(1),
            ))
            .columns([GridLength::Auto, GridLength::STAR])
            .column_spacing(16.0)
            .into(),
        hstack(acts).spacing(8.0).into(),
    ];
    // What a launch of this title streams with, beating the host's binding.
    let presets = CardPreset::catalog();
    let t = &cx.shelf.target.host;
    let record = known.resolve(t.fp_hex.as_deref(), &t.addr, t.port);
    if let Some(k) = record.filter(|_| !presets.is_empty()) {
        let mut names = vec![client::lib_title_preset_same().to_string()];
        names.extend(presets.iter().map(|p| p.name.clone()));
        let bound = k
            .preset_for_game(&g.id)
            .and_then(|id| presets.iter().position(|p| p.id == id))
            .map_or(0, |i| i + 1);
        let ids: Vec<String> = presets.iter().map(|p| p.id.clone()).collect();
        let (svc, who, game) = (cx.svc.clone(), hosts::HostRef::of(k), g.id.clone());
        let picker = ComboBox::new()
            .items_source(names)
            .selected_index(bound)
            .automation_name(client::lib_title_preset_row())
            .on_selection_changed(move |i: Option<usize>| {
                // None is "nothing selected", which an in-place items rebuild raises.
                let Some(i) = i else { return };
                let id = i.checked_sub(1).and_then(|i| ids.get(i)).cloned();
                let r = KnownHosts::update(|known| {
                    if let Some(h) = who.index(known).and_then(|i| known.hosts.get_mut(i)) {
                        h.bind_game_preset(&game, id.as_deref());
                    }
                });
                super::saved(r, &svc.set_status, None);
            });
        body.push(row_card(
            pal,
            vec![row(
                pal,
                client::lib_title_preset_row(),
                Some(client::lib_title_preset_row_hint()),
                picker,
            )],
        ));
    }
    body.push(
        button(common::common_done())
            .on_click(close.clone())
            .horizontal_alignment(HorizontalAlignment::Right)
            .into(),
    );
    sheet(pal, vstack(body).spacing(16.0), 520.0, 1.0, close)
}

/// The End game confirmation, armed by `is_open`. A yes ends the title on a worker thread,
/// says what the host answered, and reloads the shelf.
fn end_game_dialog(
    svc: &Svc,
    shelf: &Shelf,
    end_game: &EndGameUi,
    set_end_game: AsyncSetState<EndGameUi>,
    reload: AsyncSetState<()>,
) -> ContentDialog {
    let pending = end_game.ask.clone();
    let (se, ctx, target) = (set_end_game, svc.ctx.clone(), shelf.target.clone());
    let heading = pending
        .as_ref()
        .map(|(_, title)| client::lib_end_game_confirm_title(title))
        .unwrap_or_default();
    ContentDialog::new()
        .title(heading)
        .content(client::lib_end_game_confirm_body())
        .primary_button_text(client::lib_end_game())
        .close_button_text(common::common_cancel())
        .is_open(pending.is_some())
        .on_closed(move |r: ContentDialogResult| {
            se.call(EndGameUi::default());
            let Some((id, title)) = pending
                .clone()
                .filter(|_| r == ContentDialogResult::Primary)
            else {
                return;
            };
            let (se, ctx, target, reload) =
                (se.clone(), ctx.clone(), target.clone(), reload.clone());
            let _ = std::thread::Builder::new()
                .name("punktfunk-endgame".into())
                .spawn(move || {
                    let host = &target.host;
                    let pin = host.fp_hex.as_deref().and_then(crate::trust::parse_hex32);
                    let mgmt = host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT);
                    let outcome =
                        library::end_game(&host.addr, mgmt, host.port, &ctx.identity, pin, &id);
                    tracing::info!(app = %id, ?outcome, "end game");
                    se.call(EndGameUi {
                        ask: None,
                        said: Some(outcome.notice(&title)),
                    });
                    reload.call(());
                });
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::trust::KnownHost;

    fn host(id: &str, name: &str, paired: bool, pins: &[&str]) -> KnownHost {
        KnownHost {
            id: Some(id.into()),
            name: name.into(),
            addr: format!("{id}.lan"),
            port: 9777,
            fp_hex: match paired {
                true => format!("fp-{id}"),
                false => String::new(),
            },
            paired,
            pinned_presets: pins.iter().map(|p| p.to_string()).collect(),
            ..KnownHost::default()
        }
    }

    fn game(id: &str) -> GameEntry {
        GameEntry {
            install: None,
            overrides: None,
            id: id.into(),
            store: "steam".into(),
            title: id.into(),
            art: Default::default(),
            platform: None,
            developer: None,
            release_year: None,
            genres: Vec::new(),
            role: None,
            icon: None,
            stats: None,
        }
    }

    fn catalog() -> Vec<CardPreset> {
        vec![CardPreset::named("work", "Work")]
    }

    #[test]
    fn every_paired_host_is_a_shelf_with_its_pinned_presets() {
        let known = KnownHosts {
            hosts: vec![
                host("desk", "Desk", true, &["work", "gone"]),
                host("den", "Den", false, &[]),
                host("deck", "Deck", true, &[]),
            ],
        };
        let all = shelves(&known, &catalog());
        let keys: Vec<&str> = all.iter().map(|s| s.key.as_str()).collect();
        // The unpaired host has no library, and a pin whose preset is gone has no shelf.
        assert_eq!(keys, ["desk", "desk/work", "deck"]);
        assert_eq!(all[1].label, "Desk \u{b7} Work");
        assert_eq!(all[1].target.preset.as_deref(), Some("work"));
        assert_eq!(all[0].target.preset, None);
        // A card's Browse library names the same shelf.
        assert_eq!(shelf_key(&all[1].target.host, Some("work")), "desk/work");
        assert_eq!(shelf_key(&all[0].target.host, Some("")), "desk");
    }

    #[test]
    fn the_shelf_shown_is_the_picked_one_then_the_remembered_one() {
        let known = KnownHosts {
            hosts: vec![
                host("desk", "Desk", true, &["work"]),
                host("deck", "Deck", true, &[]),
            ],
        };
        let all = shelves(&known, &catalog());
        let key = |wanted: &[&str]| pick(&all, wanted).map(|s| s.key.as_str());
        assert_eq!(key(&["deck", "desk/work", "desk"]), Some("deck"));
        // A shelf that is gone gives way to the next wish, an empty wish to the next.
        assert_eq!(key(&["gone", "desk/work", "desk"]), Some("desk/work"));
        assert_eq!(key(&["", "", "deck"]), Some("deck"));
        assert_eq!(key(&["", "", ""]), Some("desk"));
        assert!(pick(&[], &["desk"]).is_none());
    }

    #[test]
    fn a_row_keeps_its_key_through_a_change_around_it() {
        let games = [game("a"), game("b"), game("c")];
        let grid = |first: usize| Row::Posters {
            tiles: vec![Tile::Game(first)],
            caption: Caption::None,
        };
        let before = vec![Row::Heading("Games".into()), grid(0), grid(2)];
        let after = vec![
            Row::Heading("Favorites".into()),
            Row::Band {
                tiles: vec![Tile::Game(1)],
                caption: Caption::None,
            },
            Row::Heading("Games".into()),
            grid(0),
            grid(2),
        ];
        let (b, a) = (row_keys(&before, &games), row_keys(&after, &games));
        assert_eq!(b, ["heading:grid:Games", "row:Games:a", "row:Games:c"]);
        assert_eq!(a[2..], b[..], "the grid's rows are the rows they were");
        assert_eq!(a[..2], ["heading:Favorites", "band:Favorites"]);
        // Keys are unique: the list refuses a repeat.
        let mut seen = a.clone();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), a.len());
    }

    #[test]
    fn a_hidden_title_leaves_the_shelf_until_it_is_shown_again() {
        let loaded = Arc::new(vec![game("a"), game("b")]);
        let mut hide = HideUi::default();
        assert!(Arc::ptr_eq(&hide.shelf(&loaded), &loaded), "nothing moved");
        hide.took(game("a"), true);
        let ids =
            |h: &HideUi| -> Vec<String> { h.shelf(&loaded).iter().map(|g| g.id.clone()).collect() };
        assert_eq!(ids(&hide), ["b"]);
        hide.took(game("a"), false);
        assert_eq!(ids(&hide), ["a", "b"]);
        // One hidden before the shelf loaded comes back at its end.
        hide.took(game("z"), false);
        assert_eq!(ids(&hide), ["a", "b", "z"]);
    }
}
