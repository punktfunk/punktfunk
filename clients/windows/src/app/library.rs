//! The game library page (mouse/keyboard): the target host's library as a responsive
//! poster grid over `pf-client-core::library` — the WinUI counterpart of the GTK shell's
//! `ui_library.rs`, sharing its service layer (mTLS fetch against the host's management
//! API, pre-classified errors, the 3-worker art pipeline) and its four states
//! (loading / error+retry / empty / grid). Reached from a paired host's "…" menu
//! ("Browse library…"); picking a title starts a normal stream carrying `--launch id` —
//! the host launches the app during the connect handshake.
//!
//! Poster bytes land in the shared `pf_client_core::art_cache` (keyed by poster URL, bounded),
//! which the `Image` loads them from: revisits (and offline browsing) are instant.
//!
//! The fetch and the art stream complete on worker threads into the root's [`LibraryState`];
//! `Shared::library_gen` invalidates a superseded fetch exactly like the speed test's
//! generation guard.

use super::connect::{initiate_launch, initiate_waking};
use super::lucide;
use super::style::*;
use super::{AppCtx, AsyncSetState, Msg, Screen, Shell, Svc, Target};
use pf_client_core::collate::{self, Collatable, SortKey};
use pf_client_core::hide;
use pf_client_core::library::{self, initials, store_label, GameEntry, DESKTOP_ID};
use pf_client_core::trust::Settings;
use pf_fluent::{Palette, Token};
use pf_l10n::{client, common};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use windows_reactor::*;

/// Poster-grid metrics: minimum poster-column width before dropping a column, the gap,
/// and the 2:3 portrait ratio.
const POSTER_MIN_WIDTH: f64 = 150.0;
const POSTER_GAP: f64 = 12.0;
const POSTER_RATIO: f64 = 1.5;
/// How long arriving posters gather before the shelf is told: about ten renders a second at
/// most, however fast the workers stream.
const ART_BATCH: std::time::Duration = std::time::Duration::from_millis(100);

/// One game, as the grid renders it (the wire `GameEntry` minus the artwork paths, which
/// resolve into `LibraryState::art`).
#[derive(Clone, PartialEq)]
pub(crate) struct Game {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) store: String,
    /// This entry opens the launcher itself (Steam Big Picture, Heroic) rather than a title —
    /// design D4. Reduced from the wire's `role` by `GameEntry::is_launcher`, so "anything that
    /// isn't `launcher` is a game" is decided in one place for every client.
    pub(crate) launcher: bool,
    /// Host free-form display string (`"PC"`, `"PS2"`, …). Carried so this shelf can sort and
    /// bucket by it like every other client — dropping it here was why Platform sorted flat.
    pub(crate) platform: Option<String>,
    /// This entry's brand mark ([`super::launcher_icons::image`]) — `None` when the entry
    /// names no mark or one we don't ship. Resolved at decode time rather than per render: the
    /// shelf re-renders on every art arrival.
    pub(crate) icon: Option<EncodedImage>,
    /// [`library::host_line`] for the entry's overrides; `None` when the host sets nothing.
    pub(crate) host: Option<String>,
}

/// Streaming the desktop was the host tile's click, two pages back from a shelf. Built here
/// rather than fetched: it is presentation, never persisted and never cached.
fn desktop_entry() -> Game {
    Game {
        id: DESKTOP_ID.into(),
        title: client::lib_desktop().into(),
        store: String::new(),
        launcher: false,
        platform: None,
        icon: None,
        host: None,
    }
}

/// This shelf sorts its own reduced model; the policy is `pf_client_core::collate`.
impl Collatable for Game {
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
}

#[derive(Clone, PartialEq, Default)]
pub(crate) enum LibraryPhase {
    #[default]
    Loading,
    Failed(String),
    Empty,
    Ready(Vec<Game>),
}

/// The page's whole thread-driven state: the fetch phase plus poster art as it lands
/// (game id → its file in the disk cache).
#[derive(Clone, PartialEq, Default)]
pub(crate) struct LibraryState {
    pub(crate) phase: LibraryPhase,
    pub(crate) art: HashMap<String, String>,
    /// What the host has up (`/status`), by game id: `true` when this device launched it and
    /// may end it.
    pub(crate) running: HashMap<String, bool>,
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
/// titles while they show in its place. Every fetch starts it afresh.
#[derive(Clone, PartialEq, Default)]
pub(crate) struct HideUi {
    /// Hidden from the shelf since it loaded.
    gone: Vec<String>,
    /// Shown again since it loaded, and not on the list it loaded.
    back: Vec<Game>,
    view: Option<HiddenView>,
}

/// The hidden titles in the shelf's place.
#[derive(Clone, PartialEq)]
pub(crate) enum HiddenView {
    Loading,
    Failed(String),
    Listed(Vec<Hidden>),
}

/// One hidden title: as the grid draws it, its cached poster, and the entry the cached catalog
/// takes back when it is shown again.
#[derive(Clone)]
pub(crate) struct Hidden {
    game: Game,
    art: Option<String>,
    entry: GameEntry,
}

impl PartialEq for Hidden {
    fn eq(&self, other: &Self) -> bool {
        (&self.game, &self.art) == (&other.game, &other.art)
    }
}

/// A change to [`HideUi`], made on the UI thread.
pub(crate) type HideEdit = Box<dyn FnOnce(&mut HideUi) + Send>;

impl HideUi {
    /// The host hid (`hidden`) or showed `game`.
    fn took(&mut self, game: Game, hidden: bool) {
        if hidden {
            self.back.retain(|g| g.id != game.id);
            self.gone.push(game.id);
            return;
        }
        if let Some(HiddenView::Listed(list)) = &mut self.view {
            list.retain(|h| h.game.id != game.id);
        }
        let len = self.gone.len();
        self.gone.retain(|id| *id != game.id);
        if self.gone.len() == len && !self.back.iter().any(|g| g.id == game.id) {
            self.back.push(game);
        }
    }

    /// `loaded` as it stands now: without what left it, with what came back.
    fn shelf(&self, loaded: &[Game]) -> Vec<Game> {
        (loaded.iter())
            .filter(|g| !self.gone.contains(&g.id))
            .chain(&self.back)
            .cloned()
            .collect()
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

/// Show `target`'s library. The target becomes `Shared::target`, which the grid launches
/// through, so a pinned card's preset rides along; then the fetch starts and the screen shows.
pub(crate) fn open_library(svc: &Svc, target: Target) {
    *svc.ctx.shared.target.lock().unwrap() = target;
    start_fetch(&svc.ctx, &svc.set_library);
    svc.set_screen.call(Screen::Library);
}

/// Fetch the library for `Shared::target` off the UI thread, publishing into root state:
/// phase first, then art entries as the workers stream them in. A newer call (re-open,
/// Retry, another host) bumps `Shared::library_gen`, and a superseded worker stops
/// publishing — the speed test's generation pattern.
pub(crate) fn start_fetch(ctx: &Arc<AppCtx>, set_library: &AsyncSetState<LibraryState>) {
    let target = ctx.shared.target.lock().unwrap().clone();
    let generation = ctx.shared.library_gen.fetch_add(1, Ordering::SeqCst) + 1;
    set_library.call(LibraryState::default()); // Loading, no art
    let (shared, identity, set) = (
        ctx.shared.clone(),
        ctx.identity.clone(),
        set_library.clone(),
    );
    std::thread::Builder::new()
        .name("pf-library".into())
        .spawn(move || {
            let pin = target
                .host
                .fp_hex
                .as_deref()
                .and_then(crate::trust::parse_hex32);
            let publish = |state: &LibraryState| {
                if shared.library_gen.load(Ordering::SeqCst) == generation {
                    set.call(state.clone());
                }
            };
            let mut state = LibraryState::default();
            drop_per_id_posters();
            let known = pf_client_core::trust::KnownHosts::load();
            let saved = known.resolve(
                target.host.fp_hex.as_deref(),
                &target.host.addr,
                target.host.port,
            );
            let pick = saved
                .and_then(|h| h.profile.as_ref())
                .map(|p| p.id.as_str());
            let mgmt = target.host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT);
            let port = target.host.port;
            let (games, served) =
                match library::fetch_games(&target.host.addr, mgmt, port, &identity, pin, pick) {
                    Ok(fetched) => fetched,
                    Err(e) => {
                        state.phase = LibraryPhase::Failed(e.to_string());
                        return publish(&state);
                    }
                };
            // The pick's own enumerate row says whether this device may hide its titles.
            let hides = || -> Option<Hides> {
                let addr = &target.host.addr;
                let listed =
                    pf_client_core::profiles::fetch_enumerate(addr, mgmt, port, &identity, pin);
                let listed = listed.ok().flatten();
                let row = hide::offer(listed.as_deref(), pick)?;
                Some(Hides {
                    profile: row.id.clone(),
                    name: row.display_name.clone(),
                })
            };
            if games.is_empty() {
                state.phase = LibraryPhase::Empty;
                state.hides = pick.and_then(|_| hides());
                return publish(&state);
            }
            // The connect that launches a title reads its game overrides from here, under the
            // profile the catalog was fetched as.
            if let Some(fp) = target.host.fp_hex.as_deref() {
                pf_client_core::library_cache::store(fp, served, &games);
            }

            // Seed cached posters; queue the art pipeline for the rest.
            let base = library::base_url(&target.host.addr, mgmt);
            let mut jobs: VecDeque<(String, Vec<String>)> = VecDeque::new();
            for g in &games {
                let candidates = g.art.poster_candidates(&base);
                match cached_art(&candidates) {
                    Some(path) => {
                        state.art.insert(g.id.clone(), path);
                    }
                    None if !candidates.is_empty() => jobs.push_back((g.id.clone(), candidates)),
                    None => {}
                }
            }
            state.phase = LibraryPhase::Ready(games.iter().map(game_of).collect());
            publish(&state);

            // After the titles: a slow `/status` must not hold the shelf back.
            for g in library::fetch_running(&target.host.addr, mgmt, port, &identity, pin) {
                if let Some(id) = g.app_id.clone().filter(|_| g.is_up()) {
                    *state.running.entry(id).or_default() |= g.endable;
                }
            }
            state.hides = pick.and_then(|_| hides());
            if !state.running.is_empty() || state.hides.is_some() {
                publish(&state);
            }

            if jobs.is_empty() {
                return;
            }
            let api = library::HostApi::new(&target.host.addr, mgmt, port, &identity, pin);
            let urls: HashMap<String, Vec<String>> = jobs.iter().cloned().collect();
            let rx = library::spawn_art_fetch(api, jobs);
            // The workers have written the winning poster to the cache; the bytes are not needed.
            // A publish clones the state and re-renders the shelf, so the posters that land in
            // one ART_BATCH go out together.
            while let Ok(first) = rx.recv_blocking() {
                std::thread::sleep(ART_BATCH);
                if shared.library_gen.load(Ordering::SeqCst) != generation {
                    return; // superseded — stop touching state (and the cache is shared anyway)
                }
                let mut landed = false;
                for (id, _) in
                    std::iter::once(first).chain(std::iter::from_fn(|| rx.try_recv().ok()))
                {
                    if let Some(path) = urls.get(&id).and_then(|c| cached_art(c)) {
                        state.art.insert(id, path);
                        landed = true;
                    }
                }
                if landed {
                    publish(&state);
                }
            }
        })
        .ok();
}

/// Removes `%LOCALAPPDATA%\punktfunk\art-cache`, whose per-id posters nothing reads now.
fn drop_per_id_posters() {
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let _ = std::fs::remove_dir_all(std::path::Path::new(&local).join("punktfunk/art-cache"));
    }
}

/// A catalog entry as the grid draws it.
fn game_of(g: &GameEntry) -> Game {
    Game {
        id: g.id.clone(),
        title: g.title.clone(),
        store: g.store.clone(),
        launcher: g.is_launcher(),
        platform: g.platform.clone(),
        icon: super::launcher_icons::image(g.icon_token()),
        host: g.overrides.as_ref().and_then(library::host_line),
    }
}

/// The open shelf's host as this device reaches its management lane, its pin, and the
/// shelf's generation: a worker's answer for an older shelf changes nothing.
fn shelf_api(ctx: &AppCtx) -> (library::HostApi, Option<String>, u64) {
    let target = ctx.shared.target.lock().unwrap().clone();
    let pin = (target.host.fp_hex.as_deref()).and_then(crate::trust::parse_hex32);
    let mgmt = target.host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT);
    let (addr, port) = (&target.host.addr, target.host.port);
    let api = library::HostApi::new(addr, mgmt, port, &ctx.identity, pin);
    let generation = ctx.shared.library_gen.load(Ordering::SeqCst);
    (api, target.host.fp_hex, generation)
}

/// Hide (`hidden`) or show `game` for the shelf's profile on a worker. The status line says
/// what the host answered; where it took it, the page and the cached catalog follow.
fn set_hidden(
    svc: &Svc,
    set_hide: &AsyncSetState<HideEdit>,
    hides: &Hides,
    game: Game,
    entry: Option<GameEntry>,
    hidden: bool,
) {
    let (api, fp, generation) = shelf_api(&svc.ctx);
    let (svc, set_hide, hides) = (svc.clone(), set_hide.clone(), hides.clone());
    let _ = std::thread::Builder::new()
        .name("punktfunk-hide".into())
        .spawn(move || {
            let outcome = hide::set_hidden(&api, &hides.profile, &game.id, hidden);
            let notice = outcome.notice(hidden, &game.title, &hides.name);
            if outcome != hide::Outcome::Done {
                return svc.report(Err(notice));
            }
            if let Some(fp) = fp {
                let (id, back) = (&game.id, entry.as_ref());
                pf_client_core::library_cache::note_hidden(&fp, Some(&hides.profile), id, back);
            }
            if svc.ctx.shared.library_gen.load(Ordering::SeqCst) == generation {
                set_hide.call(Box::new(move |h: &mut HideUi| h.took(game, hidden)));
            }
            svc.report(Ok(notice));
        });
}

/// Show what the shelf's profile hides in its place, asked of the host now.
fn open_hidden(svc: &Svc, set_hide: &AsyncSetState<HideEdit>, hides: &Hides) {
    set_hide.call(Box::new(|h: &mut HideUi| {
        h.view = Some(HiddenView::Loading)
    }));
    let (api, _, generation) = shelf_api(&svc.ctx);
    let base = {
        let target = svc.ctx.shared.target.lock().unwrap();
        let mgmt = target.host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT);
        library::base_url(&target.host.addr, mgmt)
    };
    let (ctx, set_hide, profile) = (svc.ctx.clone(), set_hide.clone(), hides.profile.clone());
    let _ = std::thread::Builder::new()
        .name("punktfunk-hidden".into())
        .spawn(move || {
            let view = match hide::fetch_hidden_from(&api, &profile) {
                Ok(entries) => HiddenView::Listed(
                    (entries.into_iter())
                        .map(|entry| Hidden {
                            game: game_of(&entry),
                            art: cached_art(&entry.art.poster_candidates(&base)),
                            entry,
                        })
                        .collect(),
                ),
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

/// The first candidate poster the shared cache holds, as the path `Image` loads. The file is
/// replaced only by rename, so it is whole or absent.
fn cached_art(candidates: &[String]) -> Option<String> {
    let path = candidates
        .iter()
        .find_map(|u| pf_client_core::art_cache::path_of(u))?;
    Some(path.display().to_string())
}

/// The per-GAME half of the link the host tiles offer (design/client-deep-links.md §5 names
/// the library game menu as an attach point for exactly this).
///
/// One title's self-emitted `punktfunk://` link: this page's host with the game's own
/// `launch=` id attached, so the URL boots straight into that title rather than the desktop.
/// Built from the STORE — the stable id and the fingerprint live there, not on the target —
/// which is what makes a link taken here identical to one taken off the host tile.
///
/// A library opened from a "Connect with" one-off carries that preset into the link, so a
/// copied URL streams the way the shelf it came from does. `None` only when the host has
/// left the store while the page was open.
fn game_link(target: &super::Target, game_id: &str) -> Option<String> {
    pf_client_core::deeplink::saved_host_link(
        &crate::trust::KnownHosts::load(),
        target.host.fp_hex.as_deref(),
        &target.host.addr,
        target.host.port,
        target.preset.as_deref().filter(|p| !p.is_empty()),
        Some(game_id),
    )
}

/// The shelf's sort picker. Presentation only, like the console's bar: it writes the shared
/// `library_sort` and re-renders what is already fetched — nothing is refetched, and no other
/// setting on the page is touched, so a load-modify-save of the whole file is honest here.
fn sort_row(pal: &Palette, current: SortKey, set_sort: AsyncSetState<SortKey>) -> View {
    let names: Vec<String> = SortKey::ALL.iter().map(|k| k.label().to_string()).collect();
    let index = SortKey::ALL.iter().position(|k| *k == current).unwrap_or(0);
    hstack((
        muted(pal, client::lib_sort())
            .font_size(12.0)
            .vertical_alignment(VerticalAlignment::Center),
        ComboBox::new()
            .items_source(names)
            .selected_index(index)
            .on_selection_changed(move |i: Option<usize>| {
                let Some(key) = i.and_then(|i| SortKey::ALL.get(i)).copied() else {
                    return;
                };
                Settings::update(|s| {
                    s.library_sort = key.id().to_string();
                    true
                });
                set_sort.call(key);
            }),
    ))
    .spacing(8.0)
    .margin(edges(2.0, 8.0, 2.0, 2.0))
    .into()
}

/// A small group label above a tile grid ("Launchers" / "Games").
fn group_heading(pal: &Palette, label: &str) -> View {
    muted(pal, label)
        .font_size(12.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .margin(edges(2.0, 8.0, 2.0, 2.0))
        .into()
}

/// One row of a title's overflow menu: its key, its words, what it does.
type MenuRow = (&'static str, String, Box<dyn Fn()>);

/// One poster tile: the artwork (or a monogram placeholder while it loads) with the store
/// badge overlaid top-left, the title below, then one line of what the host sets for it,
/// tap across the whole tile, and its `menu` behind the overflow button. A hidden title's
/// tile is dimmed (`dim`).
fn poster_tile(
    pal: &Palette,
    game: &Game,
    (art, poster_h): (Option<&str>, f64),
    tap: Box<dyn Fn()>,
    // `Some` while the host runs it.
    up: Option<bool>,
    menu: Vec<MenuRow>,
    dim: bool,
) -> View {
    let placeholder = |face: View| -> View {
        Border::new()
            .content(face)
            .background(pal.get(Token::SubtleFill))
            .height(poster_h)
            .into()
    };
    let cached = art.and_then(|path: &str| {
        Image::new()
            .source_file(path)
            .ok()
            .map(|i| i.stretch(Stretch::UniformToFill).height(poster_h))
    });
    let poster: View = match cached {
        Some(image) => image.into(),
        // A launcher rarely has poster art, and an art-less launcher drawn like an art-less game
        // reads as "a game whose cover failed to load". Its brand mark, when we ship one, IS the
        // poster; failing that it names its launcher. Either way the frame picks up the accent
        // stroke. `Uniform`, not `UniformToFill`: filling a 2:3 frame would crop a mark to a strip.
        None => match &game.icon {
            Some(mark) => placeholder(
                Image::new()
                    .source_data(mark.clone())
                    .stretch(Stretch::Uniform)
                    .margin(uniform(poster_h * 0.28))
                    .into(),
            ),
            // The desktop tile's mark is not a brand, so it comes from the shell's UI icon set.
            None if game.id == DESKTOP_ID => placeholder(
                lucide::mark(library::DESKTOP_ICON)
                    .font_size(poster_h * 0.3)
                    .foreground(pal.get(Token::SecondaryText))
                    .horizontal_alignment(HorizontalAlignment::Center)
                    .vertical_alignment(VerticalAlignment::Center)
                    .into(),
            ),
            None => placeholder(
                muted(
                    pal,
                    if game.launcher {
                        store_label(&game.store).to_string()
                    } else {
                        initials(&game.title)
                    },
                )
                .font_size(if game.launcher { 18.0 } else { 28.0 })
                .font_weight(FontWeight::SEMI_BOLD)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
            ),
        },
    };
    let mut layers: Vec<View> = vec![
        poster,
        // `Pill::Info` rather than a solid accent fill: white-on-bright is unreadable here.
        pill(
            pal,
            store_label(&game.store),
            if game.launcher {
                Pill::Info
            } else {
                Pill::Neutral
            },
        )
        .horizontal_alignment(HorizontalAlignment::Left)
        .vertical_alignment(VerticalAlignment::Top)
        .margin(uniform(6.0))
        .into(),
    ];
    if up.is_some() {
        layers.push(
            pill(pal, client::lib_badge_running(), Pill::Neutral)
                .horizontal_alignment(HorizontalAlignment::Left)
                .vertical_alignment(VerticalAlignment::Bottom)
                .margin(uniform(6.0))
                .into(),
        );
    }
    let framed = Border::new()
        .content(Grid::new().children(layers))
        .corner_radius(8.0)
        .border_brush(if game.launcher {
            ThemeBrush::Accent
        } else {
            ThemeBrush::CardStroke
        })
        .border_thickness(uniform(1.0));

    let mut below: Vec<View> = vec![
        framed.into(),
        text(&game.title)
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap)
            .margin(edges(2.0, 6.0, 2.0, 0.0))
            .into(),
    ];
    if let Some(line) = &game.host {
        below.push(
            muted(pal, line)
                .font_size(11.0)
                .text_trimming(TextTrimming::CharacterEllipsis)
                .margin(edges(2.0, 2.0, 2.0, 0.0))
                .into(),
        );
    }
    let tappable = on_tap(
        Border::new()
            .content(vstack(below))
            .background(hit_test_backstop()),
        tap,
    );

    // This entry's own actions, opposite the store badge: a button, because a menu a mouse user
    // cannot see is one they never find. A SIBLING of the tappable area, as on the host tiles,
    // so a click on the overflow never also launches the title underneath it.
    let items: Vec<MenuItem> = (menu.iter())
        .map(|(key, label, _)| MenuItem::item(*key, label.as_str()))
        .collect();
    let menu = Menu::new(items, move |key: Key| {
        if let Some((_, _, run)) = menu.iter().find(|(k, _, _)| key.as_str() == Some(*k)) {
            run();
        }
    });
    let more = client::lib_title_more_a11y(&game.title);
    Grid::new()
        .children((
            tappable,
            mark_button("ellipsis", &more)
                .style(ButtonStyle::Subtle)
                .horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Top)
                .margin(edges(0.0, 4.0, 4.0, 0.0))
                .menu(menu)
                .tooltip(client::lib_title_more()),
        ))
        .opacity(if dim { HIDDEN_OPACITY } else { 1.0 })
        .into()
}

/// A hidden title's tile: there to bring back, not to play.
const HIDDEN_OPACITY: f64 = 0.55;

pub(crate) fn library_page(shell: &Shell) -> View {
    let svc = shell.svc();
    let (ctx, pal) = (&svc.ctx, &svc.pal);
    let state = &shell.library;
    let target = ctx.shared.target.lock().unwrap().clone();
    let (ss, st) = (svc.set_screen.clone(), svc.set_status.clone());
    let set_end_game = shell.set(Msg::EndGame);
    let set_hide = shell.set(Msg::Hide);
    // What the shelf's profile hides, while it shows in the shelf's place.
    let hidden = state.hides.as_ref().zip(shell.hide.view.as_ref());

    // Responsive poster columns from the window width (the hosts page's pattern).
    let content_w = (shell.width - 64.0).clamp(POSTER_MIN_WIDTH, 1120.0);
    let cols =
        (((content_w + POSTER_GAP) / (POSTER_MIN_WIDTH + POSTER_GAP)).floor() as usize).clamp(2, 6);
    let tile_w = (content_w - POSTER_GAP * (cols as f64 - 1.0)) / cols as f64;
    let poster_h = tile_w * POSTER_RATIO;

    // Back from the hidden titles is the shelf; back from the shelf, the hosts.
    let back_btn = icon_button("arrow-left", common::common_back()).on_click({
        let (ss, se, sh) = (ss.clone(), set_end_game.clone(), set_hide.clone());
        let viewing = hidden.is_some();
        move || {
            if viewing {
                return sh.call(Box::new(|h: &mut HideUi| h.view = None));
            }
            se.call(EndGameUi::default());
            ss.call(Screen::Hosts)
        }
    });
    let title = match hidden {
        Some((hides, _)) => client::lib_hidden_for(&hides.name),
        None if target.host.name.is_empty() => client::lib_title().to_string(),
        None => client::lib_title_host(&target.host.name),
    };
    // Opens the hidden titles, where this device may hide them.
    let hidden_button = |hides: &Hides| -> View {
        let (svc, sh, hides) = (svc.clone(), set_hide.clone(), hides.clone());
        icon_button("eye-off", text(client::lib_hidden_for(&hides.name)))
            .style(ButtonStyle::Subtle)
            .on_click(move || open_hidden(&svc, &sh, &hides))
            .into()
    };
    let mut body: Vec<View> = vec![page_header(&title, back_btn)];
    if let Some(said) = &shell.end_game.said {
        body.push(
            InfoBar::new()
                .title(client::lib_end_game())
                .message(said)
                .is_open(true)
                .is_closable(false)
                .into(),
        );
    }

    let shelf = match &state.phase {
        LibraryPhase::Ready(loaded) => Some(shell.hide.shelf(loaded)),
        // Everything hidden, then one shown again: the shelf has that one.
        LibraryPhase::Empty if !shell.hide.back.is_empty() => Some(shell.hide.shelf(&[])),
        _ => None,
    };
    match (hidden, &state.phase, shelf) {
        (Some((hides, view)), _, _) => {
            hidden_view(&svc, &set_hide, hides, view, (cols, poster_h), &mut body)
        }
        (None, _, Some(games)) => {
            let tile = |g: &Game| -> View {
                let (ctx2, ss, st) = (ctx.clone(), ss.clone(), st.clone());
                let (target, id) = (target.clone(), g.id.clone());
                let (se, ask) = (set_end_game.clone(), (g.id.clone(), g.title.clone()));
                let (link_target, link_id) = (target.clone(), id.clone());
                // The desktop tile is the host, not one of its titles: it streams with no
                // launch id, and wakes first because it is often the first dial of the day.
                let desktop = g.id == DESKTOP_ID;
                let up = state.running.get(&g.id).copied();
                // Silent on success, exactly like the host tile's "Copy link" on the hosts
                // page: the two must not disagree about what copying a link looks like.
                let copy = move || match game_link(&link_target, &link_id) {
                    Some(url) => pf_client_core::clipboard::set_text(&url),
                    None => tracing::warn!(id = %link_id, "no saved host to build a link from"),
                };
                let mut menu: Vec<MenuRow> = Vec::new();
                if let Some(hides) = state.hides.as_ref().filter(|_| !desktop) {
                    let label = client::lib_hide_for(&hides.name);
                    let (svc, sh, hides, game) =
                        (svc.clone(), set_hide.clone(), hides.clone(), g.clone());
                    let hide = move || set_hidden(&svc, &sh, &hides, game.clone(), None, true);
                    menu.push(("hide", label, Box::new(hide)));
                }
                menu.push(("copy", client::hostcard_copy_link().into(), Box::new(copy)));
                // Only on a title this device launched and the host still runs.
                if up == Some(true) {
                    let end = move || {
                        se.call(EndGameUi {
                            ask: Some(ask.clone()),
                            said: None,
                        })
                    };
                    menu.push(("end", client::lib_end_game().into(), Box::new(end)));
                }
                let play = move || {
                    if desktop {
                        initiate_waking(&ctx2, target.clone(), &ss, &st);
                    } else {
                        initiate_launch(&ctx2, target.clone(), id.clone(), &ss, &st);
                    }
                };
                let art = state.art.get(&g.id).map(String::as_str);
                poster_tile(pal, g, (art, poster_h), Box::new(play), up, menu, false)
            };
            // Design D4: launcher entries get their own shelf above the titles, never
            // interleaved. `collate` is the shared policy — launchers lead as their own group
            // and the sort applies inside a group, never across.
            let groups = collate::collate(&games[..], shell.library_sort, None);
            let of_group = |key: collate::GroupKey| -> Vec<&Game> {
                groups
                    .iter()
                    .find(|g| g.key == key)
                    .map(|g| g.games.iter().map(|&i| &games[i]).collect())
                    .unwrap_or_default()
            };
            let launchers = of_group(collate::GroupKey::Launchers);
            // Ungrouped collation puts every title in one bucket whose label is never drawn.
            let titles = of_group(collate::GroupKey::Platform("All".to_string()));
            // The desktop leads the launcher band: both open something rather than play a
            // title, and it means the shelf is never a dead end for the desktop-only user.
            let desktop = desktop_entry();
            let leading: Vec<&Game> = std::iter::once(&desktop).chain(launchers).collect();
            let sort = sort_row(pal, shell.library_sort, shell.set(Msg::LibrarySort));
            body.push(match state.hides.as_ref() {
                Some(hides) => hstack((sort, hidden_button(hides))).spacing(12.0).into(),
                None => sort,
            });
            body.push(group_heading(
                pal,
                if leading.len() == 1 {
                    client::lib_heading_host()
                } else {
                    client::lib_section_launchers()
                },
            ));
            body.push(tile_grid(
                leading.iter().map(|g| tile(g)).collect(),
                cols,
                POSTER_GAP,
            ));
            if !titles.is_empty() {
                body.push(group_heading(pal, client::lib_section_games()));
                body.push(tile_grid(
                    titles.iter().map(|g| tile(g)).collect(),
                    cols,
                    POSTER_GAP,
                ));
            }
        }
        (None, LibraryPhase::Loading, _) => body.push(loading(pal)),
        (None, LibraryPhase::Failed(msg), _) => {
            let (ctx2, set_library) = (ctx.clone(), svc.set_library.clone());
            failed(msg, move || start_fetch(&ctx2, &set_library), &mut body);
        }
        (None, _, _) => {
            body.push(note(pal, client::lib_empty()));
            // Everything hidden still leaves the way back.
            body.extend(state.hides.as_ref().map(hidden_button));
        }
    }

    page_wide(body).content_dialog(end_game_dialog(&svc, &shell.end_game, set_end_game))
}

/// The hidden titles in the shelf's place: dimmed, and a click on one shows it again.
fn hidden_view(
    svc: &Svc,
    set_hide: &AsyncSetState<HideEdit>,
    hides: &Hides,
    view: &HiddenView,
    (cols, poster_h): (usize, f64),
    body: &mut Vec<View>,
) {
    let pal = &svc.pal;
    match view {
        HiddenView::Loading => body.push(loading(pal)),
        HiddenView::Failed(msg) => {
            let (svc, sh, hides) = (svc.clone(), set_hide.clone(), hides.clone());
            failed(msg, move || open_hidden(&svc, &sh, &hides), body);
        }
        HiddenView::Listed(list) if list.is_empty() => {
            body.push(note(pal, &client::lib_hidden_none(&hides.name)))
        }
        HiddenView::Listed(list) => {
            let tile = |h: &Hidden| -> View {
                let (svc, sh, hides) = (svc.clone(), set_hide.clone(), hides.clone());
                let (game, entry, label) = (
                    h.game.clone(),
                    h.entry.clone(),
                    client::lib_show_for(&hides.name),
                );
                let show =
                    move || set_hidden(&svc, &sh, &hides, game.clone(), Some(entry.clone()), false);
                let menu: Vec<MenuRow> = vec![("show", label, Box::new(show.clone()))];
                let art = (h.art.as_deref(), poster_h);
                poster_tile(pal, &h.game, art, Box::new(show), None, menu, true)
            };
            body.push(tile_grid(list.iter().map(tile).collect(), cols, POSTER_GAP));
        }
    }
}

fn loading(pal: &Palette) -> View {
    card(
        hstack((
            ProgressRing::new()
                .is_indeterminate(true)
                .width(18.0)
                .height(18.0),
            muted(pal, client::lib_loading()),
        ))
        .spacing(12.0),
    )
    .into()
}

fn note(pal: &Palette, line: &str) -> View {
    card(muted(pal, line).text_wrapping(TextWrapping::Wrap)).into()
}

/// A list that did not load: the cause, and Retry.
fn failed(msg: &str, retry: impl Fn() + 'static, body: &mut Vec<View>) {
    body.push(
        InfoBar::new()
            .title(client::lib_load_failed())
            .message(msg)
            .severity(InfoBarSeverity::Error)
            .is_open(true)
            .is_closable(false)
            .into(),
    );
    body.push(
        icon_button("refresh-cw", common::common_retry())
            .style(ButtonStyle::Accent)
            .on_click(retry)
            .horizontal_alignment(HorizontalAlignment::Left)
            .into(),
    );
}

/// The End game confirmation, armed by `is_open`. A yes ends the title on a worker thread,
/// says what the host answered, and refetches the shelf.
fn end_game_dialog(
    svc: &Svc,
    end_game: &EndGameUi,
    set_end_game: AsyncSetState<EndGameUi>,
) -> ContentDialog {
    let pending = end_game.ask.clone();
    let (se, ctx2, set_library) = (set_end_game, svc.ctx.clone(), svc.set_library.clone());
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
            let (se, ctx2, set_library) = (se.clone(), ctx2.clone(), set_library.clone());
            let _ = std::thread::Builder::new()
                .name("punktfunk-endgame".into())
                .spawn(move || {
                    let target = ctx2.shared.target.lock().unwrap().clone();
                    let pin = target
                        .host
                        .fp_hex
                        .as_deref()
                        .and_then(crate::trust::parse_hex32);
                    let mgmt = target.host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT);
                    let (addr, port) = (&target.host.addr, target.host.port);
                    let outcome = library::end_game(addr, mgmt, port, &ctx2.identity, pin, &id);
                    tracing::info!(app = %id, ?outcome, "end game");
                    se.call(EndGameUi {
                        ask: None,
                        said: Some(outcome.notice(&title)),
                    });
                    start_fetch(&ctx2, &set_library);
                });
        })
}
