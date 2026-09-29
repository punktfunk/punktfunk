//! The Library destination (design §2.5): one shelf at a time — a paired host, or a host with
//! one of its pinned presets — laid out in the sections this device keeps. The catalog comes
//! from the disk first, then the host; posters load as their rows are drawn. The rows are one
//! virtualized list, so a library of thousands scrolls like one of ten.

pub mod art;
mod canvas;
mod customize;
mod details;
mod poster;
mod rows;
mod tile;

use crate::hosts::{saved_request, ConnectRequest, HostRef};
use crate::store::Store;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use pf_client_core::collate::{GroupBy, SortKey};
use pf_client_core::library::{self, GameEntry, LibraryError, RunningGame};
use pf_client_core::library_layout as layout;
use relm4::prelude::*;
use rows::Row;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// The widest the page's content grows; a wider window centres it.
const MAX_WIDTH: i32 = 2400;

/// The `Settings::extra` key the chosen shelf is kept under, on this device only.
const SHELF_KEY: &str = "library_shelf";

/// One shelf: a paired host's library, streamed with its binding or a pinned preset.
#[derive(Clone, Debug, PartialEq)]
pub struct Shelf {
    /// The host's record id, then `/preset id` for a pinned shelf.
    pub key: String,
    pub host: HostRef,
    /// "Desk", or "Desk · Work" for a pinned shelf.
    pub label: String,
    pub os: String,
    /// What a title here connects with; a pinned shelf's preset is its one-off.
    pub req: ConnectRequest,
    pub mgmt_port: u16,
}

/// A paired host as the Desktops section shows it.
#[derive(Clone, Debug)]
pub struct Desktop {
    pub name: String,
    pub os: String,
    /// What the host has up, empty when nothing.
    pub playing: String,
    pub req: ConnectRequest,
}

/// What the tiles read. Shared with the row factory, which builds tiles as rows are bound.
pub struct View {
    pub games: RefCell<Vec<GameEntry>>,
    pub running: RefCell<HashMap<String, RunningGame>>,
    pub favorites: RefCell<Vec<String>>,
    pub shelves: RefCell<Vec<Shelf>>,
    pub selected: RefCell<Option<String>>,
    pub desktops: RefCell<Vec<Desktop>>,
    /// The grid's columns: every grid row has this many slots.
    pub columns: Cell<usize>,
    pub art: Rc<art::Art>,
    pub sender: relm4::Sender<LibraryMsg>,
}

pub struct LibraryInit {
    pub store: Rc<Store>,
    pub identity: (String, String),
    pub views: adw::ViewStack,
    pub narrow: adw::Breakpoint,
}

#[derive(Debug)]
pub enum LibraryMsg {
    /// The store changed (hosts, favorites, the sections), or the tab came on screen.
    Refresh,
    Select(String),
    /// Show this host's shelf: a card's Browse Library, a `browse` link, Start in.
    Open(ConnectRequest),
    Reload,
    Loaded(u64, Loaded),
    Search(String),
    Columns(usize),
    Sort(SortKey),
    Group(Option<GroupBy>),
    Play(String),
    /// A host's desktop: an index into the Desktops row, or `None` for the shelf's own host.
    Desktop(Option<usize>),
    ToggleFavorite(String),
    Details(String),
    CopyLink(String),
    Shortcut(String),
    EndGame(String),
    Ended(u64, library::GameEnd, String, Vec<RunningGame>),
    FocusSearch,
    /// The shelf's host no longer takes this device's pin: pair again.
    Pair,
    /// Screenshot scenes: these titles and posters on the selected shelf, no network.
    Mock(Vec<GameEntry>, Vec<(String, gdk::Texture)>),
}

/// What the library worker reports, in order: the disk snapshot if there is one, the host's
/// answer, then what the host has up. The first lands without waiting for the rest.
#[derive(Debug)]
pub enum Loaded {
    Cached(Vec<GameEntry>),
    Fetched(Result<Vec<GameEntry>, library::LibraryError>),
    Running(Vec<RunningGame>),
}

#[derive(Debug)]
pub enum LibraryOutput {
    Connect(ConnectRequest),
    WakeConnect(ConnectRequest),
    Toast(String),
    ShowHosts,
    Pair(ConnectRequest),
}

pub struct LibraryPage {
    store: Rc<Store>,
    identity: (String, String),
    view: Rc<View>,
    /// Bumped by every load. A result whose generation is stale when it lands is dropped.
    generation: u64,
    /// The shelf the catalog on screen belongs to, as it was loaded. A re-pair or a new address
    /// changes it, and the shelf loads again.
    loaded: Option<Shelf>,
    sort: SortKey,
    /// This window's, not a setting.
    group: Option<GroupBy>,
    search: String,
    columns: usize,
    canvas: canvas::Canvas,
    /// What the canvas shows. A relayout that builds the same rows leaves it alone; a new
    /// catalog or running set clears this, since equal rows then show new data.
    drawn: Vec<Row>,
    /// The shelves and the selection the chip bar shows; a refresh that changes neither keeps it.
    chips_drawn: Option<Chips>,
    widgets: Widgets,
}

/// Each shelf's key and label, and the selected key.
type Chips = (Vec<(String, String)>, Option<String>);

struct Widgets {
    root: adw::ToolbarView,
    /// The shelf picker, above every state of the page so a failing shelf never traps it.
    chips: gtk::Box,
    stack: gtk::Stack,
    banner: adw::Banner,
    error: adw::StatusPage,
    retry: gtk::Button,
    pair: gtk::Button,
    search_bar: gtk::SearchBar,
    search_entry: gtk::SearchEntry,
    sort: gio::SimpleAction,
}

impl SimpleComponent for LibraryPage {
    type Init = LibraryInit;
    type Input = LibraryMsg;
    type Output = LibraryOutput;
    type Root = adw::ToolbarView;
    type Widgets = ();

    fn init_root() -> Self::Root {
        adw::ToolbarView::new()
    }

    fn init(
        init: Self::Init,
        root: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let LibraryInit {
            store,
            identity,
            views,
            narrow,
        } = init;
        let view = Rc::new(View {
            games: RefCell::default(),
            running: RefCell::default(),
            favorites: RefCell::default(),
            shelves: RefCell::default(),
            selected: RefCell::default(),
            desktops: RefCell::default(),
            columns: Cell::new(4),
            art: Rc::default(),
            sender: sender.input_sender().clone(),
        });

        let canvas = canvas::Canvas::new(&view, MAX_WIDTH);
        let scrolled = row_scroller(&canvas, &sender);
        let stack = gtk::Stack::new();
        stack.add_named(&scrolled, Some("rows"));
        stack.add_named(&loading_page(), Some("loading"));
        let error = adw::StatusPage::builder()
            .icon_name("dialog-error-symbolic")
            .title("Couldn't load the library")
            .build();
        let pill = |label: &str, msg: fn() -> LibraryMsg| {
            let b = gtk::Button::builder()
                .label(label)
                .css_classes(["pill", "suggested-action"])
                .halign(gtk::Align::Center)
                .build();
            let sender = sender.clone();
            b.connect_clicked(move |_| sender.input(msg()));
            b
        };
        let (retry, pair) = (
            pill("Retry", || LibraryMsg::Reload),
            pill("Pair Again", || LibraryMsg::Pair),
        );
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        actions.set_halign(gtk::Align::Center);
        actions.append(&retry);
        actions.append(&pair);
        error.set_child(Some(&actions));
        stack.add_named(&error, Some("error"));
        stack.add_named(
            &status_page(
                "applications-games-symbolic",
                "No library yet",
                Some("Pair a host to browse its games here."),
                ("Show Hosts", {
                    let sender = sender.clone();
                    move || {
                        let _ = sender.output(LibraryOutput::ShowHosts);
                    }
                }),
            ),
            Some("nohost"),
        );

        let (header, bar) = crate::widgets::chrome::destination_header(&views, &narrow);
        let reload = crate::widgets::lucide::button("refresh-cw");
        reload.set_tooltip_text(Some("Reload"));
        {
            let sender = sender.clone();
            reload.connect_clicked(move |_| sender.input(LibraryMsg::Reload));
        }
        header.pack_start(&reload);
        let (arrange, sort, group) = arrange_menu(&store, &sender);
        root.insert_action_group("title", Some(&title_actions(&sender)));
        root.insert_action_group(
            "library",
            Some(&{
                let g = gio::SimpleActionGroup::new();
                g.add_action(&sort);
                g.add_action(&group);
                g
            }),
        );
        let customize = gtk::MenuButton::builder()
            .child(&crate::widgets::lucide::row_icon("sliders-horizontal"))
            .popover(&customize::popover(&store))
            .tooltip_text("Customize sections")
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Search titles")
            .build();
        {
            let sender = sender.clone();
            search_entry.connect_search_changed(move |e| {
                sender.input(LibraryMsg::Search(e.text().to_string()))
            });
        }
        let search_bar = gtk::SearchBar::builder()
            .child(
                &adw::Clamp::builder()
                    .maximum_size(480)
                    .child(&search_entry)
                    .build(),
            )
            .build();
        search_bar.connect_entry(&search_entry);
        // Typing anywhere on the Library starts a search.
        search_bar.set_key_capture_widget(Some(&root));
        let search_toggle = gtk::ToggleButton::builder()
            .child(&crate::widgets::lucide::row_icon("search"))
            .tooltip_text("Search titles")
            .build();
        search_bar
            .bind_property("search-mode-enabled", &search_toggle, "active")
            .bidirectional()
            .sync_create()
            .build();
        // Packed inward from the right: menu, customize, sort, search.
        header.pack_end(&crate::widgets::chrome::primary_menu());
        header.pack_end(&customize);
        header.pack_end(&arrange);
        header.pack_end(&search_toggle);

        // A remembered shelf says so until the host answers.
        let banner = adw::Banner::new("");
        {
            let sender = sender.clone();
            banner.connect_button_clicked(move |_| sender.input(LibraryMsg::Pair));
        }
        let chips = gtk::Box::new(gtk::Orientation::Vertical, 0);
        chips.set_margin_top(12);
        chips.set_margin_bottom(12);
        chips.set_margin_start(8);
        chips.set_margin_end(8);
        chips.set_visible(false);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.append(
            &adw::Clamp::builder()
                .maximum_size(MAX_WIDTH)
                .tightening_threshold(MAX_WIDTH)
                .child(&chips)
                .build(),
        );
        content.append(&stack);
        stack.set_vexpand(true);
        root.add_top_bar(&header);
        root.add_top_bar(&search_bar);
        root.add_top_bar(&banner);
        root.add_bottom_bar(&bar);
        root.set_content(Some(&content));
        {
            let sender = sender.clone();
            store.subscribe(move |_| sender.input(LibraryMsg::Refresh));
        }
        {
            let sender = sender.clone();
            root.connect_map(move |_| sender.input(LibraryMsg::Refresh));
        }

        let sort_key = SortKey::parse(&store.settings().library_sort);
        let mut model = LibraryPage {
            store,
            identity,
            view,
            generation: 0,
            loaded: None,
            sort: sort_key,
            group: None,
            search: String::new(),
            columns: 4,
            canvas,
            chips_drawn: None,
            drawn: Vec::new(),
            widgets: Widgets {
                root,
                chips,
                stack,
                banner,
                error,
                retry,
                pair,
                search_bar,
                search_entry,
                sort,
            },
        };
        model.refresh();
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: LibraryMsg, sender: ComponentSender<Self>) {
        let out = |o| {
            let _ = sender.output(o);
        };
        match msg {
            LibraryMsg::Refresh => self.refresh(),
            LibraryMsg::Select(key) => self.select(&key),
            LibraryMsg::Open(req) => {
                let key = self
                    .view
                    .shelves
                    .borrow()
                    .iter()
                    .find(|s| same_host(&s.req, &req) && s.req.preset == req.preset)
                    .map(|s| s.key.clone());
                match key {
                    Some(key) => self.select(&key),
                    None => out(LibraryOutput::Toast(format!(
                        "Pair {} to browse its library.",
                        req.name
                    ))),
                }
            }
            LibraryMsg::Reload => self.load(true),
            LibraryMsg::Loaded(generation, loaded) => {
                if generation == self.generation {
                    self.loaded_step(loaded);
                }
            }
            LibraryMsg::Search(text) => {
                self.search = text;
                self.relayout();
            }
            LibraryMsg::Columns(n) => {
                if n != self.columns {
                    self.columns = n;
                    self.relayout();
                }
            }
            LibraryMsg::Sort(key) => {
                self.sort = key;
                self.store
                    .update_settings(|s| s.library_sort = key.id().to_string());
                self.relayout();
            }
            LibraryMsg::Group(by) => {
                self.group = by;
                self.relayout();
            }
            LibraryMsg::Play(id) => {
                if let Some(mut req) = self.shelf().map(|s| s.req) {
                    // A title already up resumes: launching it again is how a second copy starts.
                    let up = self.view.running.borrow().contains_key(&id);
                    req.launch = (!up && id != library::DESKTOP_ID).then_some(id);
                    out(connect(req));
                }
            }
            LibraryMsg::Desktop(i) => {
                let req = match i {
                    Some(i) => self.view.desktops.borrow().get(i).map(|d| d.req.clone()),
                    None => self.shelf().map(|s| s.req),
                };
                if let Some(req) = req {
                    out(connect(req));
                }
            }
            LibraryMsg::ToggleFavorite(id) => {
                if let Some(fp) = self.shelf().and_then(|s| s.req.fp_hex) {
                    self.store
                        .update_settings(|s| layout::toggle_favorite(s, &fp, &id));
                }
            }
            LibraryMsg::Details(id) => {
                if let Some(shelf) = self.shelf() {
                    details::show(&self.widgets.root, &self.view, &self.store, &shelf, &id);
                }
            }
            LibraryMsg::CopyLink(id) => match self.link(&id) {
                Some((url, _)) => {
                    if let Some(display) = gdk::Display::default() {
                        display.clipboard().set_text(&url);
                    }
                    out(LibraryOutput::Toast("Link copied".into()));
                }
                None => out(LibraryOutput::Toast(
                    "This host isn't saved any more".into(),
                )),
            },
            LibraryMsg::Shortcut(id) => {
                if let Some((url, label)) = self.link(&id) {
                    if let Some(msg) =
                        crate::desktop::shortcuts::create(&self.widgets.root, &label, &url)
                    {
                        out(LibraryOutput::Toast(msg));
                    }
                }
            }
            LibraryMsg::EndGame(id) => self.confirm_end(&id),
            LibraryMsg::Ended(generation, outcome, title, running) => {
                out(LibraryOutput::Toast(outcome.notice(&title)));
                if generation == self.generation {
                    self.set_running(running);
                    self.relayout();
                }
            }
            LibraryMsg::Pair => {
                if let Some(shelf) = self.shelf() {
                    out(LibraryOutput::Pair(shelf.req));
                }
            }
            LibraryMsg::FocusSearch => {
                self.widgets.search_bar.set_search_mode(true);
                self.widgets.search_entry.grab_focus();
            }
            LibraryMsg::Mock(games, art) => {
                self.generation += 1;
                self.loaded = self.shelf();
                for (id, tex) in art {
                    self.view.art.insert(id, tex);
                }
                *self.view.games.borrow_mut() = games;
                self.drawn.clear();
                self.widgets.stack.set_visible_child_name("rows");
                self.relayout();
            }
        }
    }
}

/// The pin when both have one, else the address.
fn same_host(a: &ConnectRequest, b: &ConnectRequest) -> bool {
    match (&a.fp_hex, &b.fp_hex) {
        (Some(x), Some(y)) => x == y,
        _ => a.addr == b.addr && a.port == b.port,
    }
}

/// A title or a desktop dials first and wakes on a known MAC: the shelf may be a memory of a
/// host that has since gone to sleep.
fn connect(req: ConnectRequest) -> LibraryOutput {
    if req.mac.is_empty() {
        LibraryOutput::Connect(req)
    } else {
        LibraryOutput::WakeConnect(req)
    }
}

/// How many columns a list `width` wide holds at the smallest poster: n posters take
/// n·150 + (n−1)·16, and the row's margins take the 16 the last gap leaves over. The posters
/// stretch into whatever is left.
fn columns_for(width: f64) -> usize {
    (width / f64::from(poster::NATURAL_W + tile::GAP))
        .floor()
        .max(1.0) as usize
}

/// The rows in a scrolled window. The canvas reports its content width as the horizontal page
/// size, and the grid's columns follow it.
fn row_scroller(
    canvas: &canvas::Canvas,
    sender: &ComponentSender<LibraryPage>,
) -> gtk::ScrolledWindow {
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(canvas)
        .build();
    let (sender, last) = (sender.clone(), Cell::new(0usize));
    scrolled.hadjustment().connect_page_size_notify(move |adj| {
        let n = columns_for(adj.page_size());
        if n != last.replace(n) {
            sender.input(LibraryMsg::Columns(n));
        }
    });
    scrolled
}

fn loading_page() -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_valign(gtk::Align::Center);
    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    page.append(&spinner);
    page.append(
        &gtk::Label::builder()
            .label("Loading library\u{2026}")
            .css_classes(["dim-label"])
            .build(),
    );
    page
}

/// Why a shelf did not load, and the next move, in the player's words.
fn failure(e: &LibraryError, host: &str) -> String {
    match e {
        LibraryError::PinMismatch => {
            format!("{host}'s certificate changed since you paired. Pair again to see its games.")
        }
        LibraryError::NotPaired => {
            format!("{host} doesn't recognize this device any more. Pair again to see its games.")
        }
        LibraryError::Http(code) => {
            format!("{host} turned the request down (HTTP {code}). Try again later.")
        }
        LibraryError::Unreachable(_) => {
            format!("{host} didn't answer. Check that it's on, then try again.")
        }
    }
}

fn status_page(
    icon: &str,
    title: &str,
    description: Option<&str>,
    (label, pressed): (&str, impl Fn() + 'static),
) -> adw::StatusPage {
    let page = adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .build();
    page.set_description(description);
    let button = gtk::Button::builder()
        .label(label)
        .css_classes(["pill", "suggested-action"])
        .halign(gtk::Align::Center)
        .build();
    button.connect_clicked(move |_| pressed());
    page.set_child(Some(&button));
    page
}

/// A title's menu rows, each taking the title's id: one set for the page, not one per poster.
fn title_actions(sender: &ComponentSender<LibraryPage>) -> gio::SimpleActionGroup {
    let group = gio::SimpleActionGroup::new();
    type Act = (&'static str, fn(String) -> LibraryMsg);
    let acts: [Act; 5] = [
        ("play", LibraryMsg::Play),
        ("favorite", LibraryMsg::ToggleFavorite),
        ("details", LibraryMsg::Details),
        ("copy-link", LibraryMsg::CopyLink),
        ("end-game", LibraryMsg::EndGame),
    ];
    for (name, msg) in acts {
        let action = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        let sender = sender.clone();
        action.connect_activate(move |_, v| {
            if let Some(id) = v.and_then(|v| v.str()) {
                sender.input(msg(id.to_string()));
            }
        });
        group.add_action(&action);
    }
    group
}

/// Sort and Group. The sort is the shared `library_sort`.
fn arrange_menu(
    store: &Store,
    sender: &ComponentSender<LibraryPage>,
) -> (gtk::MenuButton, gio::SimpleAction, gio::SimpleAction) {
    let stateful = |name: &str, at: &str, apply: Box<dyn Fn(&str)>| {
        let a =
            gio::SimpleAction::new_stateful(name, Some(glib::VariantTy::STRING), &at.to_variant());
        a.connect_change_state(move |a, v| {
            if let Some(id) = v.and_then(|v| v.str()) {
                a.set_state(&id.to_variant());
                apply(id);
            }
        });
        a
    };
    let sort = stateful(
        "sort",
        SortKey::parse(&store.settings().library_sort).id(),
        Box::new({
            let sender = sender.clone();
            move |id| sender.input(LibraryMsg::Sort(SortKey::parse(id)))
        }),
    );
    let group = stateful(
        "group",
        "none",
        Box::new({
            let sender = sender.clone();
            move |id| {
                sender.input(LibraryMsg::Group(match id {
                    "platform" => Some(GroupBy::Platform),
                    "store" => Some(GroupBy::Store),
                    _ => None,
                }))
            }
        }),
    );
    let section = |action: &str, items: &[(&str, &str)]| {
        let m = gio::Menu::new();
        for (id, label) in items {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some(action), Some(&id.to_variant()));
            m.append_item(&item);
        }
        m
    };
    let sorts: Vec<(&str, &str)> = SortKey::ALL.iter().map(|k| (k.id(), k.label())).collect();
    let menu = gio::Menu::new();
    menu.append_section(Some("Sort"), &section("library.sort", &sorts));
    menu.append_section(
        Some("Group"),
        &section(
            "library.group",
            &[
                ("none", "None"),
                ("platform", "Platform"),
                ("store", "Store"),
            ],
        ),
    );
    let button = gtk::MenuButton::builder()
        .child(&crate::widgets::lucide::row_icon("arrow-up-down"))
        .menu_model(&menu)
        .tooltip_text("Sort and group")
        .build();
    (button, sort, group)
}

impl LibraryPage {
    fn shelf(&self) -> Option<Shelf> {
        let selected = self.view.selected.borrow().clone()?;
        self.view
            .shelves
            .borrow()
            .iter()
            .find(|s| s.key == selected)
            .cloned()
    }

    /// Re-read the shelves, the desktops and the favorites. A changed shelf loads once on screen.
    fn refresh(&mut self) {
        let (shelves, desktops) = self.shelves();
        let settings = self.store.settings().clone();
        let remembered = settings
            .extra
            .get(SHELF_KEY)
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let fallback = {
            let known = self.store.hosts();
            pf_client_core::start::default_host(&settings, &known)
                .and_then(|i| known.hosts[i].id.clone())
        };
        let current = self.view.selected.borrow().clone();
        let pick = [current, remembered, fallback]
            .into_iter()
            .flatten()
            .find(|k| shelves.iter().any(|s| &s.key == k))
            .or_else(|| shelves.first().map(|s| s.key.clone()));
        *self.view.shelves.borrow_mut() = shelves;
        *self.view.desktops.borrow_mut() = desktops;
        *self.view.selected.borrow_mut() = pick.clone();
        *self.view.favorites.borrow_mut() = self
            .shelf()
            .and_then(|s| s.req.fp_hex)
            .map(|fp| layout::favorites(&settings, &fp))
            .unwrap_or_default();
        self.sort = SortKey::parse(&settings.library_sort);
        self.widgets.sort.set_state(&self.sort.id().to_variant());
        self.draw_chips();
        match pick {
            None => {
                self.loaded = None;
                self.widgets.banner.set_revealed(false);
                self.widgets.stack.set_visible_child_name("nohost");
            }
            // Only a shelf on screen loads: a load can wake the host.
            Some(_) if self.loaded != self.shelf() => {
                if self.widgets.root.is_mapped() {
                    self.load(false);
                }
            }
            Some(_) => self.relayout(),
        }
    }

    /// A shelf per paired host, then one per preset it pins; a desktop per paired host.
    fn shelves(&self) -> (Vec<Shelf>, Vec<Desktop>) {
        let presets = self.store.presets();
        let (mut shelves, mut desktops) = (Vec::new(), Vec::new());
        for k in self.store.hosts().hosts.iter().filter(|k| k.paired) {
            let key = k.id.clone().unwrap_or_else(|| k.card_key());
            let req = saved_request(k);
            let shelf = |key: String, label: String, req: ConnectRequest| Shelf {
                key,
                host: HostRef::of(k),
                label,
                os: k.os.clone(),
                req,
                mgmt_port: k.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
            };
            shelves.push(shelf(key.clone(), k.name.clone(), req.clone()));
            for p in k
                .pinned_presets
                .iter()
                .filter_map(|id| presets.presets.iter().find(|p| &p.id == id))
            {
                let mut pinned = req.clone();
                pinned.preset = Some(p.id.clone());
                shelves.push(shelf(
                    format!("{key}/{}", p.id),
                    format!("{} \u{b7} {}", k.name, p.name),
                    pinned,
                ));
            }
            desktops.push(Desktop {
                name: k.name.clone(),
                os: k.os.clone(),
                playing: library::now_playing(&k.fp_hex),
                req,
            });
        }
        (shelves, desktops)
    }

    fn select(&mut self, key: &str) {
        if self.loaded.as_ref().is_some_and(|s| s.key == key) {
            return;
        }
        *self.view.selected.borrow_mut() = Some(key.to_string());
        // The write notifies the store's listeners, and this page refreshes from there.
        let key = key.to_string();
        self.store.update_settings(|s| {
            s.extra.insert(SHELF_KEY.into(), key.into());
        });
        self.refresh();
    }

    /// Fetch the shelf off the main thread, the disk catalog first. A host that never answers
    /// keeps the remembered shelf and says so; only a first visit lands on the error page.
    fn load(&mut self, forced: bool) {
        let Some(shelf) = self.shelf() else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let new_shelf = self.loaded.as_ref() != Some(&shelf);
        self.loaded = Some(shelf.clone());
        let pin = shelf
            .req
            .fp_hex
            .as_deref()
            .and_then(crate::trust::parse_hex32);
        if new_shelf {
            self.view.games.borrow_mut().clear();
            self.view.running.borrow_mut().clear();
            self.view.art.reset(Some((
                library::base_url(&shelf.req.addr, shelf.mgmt_port),
                self.identity.clone(),
                pin,
            )));
        }
        if new_shelf || forced {
            self.widgets.stack.set_visible_child_name("loading");
            self.widgets.banner.set_revealed(false);
        }
        if crate::shots::shot_scene().is_some() {
            return;
        }
        // A sleeping host gets a knock while its remembered shelf is on screen.
        if self.store.settings().auto_wake && !shelf.req.mac.is_empty() {
            crate::wol::wake(&shelf.req.mac, shelf.req.addr.parse().ok());
        }
        let (sender, identity) = (self.view.sender.clone(), self.identity.clone());
        let (addr, port, fp) = (shelf.req.addr, shelf.mgmt_port, shelf.req.fp_hex);
        std::thread::Builder::new()
            .name("punktfunk-library".into())
            .spawn(move || {
                let send = |l| sender.send(LibraryMsg::Loaded(generation, l)).is_ok();
                // Keyed on the pin, so a box on a new DHCP lease is the same library.
                if let Some(cached) = fp
                    .as_deref()
                    .and_then(pf_client_core::library_cache::load)
                    .filter(|c| !c.games.is_empty())
                {
                    if !send(Loaded::Cached(cached.games)) {
                        return;
                    }
                }
                let fetched = library::fetch_games(&addr, port, &identity, pin);
                if let (Some(fp), Ok(games)) = (&fp, &fetched) {
                    // `store` keeps the last real catalog over an empty answer.
                    pf_client_core::library_cache::store(fp, games);
                }
                if !send(Loaded::Fetched(fetched)) {
                    return;
                }
                send(Loaded::Running(library::fetch_running(
                    &addr, port, &identity, pin,
                )));
            })
            .expect("spawn library thread");
    }

    fn loaded_step(&mut self, loaded: Loaded) {
        let w = &self.widgets;
        match loaded {
            Loaded::Cached(games) => {
                w.banner.set_button_label(None);
                w.banner
                    .set_title("Last known library \u{2014} asking the host\u{2026}");
                w.banner.set_revealed(true);
                *self.view.games.borrow_mut() = games;
                self.drawn.clear();
                w.stack.set_visible_child_name("rows");
            }
            Loaded::Fetched(Ok(games)) => {
                w.banner.set_revealed(false);
                *self.view.games.borrow_mut() = games;
                self.drawn.clear();
                w.stack.set_visible_child_name("rows");
            }
            Loaded::Fetched(Err(e)) => {
                tracing::info!(error = %e, "library not fetched");
                let name = self.shelf().map(|s| s.req.name).unwrap_or_default();
                let pair = matches!(e, LibraryError::PinMismatch | LibraryError::NotPaired);
                if self.view.games.borrow().is_empty() {
                    w.error.set_description(Some(&failure(&e, &name)));
                    w.retry.set_visible(!pair);
                    w.pair.set_visible(pair);
                    w.stack.set_visible_child_name("error");
                } else {
                    w.banner.set_title(&if pair {
                        "Last known library \u{2014} pair again to refresh it".to_string()
                    } else {
                        format!("Last known library \u{2014} {name} didn\u{2019}t answer")
                    });
                    w.banner.set_button_label(pair.then_some("Pair Again"));
                    w.banner.set_revealed(true);
                }
            }
            Loaded::Running(games) => self.set_running(games),
        }
        self.relayout();
    }

    /// The chip bar: one chip per shelf, shown with more than one. Rebuilt only when the shelves
    /// or the selection moved, so a store write mid-click keeps the chip under the pointer.
    fn draw_chips(&mut self) {
        let shelves: Vec<(String, String)> = self
            .view
            .shelves
            .borrow()
            .iter()
            .map(|s| (s.key.clone(), s.label.clone()))
            .collect();
        let next = (shelves, self.view.selected.borrow().clone());
        if self.chips_drawn.as_ref() == Some(&next) {
            return;
        }
        let holder = &self.widgets.chips;
        while let Some(c) = holder.first_child() {
            holder.remove(&c);
        }
        holder.set_visible(next.0.len() > 1);
        if next.0.len() > 1 {
            holder.append(&tile::chips(&self.view));
        }
        self.chips_drawn = Some(next);
    }

    /// Keep what is up, one row per title; the endable row wins, since it carries End Game.
    fn set_running(&mut self, games: Vec<RunningGame>) {
        self.drawn.clear();
        let mut by_id: HashMap<String, RunningGame> = HashMap::new();
        for g in games.into_iter().filter(RunningGame::is_up) {
            let Some(id) = g.app_id.clone() else { continue };
            if by_id.get(&id).is_none_or(|kept| !kept.endable) {
                by_id.insert(id, g);
            }
        }
        *self.view.running.borrow_mut() = by_id;
    }

    fn relayout(&mut self) {
        let started = std::time::Instant::now();
        let sections = layout::sections(&self.store.settings().library_sections);
        let rows = {
            let (games, favorites) = (self.view.games.borrow(), self.view.favorites.borrow());
            rows::rows(&rows::Layout {
                games: &games,
                sections: &sections,
                favorites: &favorites,
                sort: self.sort,
                group: self.group,
                search: &self.search,
                columns: self.columns,
                paired_hosts: self.view.desktops.borrow().len(),
            })
        };
        let built = started.elapsed().as_secs_f64() * 1000.0;
        self.view.columns.set(self.columns);
        let changed = rows != self.drawn;
        if changed {
            self.canvas.set_rows(rows.clone());
        }
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        tracing::debug!(rows = rows.len(), changed, built, ms, "library laid out");
        self.drawn = rows;
    }

    fn title(&self, id: &str) -> String {
        self.view
            .games
            .borrow()
            .iter()
            .find(|g| g.id == id)
            .map_or_else(|| id.to_string(), |g| g.title.clone())
    }

    /// A title's `punktfunk://` link and a launcher label for it. A pinned shelf's preset rides
    /// the link: copying off that shelf is pressing its card and picking the title.
    fn link(&self, id: &str) -> Option<(String, String)> {
        let shelf = self.shelf()?;
        let url = pf_client_core::deeplink::saved_host_link(
            &self.store.hosts(),
            shelf.req.fp_hex.as_deref(),
            &shelf.req.addr,
            shelf.req.port,
            shelf.req.preset.as_deref(),
            Some(id),
        )?;
        Some((url, format!("{} on {}", self.title(id), shelf.label)))
    }

    /// Ending a game can lose unsaved progress, so it asks first.
    fn confirm_end(&self, id: &str) {
        let Some(shelf) = self.shelf() else {
            return;
        };
        let title = self.title(id);
        let dialog = adw::AlertDialog::new(
            Some(&format!("End {title}?")),
            Some("Unsaved progress in the game is lost."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("end", "End Game")]);
        dialog.set_response_appearance("end", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let (sender, id, identity, generation) = (
            self.view.sender.clone(),
            id.to_string(),
            self.identity.clone(),
            self.generation,
        );
        dialog.connect_response(Some("end"), move |_, _| {
            let (sender, id, identity, title) =
                (sender.clone(), id.clone(), identity.clone(), title.clone());
            let (addr, port) = (shelf.req.addr.clone(), shelf.mgmt_port);
            let pin = shelf
                .req
                .fp_hex
                .as_deref()
                .and_then(crate::trust::parse_hex32);
            std::thread::Builder::new()
                .name("punktfunk-endgame".into())
                .spawn(move || {
                    let outcome = library::end_game(&addr, port, &identity, pin, &id);
                    let running = library::fetch_running(&addr, port, &identity, pin);
                    let _ = sender.send(LibraryMsg::Ended(generation, outcome, title, running));
                })
                .expect("spawn end-game thread");
        });
        dialog.present(Some(&self.widgets.root));
    }
}

#[cfg(test)]
mod tests {
    use super::columns_for;

    /// A row of n tiles is n·150 plus n−1 gaps of 16 plus 8 each side, and must fit.
    #[test]
    fn columns_fit_the_width() {
        for n in 1..12usize {
            let need = (n * 150 + (n - 1) * 16 + 16) as f64;
            assert_eq!(columns_for(need), n);
            assert_eq!(columns_for(need - 1.0), (n - 1).max(1));
        }
    }
}
