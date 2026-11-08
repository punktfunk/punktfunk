//! The game library page (mouse/keyboard): the target host's library as a responsive
//! poster grid over `pf-client-core::library` — the WinUI counterpart of the GTK shell's
//! `ui_library.rs`, sharing its service layer (mTLS fetch against the host's management
//! API, pre-classified errors, the 3-worker art pipeline) and its four states
//! (loading / error+retry / empty / grid). Reached from a paired host's "…" menu
//! ("Browse library…"); picking a title starts a normal stream carrying `--launch id` —
//! the host launches the app during the connect handshake.
//!
//! Poster bytes land in a small disk cache (`%LOCALAPPDATA%\punktfunk\art-cache`), which the
//! `Image` loads them from: revisits (and offline browsing) are instant.
//!
//! The fetch and the art stream complete on worker threads into the root's [`LibraryState`];
//! `Shared::library_gen` invalidates a superseded fetch exactly like the speed test's
//! generation guard.

use super::connect::{initiate_launch, initiate_waking};
use super::lucide;
use super::style::*;
use super::{AppCtx, AsyncSetState, Msg, Screen, Shell, Svc, Target};
use pf_client_core::collate::{self, Collatable, SortKey};
use pf_client_core::library::{self, initials, store_label, DESKTOP_ID};
use pf_client_core::trust::Settings;
use pf_fluent::{Palette, Token};
use pf_l10n::{client, common};
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
    /// This entry's brand mark ([`super::launcher_icons::image`]) — `None` when the entry
    /// names no mark or one we don't ship. Resolved at decode time rather than per render: the
    /// shelf re-renders on every art arrival.
    pub(crate) icon: Option<EncodedImage>,
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
            let games = match library::fetch_games(
                &target.host.addr,
                target.host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
                target.host.port,
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
            // The connect that launches a title reads its game overrides from here.
            if let Some(fp) = target.host.fp_hex.as_deref() {
                pf_client_core::library_cache::store(fp, &games);
            }

            // Seed cached posters; queue the art pipeline for the rest.
            let base = library::base_url(
                &target.host.addr,
                target.host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
            );
            let cache = art_cache_dir();
            let mut jobs: VecDeque<(String, Vec<String>)> = VecDeque::new();
            for g in &games {
                match cache.as_deref().and_then(|d| cached_art(d, &g.id)) {
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
                        icon: super::launcher_icons::image(g.icon_token()),
                    })
                    .collect(),
            );
            publish(&state);

            // After the titles: a slow `/status` must not hold the shelf back.
            let mgmt = target.host.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT);
            let port = target.host.port;
            for g in library::fetch_running(&target.host.addr, mgmt, port, &identity, pin) {
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
            let api = library::HostApi::new(&target.host.addr, mgmt, port, &identity, pin);
            let rx = library::spawn_art_fetch(api, jobs);
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

/// `%LOCALAPPDATA%\punktfunk\art-cache` — poster bytes by game id, so revisits skip the
/// network entirely. Small (one poster per library entry); no eviction yet.
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

fn cached_art(dir: &Path, id: &str) -> Option<String> {
    let p = art_file(dir, id);
    p.exists().then(|| p.display().to_string())
}

fn store_art(dir: &Path, id: &str, bytes: &[u8]) -> Option<String> {
    std::fs::create_dir_all(dir).ok()?;
    let p = art_file(dir, id);
    std::fs::write(&p, bytes).ok()?;
    Some(p.display().to_string())
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

/// One poster tile: the artwork (or a monogram placeholder while it loads) with the store
/// badge overlaid top-left, the title below, tap-to-launch across the whole tile.
#[allow(clippy::too_many_arguments)]
fn poster_tile(
    pal: &Palette,
    game: &Game,
    art: Option<&str>,
    poster_h: f64,
    tap: Box<dyn Fn()>,
    on_copy_link: Box<dyn Fn()>,
    // `Some` while the host runs it; `Some(true)` when this device may end it.
    up: Option<bool>,
    on_end_game: Box<dyn Fn()>,
) -> View {
    let placeholder = |face: View| -> View {
        Border::new()
            .content(face)
            .background(pal.get(Token::SubtleFill))
            .height(poster_h)
            .into()
    };
    let cached = art.and_then(|path| {
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

    let tappable = on_tap(
        Border::new()
            .content(vstack((
                framed,
                text(&game.title)
                    .font_size(12.0)
                    .text_wrapping(TextWrapping::Wrap)
                    .margin(edges(2.0, 6.0, 2.0, 0.0)),
            )))
            .background(hit_test_backstop()),
        tap,
    );

    // This entry's own actions, opposite the store badge: a button, because a menu a mouse user
    // cannot see is one they never find. A SIBLING of the tappable area, as on the host tiles,
    // so a click on the overflow never also launches the title underneath it.
    let mut items = vec![MenuItem::item("copy", client::hostcard_copy_link())];
    // Only on a title this device launched and the host still runs.
    if up == Some(true) {
        items.push(MenuItem::item("end", client::lib_end_game()));
    }
    let menu = Menu::new(items, move |key: Key| match key.as_str() {
        Some("copy") => on_copy_link(),
        Some("end") => on_end_game(),
        _ => {}
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
        .into()
}

pub(crate) fn library_page(shell: &Shell) -> View {
    let svc = shell.svc();
    let (ctx, pal) = (&svc.ctx, &svc.pal);
    let state = &shell.library;
    let target = ctx.shared.target.lock().unwrap().clone();
    let (ss, st) = (svc.set_screen.clone(), svc.set_status.clone());
    let set_end_game = shell.set(Msg::EndGame);

    // Responsive poster columns from the window width (the hosts page's pattern).
    let content_w = (shell.width - 64.0).clamp(POSTER_MIN_WIDTH, 1120.0);
    let cols =
        (((content_w + POSTER_GAP) / (POSTER_MIN_WIDTH + POSTER_GAP)).floor() as usize).clamp(2, 6);
    let tile_w = (content_w - POSTER_GAP * (cols as f64 - 1.0)) / cols as f64;
    let poster_h = tile_w * POSTER_RATIO;

    let back_btn = icon_button("arrow-left", common::common_back()).on_click({
        let (ss, se) = (ss.clone(), set_end_game.clone());
        move || {
            se.call(EndGameUi::default());
            ss.call(Screen::Hosts)
        }
    });
    let title = if target.host.name.is_empty() {
        client::lib_title().to_string()
    } else {
        client::lib_title_host(&target.host.name)
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

    match &state.phase {
        LibraryPhase::Loading => body.push(
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
            .into(),
        ),
        LibraryPhase::Failed(msg) => {
            body.push(
                InfoBar::new()
                    .title(client::lib_load_failed())
                    .message(msg)
                    .severity(InfoBarSeverity::Error)
                    .is_open(true)
                    .is_closable(false)
                    .into(),
            );
            let (ctx2, set_library) = (ctx.clone(), svc.set_library.clone());
            body.push(
                icon_button("refresh-cw", common::common_retry())
                    .style(ButtonStyle::Accent)
                    .on_click(move || start_fetch(&ctx2, &set_library))
                    .horizontal_alignment(HorizontalAlignment::Left)
                    .into(),
            );
        }
        LibraryPhase::Empty => body
            .push(card(muted(pal, client::lib_empty()).text_wrapping(TextWrapping::Wrap)).into()),
        LibraryPhase::Ready(games) => {
            let tile = |g: &Game| -> View {
                let (ctx2, ss, st) = (ctx.clone(), ss.clone(), st.clone());
                let (target, id) = (target.clone(), g.id.clone());
                let (se, ask) = (set_end_game.clone(), (g.id.clone(), g.title.clone()));
                let (link_target, link_id) = (target.clone(), id.clone());
                // The desktop tile is the host, not one of its titles: it streams with no
                // launch id, and wakes first because it is often the first dial of the day.
                let desktop = g.id == DESKTOP_ID;
                poster_tile(
                    pal,
                    g,
                    state.art.get(&g.id).map(String::as_str),
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
                    state.running.get(&g.id).copied(),
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
            body.push(sort_row(
                pal,
                shell.library_sort,
                shell.set(Msg::LibrarySort),
            ));
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
    }

    page_wide(body).content_dialog(end_game_dialog(&svc, &shell.end_game, set_end_game))
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
