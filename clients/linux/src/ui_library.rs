//! The game-library page (the Apple `LibraryView` ported): a poster grid of the host's
//! unified library fetched over the management API (`library.rs`), pushed onto the nav
//! stack from a saved card's "Browse library…" action. Poster art loads asynchronously
//! (worker threads → texture on the main loop) with a monogram placeholder, and tapping
//! a title starts a session that asks the host to launch it (the library id rides the
//! Hello via `ConnectRequest::launch`).

use crate::app::{AppModel, AppMsg};
use crate::library::{self, GameEntry};
use crate::trust;
use crate::ui_hosts::ConnectRequest;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use pf_client_core::collate::{self, SortKey};
use pf_client_core::library::{initials, store_label, DESKTOP_ICON, DESKTOP_ID};
use relm4::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

/// Poster bytes as they arrive from the fetch threads, keyed by entry id.
type ArtRx = async_channel::Receiver<(String, Vec<u8>)>;

/// Everything the page re-renders from. Kept alive by the widget closures (reload/retry/
/// card activation); dropped when the page is popped, which also winds down any in-flight
/// art consumer (its weak upgrade fails).
struct State {
    sender: ComponentSender<AppModel>,
    identity: (String, String),
    /// The advertised mgmt port when the host was live at open time (else the default).
    mgmt_port: u16,
    /// The host this library belongs to — cards clone it and add `launch`.
    req: ConnectRequest,
    stack: gtk::Stack,
    flow: gtk::FlowBox,
    /// Launcher entries (design D4) get their own shelf above the games, so a handful of ways to
    /// open a launcher aren't buried in a 400-title grid. Hidden outright when there are none.
    launcher_flow: gtk::FlowBox,
    launchers_group: gtk::Box,
    /// The "Games" heading — only earns its space once a Launchers shelf is above it.
    games_heading: gtk::Label,
    error_page: adw::StatusPage,
    /// Says the shelf is a memory, not the host's answer. Hidden by a live catalog.
    banner: adw::Banner,
    /// Per-page poster cache (entry id → texture) — a Retry re-renders without refetching.
    art: RefCell<HashMap<String, gdk::Texture>>,
    /// The Picture each entry currently renders into (rebuilt per render), so async art
    /// results land on the right card.
    pics: RefCell<HashMap<String, gtk::Picture>>,
    /// Screenshot mode: render injected entries only, never touch the network.
    mock: Cell<bool>,
    /// The snapshot on screen, so changing the sort re-renders without refetching.
    games: RefCell<Vec<GameEntry>>,
    /// Shared `library_sort` (`pf_client_core::collate`), so the console and this page
    /// order one library the same way.
    sort: Cell<SortKey>,
    /// The art channel, kept so dropping this page CLOSES it. The consuming future parks on
    /// `recv()` and holds its own handle, so on an all-miss run nothing ever wakes it and the
    /// fetch threads would carry on against a page nobody can see.
    art_rx: RefCell<Option<ArtRx>>,
    /// What the host has up, by library id (`/status`). An endable row puts End game on the tile.
    running: RefCell<HashMap<String, library::RunningGame>>,
    /// Bumped by every [`load`]. A fetch whose generation is stale when it lands is dropped:
    /// Reload can be pressed again while one is in flight, and results arrive in whatever
    /// order the two hosts answer, not the order they were asked.
    generation: Cell<u64>,
}

impl Drop for State {
    /// Close the art channel: the fetch threads watch it to know whether anyone is still
    /// looking. Nothing else here needs teardown.
    fn drop(&mut self) {
        if let Some(rx) = self.art_rx.borrow().as_ref() {
            rx.close();
        }
    }
}

/// What the page calls the host it is browsing. A request that carries a one-off preset
/// came from a PINNED card (design §5.2a), and every title launched off this grid inherits
/// it — so the page names it, the same `host · preset` shape the card wears. A plain card
/// says nothing extra: its binding is the host's own default, not a second thing to read.
/// A one-off whose preset has since been deleted resolves as no preset everywhere else,
/// and reads as a plain host here.
fn page_host_label(req: &ConnectRequest) -> String {
    let Some(id) = req.preset.as_deref().filter(|id| !id.is_empty()) else {
        return req.name.clone();
    };
    pf_client_core::presets::PresetsFile::load()
        .presets
        .into_iter()
        .find(|p| p.id == id)
        .map_or_else(
            || req.name.clone(),
            |p| format!("{} \u{b7} {}", req.name, p.name),
        )
}

/// One title's self-emitted `punktfunk://` link (design/client-deep-links.md §5): this
/// page's host with the game's own `launch=` id attached, so the URL boots straight into
/// that title instead of the desktop. Built from the STORE, like every other "Copy link"
/// in this shell, because the stable id and the pin live there rather than on the request.
///
/// A shelf opened from a PINNED card carries that card's one-off preset into the link:
/// what you copy off that shelf is what pressing the card and picking the title does.
/// `None` only when the host has left the store while the page was open.
fn game_link(req: &ConnectRequest, game_id: &str) -> Option<String> {
    pf_client_core::deeplink::saved_host_link(
        &pf_client_core::trust::KnownHosts::load(),
        req.fp_hex.as_deref(),
        &req.addr,
        req.port,
        req.preset.as_deref().filter(|p| !p.is_empty()),
        Some(game_id),
    )
}

/// Open the library page for a saved host and start the fetch. `mgmt_port` comes from
/// the live mDNS `mgmt` TXT when the host is advertising (the hosts page resolves it).
pub fn open(
    app: &AppModel,
    sender: &ComponentSender<AppModel>,
    req: ConnectRequest,
    mgmt_port: Option<u16>,
) {
    let state = build(&app.nav, app.identity.clone(), sender, req, mgmt_port);
    load(&state);
}

/// Screenshot-scene entry: render injected entries (plus pre-seeded textures, keyed by
/// entry id) with no host and no network — the CI `library` scene.
pub fn open_mock(
    nav: &adw::NavigationView,
    identity: (String, String),
    sender: &ComponentSender<AppModel>,
    req: ConnectRequest,
    games: Vec<GameEntry>,
    art: Vec<(String, gdk::Texture)>,
) {
    let state = build(nav, identity, sender, req, None);
    state.mock.set(true);
    state.art.borrow_mut().extend(art);
    if games.is_empty() {
        state.stack.set_visible_child_name("empty");
    } else {
        *state.games.borrow_mut() = games;
        render(&state);
        state.stack.set_visible_child_name("grid");
    }
}

/// Build the page (loading / error / empty / grid states in a stack) and push it.
fn build(
    nav: &adw::NavigationView,
    identity: (String, String),
    sender: &ComponentSender<AppModel>,
    req: ConnectRequest,
    mgmt_port: Option<u16>,
) -> Rc<State> {
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .activate_on_single_click(true)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(6)
        .column_spacing(12)
        .row_spacing(18)
        .valign(gtk::Align::Start)
        .build();
    // Click/keyboard activation fires `child-activated` on the FlowBox, not the child's own
    // `activate` — bridge it so each poster's connect handler (below) runs on click. The
    // bridge must be the guarded one: bare, it recurses until the stack overflows.
    crate::ui_flow::bridge_child_activation(&flow);
    // The launcher shelf: same tile geometry as the games grid, its own FlowBox so the two
    // groups never interleave and each wraps on its own.
    let launcher_flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .activate_on_single_click(true)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(6)
        .column_spacing(12)
        .row_spacing(18)
        .valign(gtk::Align::Start)
        .build();
    crate::ui_flow::bridge_child_activation(&launcher_flow);
    let launchers_heading = gtk::Label::new(Some("Launchers"));
    launchers_heading.add_css_class("pf-group-heading");
    launchers_heading.set_halign(gtk::Align::Start);
    launchers_heading.set_margin_bottom(8);
    let launchers_group = gtk::Box::new(gtk::Orientation::Vertical, 0);
    launchers_group.append(&launchers_heading);
    launchers_group.append(&launcher_flow);
    launchers_group.set_margin_bottom(24);
    launchers_group.set_visible(false);

    let games_heading = gtk::Label::new(Some("Games"));
    games_heading.add_css_class("pf-group-heading");
    games_heading.set_halign(gtk::Align::Start);
    games_heading.set_margin_bottom(8);
    games_heading.set_visible(false);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&launchers_group);
    content.append(&games_heading);
    content.append(&flow);
    let clamp = adw::Clamp::builder()
        .maximum_size(1100)
        .child(&content)
        .build();
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();

    let loading = gtk::Box::new(gtk::Orientation::Vertical, 12);
    loading.set_valign(gtk::Align::Center);
    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    spinner.set_halign(gtk::Align::Center);
    loading.append(&spinner);
    let loading_label = gtk::Label::new(Some("Loading library…"));
    loading_label.add_css_class("dim-label");
    loading.append(&loading_label);

    let error_page = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Couldn't load the library")
        .build();
    let retry = gtk::Button::with_label("Retry");
    retry.add_css_class("pill");
    retry.add_css_class("suggested-action");
    retry.set_halign(gtk::Align::Center);
    error_page.set_child(Some(&retry));

    // Disk-cache provenance lands here, not in the error page: a remembered shelf is still
    // the titles to pick from, and its cards launch through the ordinary connect path.
    let banner = adw::Banner::new("");

    let empty = adw::StatusPage::builder()
        .icon_name("applications-games-symbolic")
        .title("No games found")
        .description(
            "No games found on this host. Install Steam titles or add custom \
                      entries in the host's web console.",
        )
        .build();

    let stack = gtk::Stack::new();
    stack.add_named(&loading, Some("loading"));
    stack.add_named(&error_page, Some("error"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&scrolled, Some("grid"));

    let header = adw::HeaderBar::new();
    let reload = crate::lucide::button("refresh-cw");
    reload.set_tooltip_text(Some("Reload"));
    header.pack_end(&reload);
    // Shared `library_sort`: the same four orders the console's bar offers, so one library
    // reads the same on both fronts. Default is the host's own order, which is a no-op.
    let stored_sort = SortKey::parse(&trust::Settings::load().library_sort);
    let labels: Vec<&str> = SortKey::ALL.iter().map(|k| k.label()).collect();
    let sort_menu = gtk::DropDown::from_strings(&labels);
    sort_menu.set_tooltip_text(Some("Sort"));
    sort_menu.set_selected(
        SortKey::ALL
            .iter()
            .position(|k| *k == stored_sort)
            .unwrap_or(0) as u32,
    );
    header.pack_end(&sort_menu);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&stack));

    let page = adw::NavigationPage::builder()
        .title(format!("{} — Library", page_host_label(&req)))
        .child(&toolbar)
        .build();

    let state = Rc::new(State {
        sender: sender.clone(),
        identity,
        mgmt_port: mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
        req,
        stack,
        flow,
        launcher_flow,
        launchers_group,
        games_heading,
        error_page,
        banner,
        art: RefCell::new(HashMap::new()),
        pics: RefCell::new(HashMap::new()),
        mock: Cell::new(false),
        games: RefCell::new(Vec::new()),
        sort: Cell::new(stored_sort),
        art_rx: RefCell::new(None),
        running: RefCell::new(HashMap::new()),
        generation: Cell::new(0),
    });
    {
        let state = state.clone();
        // Presentation only, like the console's bar: write the key, re-render what is already
        // fetched. Load-modify-save because the settings file has one writer per shell.
        sort_menu.connect_selected_notify(move |menu| {
            let key = SortKey::ALL
                .get(menu.selected() as usize)
                .copied()
                .unwrap_or_default();
            if state.sort.replace(key) == key {
                return;
            }
            let mut settings = trust::Settings::load();
            settings.library_sort = key.id().to_string();
            settings.save();
            render(&state);
        });
    }
    {
        let state = state.clone();
        reload.connect_clicked(move |_| load(&state));
    }
    {
        let state = state.clone();
        retry.connect_clicked(move |_| load(&state));
    }
    nav.push(&page);
    state
}

/// What the library worker reports, in that order: the disk snapshot if there is one,
/// then whatever the host said. Two messages, because the whole point of the first is
/// that it lands without waiting for the second.
enum Loaded {
    Cached(Vec<GameEntry>),
    Fetched(Result<Vec<GameEntry>, library::LibraryError>),
    /// `/status`, after the catalog: a slow answer must not hold the titles back.
    Running(Vec<library::RunningGame>),
}

/// Keep what is up, one row per title; the endable row wins, since it carries End game.
fn set_running(state: &State, games: Vec<library::RunningGame>) {
    let mut by_id: HashMap<String, library::RunningGame> = HashMap::new();
    for g in games.into_iter().filter(library::RunningGame::is_up) {
        let Some(id) = g.app_id.clone() else { continue };
        if by_id.get(&id).is_none_or(|kept| !kept.endable) {
            by_id.insert(id, g);
        }
    }
    *state.running.borrow_mut() = by_id;
}

/// Put one catalog on screen and start its posters. The cards are the same either way —
/// a remembered title launches through the ordinary connect path, which dials the host.
fn show(state: &Rc<State>, games: Vec<GameEntry>) {
    *state.games.borrow_mut() = games.clone();
    render(state);
    state.stack.set_visible_child_name("grid");
    load_art(state, &games);
}

/// Fetch the library off the main thread and route the result into the grid or the
/// error/empty states, with the disk catalog on screen first when there is one.
///
/// A host that never answers keeps that remembered shelf and says so in the banner —
/// the console shell's rule, and the same sentence. Only a first visit to an
/// unreachable host has nothing to show and lands on the error page.
fn load(state: &Rc<State>) {
    if state.mock.get() {
        return; // screenshot scene renders injected entries only
    }
    state.stack.set_visible_child_name("loading");
    state.banner.set_revealed(false);
    let generation = state.generation.get().wrapping_add(1);
    state.generation.set(generation);
    let port = state.mgmt_port;
    let addr = state.req.addr.clone();
    let identity = state.identity.clone();
    let fp_hex = state.req.fp_hex.clone();
    let pin = fp_hex.as_deref().and_then(trust::parse_hex32);
    let (tx, rx) = async_channel::bounded(3);
    let cache_key = fp_hex.clone();
    std::thread::Builder::new()
        .name("punktfunk-library".into())
        .spawn(move || {
            // Read here, not on the main loop: a shelf is not worth a stall. Keyed on the
            // pinned fingerprint, so a box back on a new DHCP lease is the same library —
            // and an unpinned host has no key, so it simply runs uncached.
            if let Some(cached) = cache_key
                .as_deref()
                .and_then(pf_client_core::library_cache::load)
                .filter(|c| !c.games.is_empty())
                && tx.send_blocking(Loaded::Cached(cached.games)).is_err()
            {
                return;
            }
            let fetched = library::fetch_games(&addr, port, &identity, pin);
            if tx.send_blocking(Loaded::Fetched(fetched)).is_err() {
                return;
            }
            let running = library::fetch_running(&addr, port, &identity, pin);
            let _ = tx.send_blocking(Loaded::Running(running));
        })
        .expect("spawn library thread");
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        let mut remembered = false;
        while let Ok(msg) = rx.recv().await {
            let Some(state) = weak.upgrade() else { return };
            if state.generation.get() != generation {
                return; // a newer load already owns the grid
            }
            match msg {
                Loaded::Cached(games) => {
                    remembered = true;
                    state
                        .banner
                        .set_title("Last known library \u{2014} asking the host\u{2026}");
                    state.banner.set_revealed(true);
                    show(&state, games);
                }
                Loaded::Fetched(Ok(games)) if games.is_empty() => {
                    // An empty answer is the host's, so it replaces the memory. The disk
                    // file stays: `store` refuses an empty list rather than blanking it.
                    state.banner.set_revealed(false);
                    state.stack.set_visible_child_name("empty");
                }
                Loaded::Fetched(Ok(games)) => {
                    state.banner.set_revealed(false);
                    show(&state, games);
                    // Remembered AFTER it is on screen: the disk write is not on the path
                    // to a shelf.
                    if let Some(fp) = fp_hex.as_deref() {
                        pf_client_core::library_cache::store(fp, &state.games.borrow());
                    }
                }
                Loaded::Fetched(Err(e)) if remembered => {
                    tracing::info!(addr = %state.req.addr, error = %e, "library fetch failed; keeping the remembered shelf");
                    state
                        .banner
                        .set_title("Last known library \u{2014} the host didn\u{2019}t answer");
                }
                Loaded::Fetched(Err(e)) => {
                    state.error_page.set_description(Some(&e.to_string()));
                    state.stack.set_visible_child_name("error");
                }
                Loaded::Running(games) => {
                    set_running(&state, games);
                    if !state.games.borrow().is_empty() {
                        render(&state);
                    }
                }
            }
        }
    });
}

/// (Re)build the poster grid from one library snapshot. Cached textures apply
/// immediately; the rest keep their monogram placeholder until `load_art` delivers.
fn render(state: &Rc<State>) {
    state.flow.remove_all();
    state.launcher_flow.remove_all();
    state.pics.borrow_mut().clear();
    let games = state.games.borrow();
    // Design D4: launchers never interleave with titles. `collate` is the shared policy —
    // launchers lead as their own group, and the sort applies inside a group, never across.
    let groups = collate::collate(&games[..], state.sort.get(), None);
    let of_group = |key: collate::GroupKey| -> Vec<usize> {
        groups
            .iter()
            .find(|g| g.key == key)
            .map(|g| g.games.clone())
            .unwrap_or_default()
    };
    let launchers = of_group(collate::GroupKey::Launchers);
    // Ungrouped collation puts every title in one bucket the label never draws.
    let titles = of_group(collate::GroupKey::Platform("All".to_string()));
    // The desktop leads the launcher band: both open something rather than play a title, and
    // a host with no launchers gets that band for the tile alone.
    let desktop = desktop_entry();
    state.launcher_flow.append(&game_card(state, &desktop));
    for i in &launchers {
        state.launcher_flow.append(&game_card(state, &games[*i]));
    }
    for i in &titles {
        state.flow.append(&game_card(state, &games[*i]));
    }
    // The band always has the desktop tile in it now, so it is always shown; the GAMES heading
    // still only appears when there is something on both sides of it.
    state.launchers_group.set_visible(true);
    state.games_heading.set_visible(!titles.is_empty());
}

/// The launcher-tile brand marks this shell ships symbolic art for
/// (`data/icons/.../pf-launcher-<t>-symbolic.svg`, embedded via gresource). A plugin may name a
/// mark a newer build carries; an entry whose token isn't here falls back to the launcher's name,
/// which is exactly how every launcher tile looked before icons existed.
const LAUNCHER_ICON_TOKENS: &[&str] = &[
    "steam", "lutris", "heroic", "playnite", "epic", "gog", "xbox",
];

/// The poster-sized mark for an entry, or `None` when it carries no token, names one nothing
/// here draws, or already has real artwork (a plugin that sent a cover has out-voted the token).
///
/// A brand token takes the symbolic art above; anything else is looked up in the shell's Lucide
/// set, which is where the desktop tile's `monitor` comes from. Both take their ink from the
/// theme's foreground, so the two rungs read as one design.
fn poster_mark(game: &GameEntry) -> Option<gtk::Widget> {
    if !game.art.is_empty() {
        return None;
    }
    let token = game.icon_token()?;
    let mark: gtk::Widget = if LAUNCHER_ICON_TOKENS.contains(&token) {
        let img = gtk::Image::from_icon_name(&format!("pf-launcher-{token}-symbolic"));
        img.set_pixel_size(72);
        img.upcast()
    } else if pf_client_core::lucide::path(token).is_some() {
        crate::lucide::icon(token, 56).upcast()
    } else {
        return None;
    };
    mark.add_css_class("pf-poster-launcher-mark");
    mark.set_halign(gtk::Align::Center);
    mark.set_valign(gtk::Align::Center);
    mark.set_vexpand(true);
    Some(mark)
}

/// One poster tile: 2:3 art (~150×225 logical) over the title, with a store badge and a
/// monogram placeholder underneath the async art. Activation starts a session launching
/// this title (silent on a pinned host — the normal trust gate applies).
fn game_card(state: &Rc<State>, game: &GameEntry) -> gtk::FlowBoxChild {
    // A launcher usually ships no poster. Its brand mark, when we ship one, IS the poster; failing
    // that, naming the launcher on an accent face says "opens Steam". A title monogram on the
    // neutral face would say "a game whose cover didn't load", which is why games keep it.
    let launcher = game.is_launcher();
    let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    if let Some(mark) = poster_mark(game) {
        placeholder.append(&mark);
    } else {
        let monogram = if launcher {
            let l = gtk::Label::new(Some(store_label(&game.store)));
            l.add_css_class("pf-poster-launcher-name");
            l
        } else {
            let l = gtk::Label::new(Some(&initials(&game.title)));
            l.add_css_class("pf-poster-monogram");
            l
        };
        monogram.set_halign(gtk::Align::Center);
        monogram.set_valign(gtk::Align::Center);
        monogram.set_vexpand(true);
        placeholder.append(&monogram);
    }

    let pic = gtk::Picture::new();
    pic.set_content_fit(gtk::ContentFit::Cover);
    if let Some(tex) = state.art.borrow().get(&game.id) {
        pic.set_paintable(Some(tex));
    }
    state.pics.borrow_mut().insert(game.id.clone(), pic.clone());

    let badge = gtk::Label::new(Some(store_label(&game.store)));
    badge.add_css_class("pf-pill");
    badge.add_css_class("pf-store-badge");
    if launcher {
        badge.add_css_class("pf-launcher");
    }
    badge.set_halign(gtk::Align::Start);
    badge.set_valign(gtk::Align::Start);
    badge.set_margin_start(6);
    badge.set_margin_top(6);

    // The tile's own actions. Today that is one — "Copy link", the per-GAME half of the
    // pairing the host cards already offer (design/client-deep-links.md §5 names the
    // library game context menu as an attach point) — hung off a menu rather than a bare
    // button so the next one lands next to it instead of growing a second affordance.
    let actions = gio::SimpleActionGroup::new();
    {
        let (sender, req, id) = (state.sender.clone(), state.req.clone(), game.id.clone());
        let a = gio::SimpleAction::new("copy-link", None);
        a.connect_activate(move |_, _| match game_link(&req, &id) {
            Some(url) => {
                if let Some(display) = gdk::Display::default() {
                    display.clipboard().set_text(&url);
                }
                sender.input(AppMsg::Toast("Link copied".into()));
            }
            // Only reachable if the host was forgotten while this page was open.
            None => sender.input(AppMsg::Toast("This host isn't saved any more".into())),
        });
        actions.add_action(&a);
    }
    let up = state.running.borrow().get(&game.id).cloned();
    let endable = up.as_ref().is_some_and(|g| g.endable);
    if endable {
        let (weak, id, title) = (Rc::downgrade(state), game.id.clone(), game.title.clone());
        let a = gio::SimpleAction::new("end-game", None);
        a.connect_activate(move |_, _| {
            if let Some(state) = weak.upgrade() {
                confirm_end_game(&state, &id, &title);
            }
        });
        actions.add_action(&a);
    }
    let menu = gio::Menu::new();
    menu.append(Some("Copy link"), Some("game.copy-link"));
    if endable {
        menu.append(Some("End game"), Some("game.end-game"));
    }
    let menu_btn = gtk::MenuButton::builder()
        .child(&crate::lucide::row_icon("ellipsis"))
        .menu_model(&menu)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .build();
    menu_btn.add_css_class("flat");
    menu_btn.add_css_class("pf-poster-menu");
    menu_btn.set_tooltip_text(Some("More options"));

    let poster = gtk::Overlay::new();
    poster.set_child(Some(&placeholder));
    poster.add_overlay(&pic);
    poster.add_overlay(&badge);
    if up.is_some() {
        let pill = gtk::Label::new(Some("Running"));
        pill.add_css_class("pf-pill");
        pill.add_css_class("pf-store-badge");
        pill.set_halign(gtk::Align::Start);
        pill.set_valign(gtk::Align::End);
        pill.set_margin_start(6);
        pill.set_margin_bottom(6);
        poster.add_overlay(&pill);
    }
    poster.add_overlay(&menu_btn);
    poster.insert_action_group("game", Some(&actions));
    poster.add_css_class("pf-poster");
    if launcher {
        poster.add_css_class("pf-launcher");
    }
    poster.set_overflow(gtk::Overflow::Hidden);
    poster.set_size_request(150, 225);
    poster.set_halign(gtk::Align::Center);

    let title = gtk::Label::new(Some(&game.title));
    title.add_css_class("caption");
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.set_max_width_chars(16);
    title.set_tooltip_text(Some(&game.title));

    let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
    card.append(&poster);
    card.append(&title);

    let child = gtk::FlowBoxChild::new();
    child.set_child(Some(&card));
    // Right-click anywhere on the tile is the same menu — the desktop gesture for "this
    // item's actions", and what the host cards already answer to.
    let right_click = gtk::GestureClick::builder().button(3).build();
    {
        let menu_btn = menu_btn.clone();
        right_click.connect_pressed(move |_, _, _, _| menu_btn.popup());
    }
    child.add_controller(right_click);
    let sender = state.sender.clone();
    let mut req = state.req.clone();
    // The desktop tile is the host, not one of its titles: it streams with no launch id.
    // Asking a host to launch what it is already showing is how a second copy starts.
    if !is_desktop(game) {
        req.launch = Some((game.id.clone(), game.title.clone()));
    }
    child.connect_activate(move |_| sender.input(AppMsg::Connect(req.clone())));
    child
}

/// Ending a game can lose unsaved progress, so it asks first.
fn confirm_end_game(state: &Rc<State>, id: &str, title: &str) {
    let dialog = adw::AlertDialog::new(
        Some(&format!("End {title}?")),
        Some("Unsaved progress in the game is lost."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("end", "End game")]);
    dialog.set_response_appearance("end", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let weak = Rc::downgrade(state);
    let (id, title) = (id.to_string(), title.to_string());
    dialog.connect_response(Some("end"), move |_, _| {
        if let Some(state) = weak.upgrade() {
            end_game(&state, id.clone(), title.clone());
        }
    });
    dialog.present(Some(&state.stack));
}

/// Ask the host on a worker, toast the answer, then redraw from a fresh `/status`.
fn end_game(state: &Rc<State>, id: String, title: String) {
    let (addr, port, identity) = (
        state.req.addr.clone(),
        state.mgmt_port,
        state.identity.clone(),
    );
    let pin = state.req.fp_hex.as_deref().and_then(trust::parse_hex32);
    let (tx, rx) = async_channel::bounded(1);
    std::thread::Builder::new()
        .name("punktfunk-endgame".into())
        .spawn(move || {
            let outcome = library::end_game(&addr, port, &identity, pin, &id);
            let running = library::fetch_running(&addr, port, &identity, pin);
            let _ = tx.send_blocking((outcome, running));
        })
        .expect("spawn end-game thread");
    let (weak, sender) = (Rc::downgrade(state), state.sender.clone());
    glib::spawn_future_local(async move {
        let Ok((outcome, running)) = rx.recv().await else {
            return;
        };
        sender.input(AppMsg::Toast(outcome.notice(&title)));
        if let Some(state) = weak.upgrade() {
            set_running(&state, running);
            render(&state);
        }
    });
}

fn is_desktop(game: &GameEntry) -> bool {
    game.id == DESKTOP_ID
}

/// Streaming the desktop was the host card's click, two pages back from a shelf. This puts it
/// on the shelf, so the library is never a dead end for the desktop-only user and a host with
/// no plugins still has one card to press. Never fetched, never cached: built here — the id and
/// the mark come from `pf-client-core` so this card is the same one on every shell.
fn desktop_entry() -> GameEntry {
    GameEntry {
        id: DESKTOP_ID.into(),
        store: String::new(),
        title: "Desktop".into(),
        art: Default::default(),
        platform: None,
        developer: None,
        release_year: None,
        genres: Vec::new(),
        role: None,
        icon: Some(DESKTOP_ICON.into()),
        stats: None,
    }
}

/// Fetch poster art for every uncached entry on a small worker pool, walking each
/// entry's candidates in the Apple fallback order (portrait → header → hero) and
/// texturing the first that loads on the main loop.
fn load_art(state: &Rc<State>, games: &[GameEntry]) {
    let base = library::base_url(&state.req.addr, state.mgmt_port);
    let jobs: VecDeque<(String, Vec<String>)> = {
        let cache = state.art.borrow();
        games
            .iter()
            .filter(|g| !cache.contains_key(&g.id))
            .map(|g| (g.id.clone(), g.art.poster_candidates(&base)))
            .filter(|(_, candidates)| !candidates.is_empty())
            .collect()
    };
    if jobs.is_empty() {
        return;
    }
    let identity = state.identity.clone();
    let pin = state.req.fp_hex.as_deref().and_then(trust::parse_hex32);
    let rx = library::spawn_art_fetch(base, identity, pin, jobs);
    // A previous page's channel closes here too: one library page, one art run.
    if let Some(old) = state.art_rx.replace(Some(rx.clone())) {
        old.close();
    }
    let weak = Rc::downgrade(state);
    glib::spawn_future_local(async move {
        while let Ok((id, bytes)) = rx.recv().await {
            let Some(state) = weak.upgrade() else { break };
            // Texture decode happens here on the main loop — posters are small (tens of
            // KB), and `from_bytes` handles jpeg/png alike.
            match gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                Ok(tex) => {
                    if let Some(pic) = state.pics.borrow().get(&id) {
                        pic.set_paintable(Some(&tex));
                    }
                    state.art.borrow_mut().insert(id, tex);
                }
                Err(e) => tracing::debug!(%id, error = %e, "undecodable poster"),
            }
        }
    });
}
