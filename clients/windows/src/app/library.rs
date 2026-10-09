//! The game library page (mouse/keyboard): the target host's library as a responsive
//! poster grid over `pf-client-core::library` — the WinUI counterpart of the GTK shell's
//! `ui_library.rs`, sharing its service layer (mTLS fetch against the host's management
//! API, pre-classified errors, the 3-worker art pipeline) and its four states
//! (loading / error+retry / empty / grid). Reached from a paired host's "…" menu
//! ("Browse library…"); picking a title starts a normal stream carrying `--launch id` —
//! the host launches the app during the connect handshake.
//!
//! Poster bytes land in a small disk cache (`%LOCALAPPDATA%\punktfunk\art-cache`) and the
//! `Image` widget loads `file:///` URIs from it — reactor's `ImageSource` has no
//! from-bytes constructor, and the cache makes revisits (and offline browsing) instant.
//!
//! Re-render discipline: the fetch and the art stream complete on worker threads, so the
//! whole [`LibraryState`] lives in ROOT state (see the app module docs) and arrives here
//! as a prop; `Shared::library_gen` invalidates a superseded fetch exactly like the
//! speed test's generation guard.

use super::connect::{initiate_launch, initiate_waking};
use super::embedded_png::file_uri;
use super::lucide;
use super::style::*;
use super::{AppCtx, Screen, Svc, Target};
use pf_client_core::collate::{self, Collatable, SortKey};
use pf_client_core::library::{self, initials, store_label, DESKTOP_ID};
use pf_client_core::trust::Settings;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use windows_reactor::*;

/// Poster-grid metrics: minimum poster-column width before dropping a column, the gap,
/// and the 2:3 portrait ratio.
const POSTER_MIN_WIDTH: f64 = 150.0;
const POSTER_GAP: f64 = 12.0;
const POSTER_RATIO: f64 = 1.5;

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
    /// The `file:///` URI of this entry's brand mark, already resolved by
    /// [`super::launcher_icons::uri`] — `None` when the entry names no mark or one we don't ship.
    /// Resolved at decode time rather than per render: the shelf re-renders on every art arrival.
    pub(crate) icon_uri: Option<String>,
}

/// Streaming the desktop was the host tile's click, two pages back from a shelf. Built here
/// rather than fetched: it is presentation, never persisted and never cached.
fn desktop_entry() -> Game {
    Game {
        id: DESKTOP_ID.into(),
        title: "Desktop".into(),
        store: String::new(),
        launcher: false,
        platform: None,
        icon_uri: None,
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
/// (game id → `file:///` URI into the disk cache).
#[derive(Clone, PartialEq, Default)]
pub(crate) struct LibraryState {
    pub(crate) phase: LibraryPhase,
    pub(crate) art: HashMap<String, String>,
    /// What the host has up (`/status`), by game id: `true` when this device launched it and
    /// may end it.
    pub(crate) running: HashMap<String, bool>,
}

/// End game's two moments: the title awaiting a yes, and what the host said. Root state, like
/// `HostsProps::forget`: a flyout click and a worker thread set it.
#[derive(Clone, PartialEq, Default)]
pub(crate) struct EndGameUi {
    /// `(id, title)` the confirmation asks about.
    pub(crate) ask: Option<(String, String)>,
    pub(crate) said: Option<String>,
}

/// Props for the library page: the services plus the fetch/art state driving re-render.
#[derive(Clone)]
pub(crate) struct LibraryProps {
    pub(crate) svc: Svc,
    pub(crate) state: LibraryState,
    pub(crate) end_game: EndGameUi,
    pub(crate) set_end_game: AsyncSetState<EndGameUi>,
}

impl PartialEq for LibraryProps {
    fn eq(&self, other: &Self) -> bool {
        self.svc == other.svc && self.state == other.state && self.end_game == other.end_game
    }
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
            let pin = target.fp_hex.as_deref().and_then(crate::trust::parse_hex32);
            let publish = |state: &LibraryState| {
                if shared.library_gen.load(Ordering::SeqCst) == generation {
                    set.call(state.clone());
                }
            };
            let mut state = LibraryState::default();
            let games = match library::fetch_games(
                &target.addr,
                target.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
                &identity,
                pin,
            ) {
                Ok(games) => games,
                Err(e) => {
                    state.phase = LibraryPhase::Failed(e.to_string());
                    return publish(&state);
                }
            };
            if games.is_empty() {
                state.phase = LibraryPhase::Empty;
                return publish(&state);
            }

            // Seed cached posters; queue the art pipeline for the rest.
            let base = library::base_url(
                &target.addr,
                target.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
            );
            let cache = art_cache_dir();
            let mut jobs: VecDeque<(String, Vec<String>)> = VecDeque::new();
            for g in &games {
                match cache.as_deref().and_then(|d| cached_art_uri(d, &g.id)) {
                    Some(uri) => {
                        state.art.insert(g.id.clone(), uri);
                    }
                    None => {
                        let candidates = g.art.poster_candidates(&base);
                        if !candidates.is_empty() {
                            jobs.push_back((g.id.clone(), candidates));
                        }
                    }
                }
            }
            state.phase = LibraryPhase::Ready(
                games
                    .iter()
                    .map(|g| Game {
                        id: g.id.clone(),
                        title: g.title.clone(),
                        store: g.store.clone(),
                        launcher: g.is_launcher(),
                        platform: g.platform.clone(),
                        icon_uri: super::launcher_icons::uri(g.icon_token()),
                    })
                    .collect(),
            );
            publish(&state);

            // After the titles: a slow `/status` must not hold the shelf back.
            let mgmt = target.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT);
            for g in library::fetch_running(&target.addr, mgmt, &identity, pin) {
                if let Some(id) = g.app_id.clone().filter(|_| g.is_up()) {
                    *state.running.entry(id).or_default() |= g.endable;
                }
            }
            if !state.running.is_empty() {
                publish(&state);
            }

            if jobs.is_empty() {
                return;
            }
            let rx = library::spawn_art_fetch(base, identity, pin, jobs);
            while let Ok((id, bytes)) = rx.recv_blocking() {
                if shared.library_gen.load(Ordering::SeqCst) != generation {
                    return; // superseded — stop touching state (and the cache is shared anyway)
                }
                if let Some(uri) = cache.as_deref().and_then(|d| store_art(d, &id, &bytes)) {
                    state.art.insert(id, uri);
                    publish(&state);
                }
            }
        })
        .ok();
}

/// `%LOCALAPPDATA%\punktfunk\art-cache` — poster bytes by game id, so the `Image` widget
/// has a `file:///` URI to load and revisits skip the network entirely. Small (one poster
/// per library entry); no eviction yet.
fn art_cache_dir() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(base).join("punktfunk").join("art-cache"))
}

/// The cache filename for a game id (`steam:570` → `steam_570.img`) — ids are short and
/// store-qualified, so the sanitized form stays unique in practice.
fn art_file(dir: &Path, id: &str) -> PathBuf {
    let safe: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    dir.join(format!("{safe}.img"))
}

fn cached_art_uri(dir: &Path, id: &str) -> Option<String> {
    let p = art_file(dir, id);
    p.exists().then(|| file_uri(&p))
}

fn store_art(dir: &Path, id: &str, bytes: &[u8]) -> Option<String> {
    std::fs::create_dir_all(dir).ok()?;
    let p = art_file(dir, id);
    std::fs::write(&p, bytes).ok()?;
    Some(file_uri(&p))
}

/// The tile overflow's only entry today — the per-GAME half of the pairing the host tiles
/// already offer (design/client-deep-links.md §5 names the library game context menu as an
/// attach point for exactly this).
const MENU_COPY_LINK: &str = "Copy link";
/// Only on a title this device launched and the host still runs.
const MENU_END_GAME: &str = "End game";

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
        target.fp_hex.as_deref(),
        &target.addr,
        target.port,
        target.preset.as_deref().filter(|p| !p.is_empty()),
        Some(game_id),
    )
}

/// A small group label above a tile grid ("Launchers" / "Games"). Only drawn when the page shows
/// both groups — a single unlabelled grid is what every launcher-less library looked like before.
/// The shelf's sort picker. Presentation only, like the console's bar: it writes the shared
/// `library_sort` and re-renders what is already fetched — nothing is refetched, and no other
/// setting on the page is touched, so a load-modify-save of the whole file is honest here.
/// The stored sort. `use_state` evaluates its seed on every render and this page re-renders on
/// each poster that lands, so the file is read once per process; a pick updates this too, since
/// the page remounts on every visit.
static STORED_SORT: std::sync::Mutex<Option<SortKey>> = std::sync::Mutex::new(None);

fn sort_row(current: SortKey, on_pick: impl Fn(SortKey) + 'static) -> Element {
    let names: Vec<String> = SortKey::ALL.iter().map(|k| k.label().to_string()).collect();
    let index = SortKey::ALL.iter().position(|k| *k == current).unwrap_or(0);
    hstack((
        text_block("Sort")
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
        ComboBox::new(names)
            .selected_index(index as i32)
            .on_selection_changed(move |i: i32| {
                // -1 is "nothing selected": not a pick of the first sort.
                let Some(key) = usize::try_from(i).ok().and_then(|i| SortKey::ALL.get(i)) else {
                    return;
                };
                let key = *key;
                let mut settings = Settings::load();
                settings.library_sort = key.id().to_string();
                settings.save();
                *STORED_SORT.lock().unwrap() = Some(key);
                on_pick(key);
            }),
    ))
    .spacing(8.0)
    .margin(edges(2.0, 8.0, 2.0, 2.0))
    .into()
}

fn group_heading(text: &str) -> Element {
    text_block(text)
        .font_size(12.0)
        .semibold()
        .foreground(ThemeRef::SecondaryText)
        .margin(edges(2.0, 8.0, 2.0, 2.0))
        .into()
}

/// One poster tile: the artwork (or a monogram placeholder while it loads) with the store
/// badge overlaid top-left, the title below, tap-to-launch across the whole tile.
fn poster_tile(
    game: &Game,
    art_uri: Option<&str>,
    poster_h: f64,
    on_tap: Box<dyn Fn()>,
    on_copy_link: Box<dyn Fn()>,
    // `Some` while the host runs it; `Some(true)` when this device may end it.
    up: Option<bool>,
    on_end_game: Box<dyn Fn()>,
) -> Element {
    let poster: Element = match art_uri {
        Some(uri) => Image::new_with_uri(uri)
            .stretch(Stretch::UniformToFill)
            .height(poster_h)
            .into(),
        // A launcher rarely has poster art, and an art-less launcher drawn like an art-less game
        // reads as "a game whose cover failed to load". Its brand mark, when we ship one, IS the
        // poster; failing that it names its launcher. Either way the frame below picks up the
        // accent stroke.
        //
        // `Uniform`, not `UniformToFill`: the marks keep their masters' aspect ratios (Steam is
        // 496x512, Playnite 1024x1024), and filling a 2:3 frame would crop them to a strip — the
        // very thing that kept launcher tiles art-less in the first place.
        None => match game.icon_uri.as_deref() {
            Some(uri) => border(
                Image::new_with_uri(uri)
                    .stretch(Stretch::Uniform)
                    .margin(uniform(poster_h * 0.28)),
            )
            .background(ThemeRef::SubtleFill)
            .height(poster_h)
            .into(),
            // The desktop tile's mark is not a brand, so it comes from the shell's UI icon set
            // rather than `launcher_icons` — which here means Lucide's font, the one form this
            // shell can size and tint (see `lucide`).
            None if game.id == DESKTOP_ID => border(
                text_block(lucide::glyph(library::DESKTOP_ICON))
                    .font_family(lucide::FAMILY)
                    .font_size(poster_h * 0.3)
                    .foreground(ThemeRef::SecondaryText)
                    .horizontal_alignment(HorizontalAlignment::Center)
                    .vertical_alignment(VerticalAlignment::Center),
            )
            .background(ThemeRef::SubtleFill)
            .height(poster_h)
            .into(),
            None => border(
                text_block(if game.launcher {
                    store_label(&game.store).to_string()
                } else {
                    initials(&game.title)
                })
                .font_size(if game.launcher { 18.0 } else { 28.0 })
                .semibold()
                .foreground(ThemeRef::SecondaryText)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center),
            )
            .background(ThemeRef::SubtleFill)
            .height(poster_h)
            .into(),
        },
    };
    let mut layers = vec![
        poster,
        // `Pill::Info` rather than a solid accent fill — `style.rs` is explicit that
        // white-on-bright is unreadable here.
        pill(
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
            pill("Running", Pill::Neutral)
                .horizontal_alignment(HorizontalAlignment::Left)
                .vertical_alignment(VerticalAlignment::Bottom)
                .margin(uniform(6.0))
                .into(),
        );
    }
    let framed = border(grid(layers))
        .corner_radius(8.0)
        .border_brush(if game.launcher {
            ThemeRef::Accent
        } else {
            ThemeRef::CardStroke
        })
        .border_thickness(uniform(1.0));

    let tappable = border(
        vstack((
            framed,
            text_block(&game.title)
                .font_size(12.0)
                .wrap()
                .margin(edges(2.0, 6.0, 2.0, 0.0)),
        ))
        .spacing(0.0),
    )
    .background(hit_test_backstop())
    .on_tapped(on_tap);

    // This entry's own actions, opposite the store badge. A button rather than a right-click
    // context flyout because the reactor hangs `menu_flyout` off buttons only — and a menu a
    // mouse user cannot see is one they never find.
    //
    // A SIBLING of the tappable area, not a child of it: `host_tile` on the hosts page splits
    // the two exactly this way, and that split is what keeps a click on the overflow from also
    // launching the title underneath it.
    grid(vec![
        tappable.into(),
        button("")
            .icon(lucide::icon("ellipsis"))
            .subtle()
            .tooltip("More options")
            .automation_name(format!("More options for {}", game.title))
            .menu_flyout(if up == Some(true) {
                vec![menu_item(MENU_COPY_LINK), menu_item(MENU_END_GAME)]
            } else {
                vec![menu_item(MENU_COPY_LINK)]
            })
            .on_item_clicked(move |item: String| match item.as_str() {
                MENU_COPY_LINK => on_copy_link(),
                MENU_END_GAME => on_end_game(),
                _ => {}
            })
            .horizontal_alignment(HorizontalAlignment::Right)
            .vertical_alignment(VerticalAlignment::Top)
            .margin(edges(0.0, 4.0, 4.0, 0.0))
            .into(),
    ])
    .into()
}

pub(crate) fn library_page(props: &LibraryProps, cx: &mut RenderCx) -> Element {
    let ctx = &props.svc.ctx;
    let target = ctx.shared.target.lock().unwrap().clone();
    let (ss, st) = (props.svc.set_screen.clone(), props.svc.set_status.clone());

    // Responsive poster columns from the live window width (the hosts page's pattern).
    let window = cx.use_inner_size();
    // Shared `library_sort`: the same four orders the console's bar and the GTK dialog offer.
    let seed = *STORED_SORT
        .lock()
        .unwrap()
        .get_or_insert_with(|| SortKey::parse(&Settings::load().library_sort));
    let (sort, set_sort) = cx.use_state(seed);
    let content_w = (window.width - 64.0).clamp(POSTER_MIN_WIDTH, 1120.0);
    let cols =
        (((content_w + POSTER_GAP) / (POSTER_MIN_WIDTH + POSTER_GAP)).floor() as usize).clamp(2, 6);
    let tile_w = (content_w - POSTER_GAP * (cols as f64 - 1.0)) / cols as f64;
    let poster_h = tile_w * POSTER_RATIO;

    let back_btn = button("Back").icon(lucide::icon("arrow-left")).on_click({
        let (ss, se) = (ss.clone(), props.set_end_game.clone());
        move || {
            se.call(EndGameUi::default());
            ss.call(Screen::Hosts)
        }
    });
    let title = if target.name.is_empty() {
        "Game library".to_string()
    } else {
        format!("Game library \u{00B7} {}", target.name)
    };
    let mut body: Vec<Element> = vec![page_header(&title, back_btn)];
    if let Some(said) = &props.end_game.said {
        body.push(
            InfoBar::new("End game")
                .message(said.clone())
                .is_closable(false)
                .into(),
        );
    }

    match &props.state.phase {
        LibraryPhase::Loading => body.push(
            card(
                hstack((
                    ProgressRing::indeterminate().width(18.0).height(18.0),
                    text_block("Loading the library\u{2026}").foreground(ThemeRef::SecondaryText),
                ))
                .spacing(12.0),
            )
            .into(),
        ),
        LibraryPhase::Failed(msg) => {
            body.push(
                InfoBar::new("Couldn't load the library")
                    .message(msg.clone())
                    .error()
                    .is_closable(false)
                    .into(),
            );
            let (ctx2, set_library) = (ctx.clone(), props.svc.set_library.clone());
            body.push(
                button("Retry")
                    .accent()
                    .icon(lucide::icon("refresh-cw"))
                    .on_click(move || start_fetch(&ctx2, &set_library))
                    .horizontal_alignment(HorizontalAlignment::Left)
                    .into(),
            );
        }
        LibraryPhase::Empty => body.push(
            card(
                text_block(
                    "No games yet \u{2014} add titles to the host's library (its console or web \
                     UI) and they appear here.",
                )
                .wrap()
                .foreground(ThemeRef::SecondaryText),
            )
            .into(),
        ),
        LibraryPhase::Ready(games) => {
            let tile = |g: &Game| -> Element {
                let (ctx2, ss, st) = (ctx.clone(), ss.clone(), st.clone());
                let (target, id) = (target.clone(), g.id.clone());
                let (se, ask) = (props.set_end_game.clone(), (g.id.clone(), g.title.clone()));
                let (link_target, link_id) = (target.clone(), id.clone());
                // The desktop tile is the host, not one of its titles: it streams with no
                // launch id, and wakes first because it is often the first dial of the day.
                let desktop = g.id == DESKTOP_ID;
                poster_tile(
                    g,
                    props.state.art.get(&g.id).map(String::as_str),
                    poster_h,
                    Box::new(move || {
                        if desktop {
                            initiate_waking(&ctx2, target.clone(), &ss, &st);
                        } else {
                            initiate_launch(&ctx2, target.clone(), id.clone(), &ss, &st);
                        }
                    }),
                    // Silent on success, exactly like the host tile's "Copy link" on the
                    // hosts page — this shell has no toast, and the two must not disagree
                    // about what copying a link looks like.
                    Box::new(move || match game_link(&link_target, &link_id) {
                        Some(url) => pf_client_core::clipboard::set_text(&url),
                        None => tracing::warn!(id = %link_id, "no saved host to build a link from"),
                    }),
                    props.state.running.get(&g.id).copied(),
                    Box::new(move || {
                        se.call(EndGameUi {
                            ask: Some(ask.clone()),
                            said: None,
                        })
                    }),
                )
            };
            // Design D4: launcher entries get their own shelf above the titles, never
            // interleaved. `collate` is the shared policy — launchers lead as their own group
            // and the sort applies inside a group, never across. Headings appear only when
            // both groups exist, so a launcher-less library renders as it did before.
            let groups = collate::collate(&games[..], sort, None);
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
            {
                body.push(sort_row(sort, move |k| set_sort.call(k)));
                body.push(group_heading(if leading.len() == 1 {
                    "Host"
                } else {
                    "Launchers"
                }));
                body.push(tile_grid(
                    leading.iter().map(|g| tile(g)).collect(),
                    cols,
                    POSTER_GAP,
                ));
            }
            if !titles.is_empty() {
                {
                    body.push(group_heading("Games"));
                }
                body.push(tile_grid(
                    titles.iter().map(|g| tile(g)).collect(),
                    cols,
                    POSTER_GAP,
                ));
            }
        }
    }

    // ALWAYS MOUNTED in a stable trailing slot, `is_open` arming it — the hosts page's forget
    // confirmation, for the same reactor reason.
    let end_confirm: Element = {
        let pending = props.end_game.ask.clone();
        let (se, ctx2, set_library) = (
            props.set_end_game.clone(),
            ctx.clone(),
            props.svc.set_library.clone(),
        );
        let content = pending
            .as_ref()
            .map(|(_, title)| {
                format!("End {title} on the host? Unsaved progress in the game is lost.")
            })
            .unwrap_or_default();
        ContentDialog::new("End game?")
            .content(content)
            .primary_button_text("End game")
            .close_button_text("Cancel")
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
                        let pin = target.fp_hex.as_deref().and_then(crate::trust::parse_hex32);
                        let mgmt = target.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT);
                        let outcome =
                            library::end_game(&target.addr, mgmt, &ctx2.identity, pin, &id);
                        tracing::info!(app = %id, ?outcome, "end game");
                        se.call(EndGameUi {
                            ask: None,
                            said: Some(outcome.notice(&title)),
                        });
                        start_fetch(&ctx2, &set_library);
                    });
            })
            .into()
    };

    grid(vec![page_wide(body), end_confirm]).into()
}
