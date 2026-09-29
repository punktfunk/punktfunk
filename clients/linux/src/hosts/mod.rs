//! The hosts page: saved hosts as cards in the device's order and bands, the hosts on this
//! network below them, and each saved host's page one ⓘ away (design §2.2–2.4). Cards are
//! built from [`model`] values; a build equal to the last redraws nothing, so an open menu
//! survives an unchanged probe sweep. What an advert or a probe teaches is written when it
//! arrives, never while drawing. Actions leave as typed [`HostsOutput`]s.

mod card;
mod detail;
mod dialogs;
mod form;
mod model;
pub mod speed;

use crate::discovery::{self, DiscoveredHost, DiscoveryEvent};
use crate::store::{Changed, Store};
use crate::trust::{self, HostEdit, KnownHost, KnownHosts, Settings};
use adw::prelude::*;
pub(crate) use card::os_icon_name;
use gtk::{gio, glib};
pub use model::Phase;
use model::{Band, CardModel, Live, Preset, Status};
use pf_client_core::host_order;
use relm4::prelude::*;
use std::collections::HashMap;
use std::rc::Rc;

/// What the user asked to connect to. `fp_hex` comes from the mDNS TXT record when the
/// host was discovered (drives the trust decision *before* connecting); manual entries
/// have none. `pair_optional` is true ONLY when a discovered host advertised
/// `pair=optional` — the sole case in which the reduced-security TOFU path may be
/// offered; every other case mandates PIN pairing.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectRequest {
    pub name: String,
    pub addr: String,
    pub port: u16,
    pub fp_hex: Option<String>,
    pub pair_optional: bool,
    /// A library title id to launch on connect.
    pub launch: Option<String>,
    /// Wake-on-LAN MAC(s) for this host. Empty when none is known.
    pub mac: Vec<String>,
    /// A ONE-OFF settings preset for this connect ("Connect with ▸ X"): `Some(id)` overrides
    /// the host's binding for this launch, `Some("")` forces the global defaults on a bound
    /// host, `None` honors the binding. It never rebinds anything — the host's default changes
    /// only through an explicit pick on its page (design/client-settings-profiles.md §5.2).
    pub preset: Option<String>,
}

/// A saved host's plain connect: its fingerprint is already pinned, so this is the silent
/// pinned dial a card's click makes. `preset: None` honours the host's own binding.
///
/// Free rather than a method so the shell's start screen can build one before any card exists.
pub fn saved_request(k: &trust::KnownHost) -> ConnectRequest {
    ConnectRequest {
        name: k.name.clone(),
        addr: k.addr.clone(),
        port: k.port,
        // `None` for a record saved by address and never paired, so `card_key` keys it by
        // address. Same shape the discovered cards use.
        fp_hex: (!k.fp_hex.is_empty()).then(|| k.fp_hex.clone()),
        pair_optional: false,
        launch: None,
        mac: k.mac.clone(),
        preset: None,
    }
}

impl ConnectRequest {
    /// The key the page tracks an in-flight connect under (the card that swaps its
    /// avatar for a spinner): the fingerprint when known, else the address.
    pub fn card_key(&self) -> String {
        self.fp_hex
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.addr, self.port))
    }
}

/// Which saved record an act is about: its stable id, with `addr:port` behind it for a
/// record older than ids. Never a fingerprint, which an unpaired record leaves empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostRef {
    pub id: Option<String>,
    pub addr: String,
    pub port: u16,
}

impl HostRef {
    pub fn of(k: &KnownHost) -> HostRef {
        HostRef {
            id: k.id.clone(),
            addr: k.addr.clone(),
            port: k.port,
        }
    }

    pub fn index(&self, known: &KnownHosts) -> Option<usize> {
        known.index_of_card(self.id.as_deref(), &self.addr, self.port)
    }
}

/// What a card or a host page asks for. One set, so the two can never offer different acts.
#[derive(Debug)]
pub enum Act {
    Connect(ConnectRequest),
    WakeConnect(ConnectRequest),
    Library(ConnectRequest),
    Pair(ConnectRequest),
    SpeedTest(ConnectRequest),
    SendLogs(ConnectRequest),
    /// One of the host's own actions (`design/host-actions.md` §7); `danger` asks first.
    HostAction {
        req: ConnectRequest,
        action_id: String,
        label: String,
        danger: bool,
    },
    Wake {
        mac: Vec<String>,
        addr: String,
    },
    CopyLink {
        host: HostRef,
        preset: Option<String>,
    },
    CreateShortcut {
        host: HostRef,
        preset: Option<String>,
    },
    Details(HostRef),
    Unpin {
        host: HostRef,
        preset_id: String,
    },
    Toast(String),
}

/// How long each saved-host reachability probe waits, and how often the sweep runs. Presence is
/// this sweep and nothing else, so a host reached only over a routed network (Tailscale/VPN) —
/// which never appears on mDNS — shows Online, and a sleeping one shows Offline within a cycle.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(12);

pub struct HostsInit {
    pub store: Rc<Store>,
    /// The window's navigation, which host pages push onto.
    pub nav: adw::NavigationView,
    pub views: adw::ViewStack,
    pub narrow: adw::Breakpoint,
}

pub struct HostsPage {
    store: Rc<Store>,
    nav: adw::NavigationView,
    sender: relm4::Sender<HostsMsg>,
    adverts: HashMap<String, DiscoveredHost>,
    /// Saved hosts proven reachable by the periodic QUIC probe (mDNS-independent), keyed by
    /// [`KnownHost::card_key`].
    probed: HashMap<String, bool>,
    /// This device's session, by card key.
    session: Option<(String, Phase)>,
    /// What is on screen; a build equal to it redraws nothing.
    drawn: Option<(Vec<Band>, Vec<CardModel>, Vec<Preset>)>,
    detail: Option<detail::DetailPage>,
    widgets: PageWidgets,
    /// Forces the mDNS browse to re-query (the header's Refresh button). `None` only if the
    /// browse never started — the button then just re-renders.
    rescan: Option<discovery::Rescan>,
}

struct PageWidgets {
    stack: gtk::Stack,
    banner: adw::Banner,
    saved: gtk::Box,
    discovered: gtk::FlowBox,
    searching: gtk::Box,
    arrange: gtk::MenuButton,
    sort: gio::SimpleAction,
    group: gio::SimpleAction,
}

#[derive(Debug)]
pub enum HostsMsg {
    /// A resolved mDNS advert (also the CI scenes' injection path).
    Advert(DiscoveredHost),
    AdvertRemoved {
        fullname: String,
    },
    /// Re-render: the store changed, or the console handed the window back.
    Refresh,
    /// Re-query mDNS *and* re-render — the header's Refresh button. After a while `mdns-sd`
    /// re-queries about once an hour, so a host that appeared since needs an actual query.
    Rescan,
    /// A completed reachability sweep: saved-host key → reachable.
    Probed(HashMap<String, bool>),
    /// This device's session with the card of `ConnectRequest::card_key`; `None` when none
    /// runs.
    SetSession(Option<(String, Phase)>),
    ShowError(String),
    ClearError,
    ShowAddHost,
    /// A host page was popped off the navigation.
    DetailClosed,
    Act(Act),
}

#[derive(Debug)]
pub enum HostsOutput {
    Connect(ConnectRequest),
    /// A one-line confirmation for the window's toast overlay.
    Toast(String),
    WakeConnect(ConnectRequest),
    Pair(ConnectRequest),
    SpeedTest(ConnectRequest),
    Library(ConnectRequest),
    /// With the advertised mgmt port when a live advert carries one.
    SendLogs(ConnectRequest, Option<u16>),
    /// Run one of the host's own actions — same mgmt-port resolution as [`HostsOutput::SendLogs`].
    HostAction {
        req: ConnectRequest,
        mgmt: Option<u16>,
        action_id: String,
        label: String,
        danger: bool,
    },
}

impl SimpleComponent for HostsPage {
    type Init = HostsInit;
    type Input = HostsMsg;
    type Output = HostsOutput;
    type Root = adw::ToolbarView;
    type Widgets = ();

    fn init_root() -> Self::Root {
        adw::ToolbarView::new()
    }

    fn init(
        init: Self::Init,
        page: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let HostsInit {
            store,
            nav,
            views,
            narrow,
        } = init;
        let heading = |text: &str| {
            let l = gtk::Label::new(Some(text));
            l.add_css_class("heading");
            l.set_halign(gtk::Align::Start);
            l
        };
        let disc_heading = heading("On this network");
        // Bands of saved cards, redrawn as a whole when the models change.
        let saved = gtk::Box::new(gtk::Orientation::Vertical, 12);
        let discovered = card_grid();

        // Shown under the discovered heading while no (unsaved) advert is live yet.
        let searching = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        searching.append(&adw::Spinner::new());
        let searching_label = gtk::Label::new(Some("Searching the network\u{2026}"));
        searching_label.add_css_class("dim-label");
        searching.append(&searching_label);
        searching.set_margin_top(6);
        searching.set_margin_bottom(6);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.append(&saved);
        content.append(&disc_heading);
        content.append(&searching);
        content.append(&discovered);

        let clamp = adw::Clamp::builder()
            .maximum_size(1200)
            .child(&content)
            .build();
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&clamp)
            .build();

        // No saved hosts AND nothing on the network → the whole page is the empty state.
        let empty = adw::StatusPage::builder()
            .icon_name("network-workgroup-symbolic")
            .title("No hosts yet")
            .description(
                "Hosts on your network appear here automatically.\nAdd one by address with +.",
            )
            .build();
        let add_btn = gtk::Button::with_label("Add Host");
        add_btn.add_css_class("pill");
        add_btn.add_css_class("suggested-action");
        add_btn.set_halign(gtk::Align::Center);
        add_btn.set_action_name(Some("win.add-host"));
        empty.set_child(Some(&add_btn));

        let stack = gtk::Stack::new();
        stack.add_named(&scrolled, Some("grid"));
        stack.add_named(&empty, Some("empty"));

        // Connect failures land here, not in toasts.
        let banner = adw::Banner::new("");
        banner.set_button_label(Some("Dismiss"));
        banner.connect_button_clicked(|b| b.set_revealed(false));

        let (arrange, sort, group) = arrange_menu(&store);
        page.insert_action_group(
            "hosts",
            Some(&{
                let g = gio::SimpleActionGroup::new();
                g.add_action(&sort);
                g.add_action(&group);
                g
            }),
        );

        let (header, bar) = crate::widgets::chrome::destination_header(&views, &narrow);
        let add_host_btn = crate::widgets::lucide::button("plus");
        add_host_btn.set_tooltip_text(Some("Add host"));
        add_host_btn.set_action_name(Some("win.add-host"));
        header.pack_start(&add_host_btn);
        let rescan_btn = crate::widgets::lucide::button("refresh-cw");
        rescan_btn.set_tooltip_text(Some("Scan the network for hosts again"));
        {
            let sender = sender.clone();
            rescan_btn.connect_clicked(move |_| sender.input(HostsMsg::Rescan));
        }
        header.pack_start(&rescan_btn);
        header.pack_start(&arrange);
        // Packed first so the menu stays rightmost (pack_end fills inward).
        header.pack_end(&crate::widgets::chrome::primary_menu());
        if cfg!(feature = "console") {
            // The couch UI's front door, beside the page's other actions.
            let console_btn = crate::widgets::lucide::button("gamepad-2");
            console_btn
                .set_tooltip_text(Some("Console UI — the controller-driven couch interface"));
            console_btn.set_action_name(Some("win.console"));
            header.pack_end(&console_btn);
        }

        page.add_top_bar(&header);
        page.add_top_bar(&banner);
        page.add_bottom_bar(&bar);
        page.set_content(Some(&stack));
        {
            let sender = sender.clone();
            nav.connect_popped(move |_, popped| {
                if popped.tag().as_deref() == Some("host") {
                    sender.input(HostsMsg::DetailClosed);
                }
            });
        }
        {
            let sender = sender.clone();
            store.subscribe(move |_| sender.input(HostsMsg::Refresh));
        }

        // Stream mDNS adverts into the model; every add/remove re-evaluates both grids.
        let (rx, rescan) = discovery::browse();
        {
            let sender = sender.clone();
            glib::spawn_future_local(async move {
                while let Ok(event) = rx.recv().await {
                    match event {
                        DiscoveryEvent::Resolved(h) => sender.input(HostsMsg::Advert(h)),
                        DiscoveryEvent::Removed { fullname } => {
                            sender.input(HostsMsg::AdvertRemoved { fullname })
                        }
                    }
                }
            });
        }

        // The reachability sweep — the only thing presence is made of, since an advert outlives
        // the machine it describes. Each cycle probes every saved host off the main thread
        // (bounded QUIC handshake, then the addresses a silent host left); the first sweep runs
        // at once, then every `PROBE_INTERVAL`.
        {
            let (sender, store) = (sender.clone(), store.clone());
            glib::spawn_future_local(async move {
                loop {
                    let hosts: Vec<KnownHost> = store
                        .hosts()
                        .hosts
                        .iter()
                        .filter(|h| !h.addr.is_empty())
                        .cloned()
                        .collect();
                    if !hosts.is_empty() {
                        let (tx, rx) = async_channel::bounded(1);
                        std::thread::Builder::new()
                            .name("punktfunk-probe".into())
                            .spawn(move || {
                                let results = crate::trust::probe_known(&hosts, PROBE_TIMEOUT);
                                let map: HashMap<String, bool> =
                                    hosts.iter().map(KnownHost::card_key).zip(results).collect();
                                let _ = tx.send_blocking(map);
                            })
                            .expect("spawn probe thread");
                        if let Ok(map) = rx.recv().await {
                            sender.input(HostsMsg::Probed(map));
                        }
                    }
                    glib::timeout_future(PROBE_INTERVAL).await;
                }
            });
        }

        let mut model = HostsPage {
            store,
            nav,
            sender: sender.input_sender().clone(),
            adverts: HashMap::new(),
            probed: HashMap::new(),
            session: None,
            drawn: None,
            detail: None,
            widgets: PageWidgets {
                stack,
                banner,
                saved,
                discovered,
                searching,
                arrange,
                sort,
                group,
            },
            rescan: Some(rescan),
        };
        model.rebuild();
        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: HostsMsg, sender: ComponentSender<Self>) {
        match msg {
            HostsMsg::Advert(h) => {
                self.learn(&h);
                self.adverts.insert(h.key.clone(), h);
                self.rebuild();
            }
            HostsMsg::AdvertRemoved { fullname } => {
                self.adverts.retain(|_, a| a.fullname != fullname);
                self.rebuild();
            }
            HostsMsg::Refresh => self.rebuild(),
            HostsMsg::Rescan => {
                if let Some(rescan) = &self.rescan {
                    rescan.request();
                }
                self.rebuild();
            }
            HostsMsg::Probed(map) => {
                self.probed = map;
                // A host saved after its advert arrived learns from it here, within a sweep.
                let adverts: Vec<DiscoveredHost> = self.adverts.values().cloned().collect();
                for a in &adverts {
                    self.learn(a);
                }
                self.refresh_reachable();
                self.rebuild();
            }
            HostsMsg::SetSession(session) => {
                self.session = session;
                self.rebuild();
            }
            HostsMsg::ShowError(msg) => {
                self.widgets.banner.set_title(&msg);
                self.widgets.banner.set_revealed(true);
            }
            HostsMsg::ClearError => self.widgets.banner.set_revealed(false),
            HostsMsg::ShowAddHost => self.add_host_dialog(&sender),
            HostsMsg::DetailClosed => self.detail = None,
            HostsMsg::Act(act) => self.act(act, &sender),
        }
    }
}

/// A FlowBox of cards. Clicks and Enter reach each child's own `activate` through the guarded
/// bridge (bare, the two signals recurse until the stack overflows).
fn card_grid() -> gtk::FlowBox {
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .activate_on_single_click(true)
        .homogeneous(true)
        .min_children_per_line(1)
        .max_children_per_line(4)
        .column_spacing(12)
        .row_spacing(12)
        .valign(gtk::Align::Start)
        .build();
    // Scopes the concentric hover-highlight radius (see data/style.css).
    flow.add_css_class("pf-host-grid");
    crate::widgets::flow::bridge_child_activation(&flow);
    flow
}

/// The header's Sort and Group menu, two radio sets over the device's `host_sort` and
/// `host_grouping` keys — the same order the console shows.
fn arrange_menu(store: &Rc<Store>) -> (gtk::MenuButton, gio::SimpleAction, gio::SimpleAction) {
    let settings = store.settings();
    let radio = |name: &str, key: &'static str, value: &str| {
        let action = gio::SimpleAction::new_stateful(
            name,
            Some(glib::VariantTy::STRING),
            &value.to_variant(),
        );
        let store = store.clone();
        action.connect_change_state(move |action, value| {
            let Some(value) = value.and_then(|v| v.str()).map(str::to_string) else {
                return;
            };
            action.set_state(&value.to_variant());
            store.update_settings(|s| {
                s.extra.insert(key.into(), value.into());
            });
        });
        action
    };
    let sort = radio(
        "sort",
        host_order::HOST_SORT_KEY,
        host_order::sort(&settings),
    );
    let group = radio(
        "group",
        host_order::HOST_GROUPING_KEY,
        host_order::grouping(&settings),
    );
    let menu = gio::Menu::new();
    for (title, action, values) in [
        ("Sort", "hosts.sort", &host_order::HOST_SORTS),
        ("Group", "hosts.group", &host_order::HOST_GROUPINGS),
    ] {
        let section = gio::Menu::new();
        for (id, label) in values {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some(action), Some(&id.to_variant()));
            section.append_item(&item);
        }
        menu.append_section(Some(title), &section);
    }
    let button = gtk::MenuButton::builder()
        .child(&crate::widgets::lucide::row_icon("arrow-up-down"))
        .menu_model(&menu)
        .tooltip_text("Sort and group")
        .build();
    (button, sort, group)
}

impl HostsPage {
    /// What a live advert teaches the saved host it matches: its wake MACs, its OS chain (so
    /// the icon survives it going offline), its management port, and an address the probe
    /// sweep asks. `learn_from_advert` writes only when something moved.
    fn learn(&self, a: &DiscoveredHost) {
        let target = self
            .store
            .hosts()
            .hosts
            .iter()
            .find(|k| discovery::same_host(k, a))
            .map(|k| (k.fp_hex.clone(), k.addr.clone(), k.port));
        let Some((fp, addr, port)) = target else {
            return;
        };
        trust::learn_from_advert(&fp, &addr, port, &a.addr, &a.mac, &a.os, a.mgmt_port);
        self.store.reload(Changed::Hosts);
    }

    /// Keep each paired, reachable host's own actions and what it has up warm, so a menu or a
    /// status is built from a settled answer. Both caches are TTL-gated and fetch on a worker.
    fn refresh_reachable(&self) {
        for k in self.store.hosts().hosts.iter() {
            if !k.paired || !self.probed.get(&k.card_key()).copied().unwrap_or(false) {
                continue;
            }
            let mgmt = self.mgmt_port(&k.fp_hex, &k.addr, k.port);
            pf_client_core::host_actions::refresh(&k.addr, mgmt, &k.fp_hex);
            pf_client_core::library::refresh_running(&k.addr, mgmt, &k.fp_hex);
        }
    }

    fn presets(&self) -> Vec<Preset> {
        self.store
            .presets()
            .presets
            .iter()
            .map(|p| Preset {
                id: p.id.clone(),
                name: p.name.clone(),
                accent: p.accent.clone(),
            })
            .collect()
    }

    /// Build the models, redraw the cards when they changed, and refresh an open host page.
    fn rebuild(&mut self) {
        let hosts = self.store.hosts().hosts.clone();
        let settings = self.store.settings().clone();
        let presets = self.presets();
        let playing = |fp: &str| pf_client_core::library::now_playing(fp);
        let live = Live {
            probed: &self.probed,
            session: self
                .session
                .as_ref()
                .map(|(key, phase)| (key.as_str(), *phase)),
            playing: &playing,
        };
        let bands = model::saved_bands(&hosts, &presets, &settings, &live);
        let dialing = match &self.session {
            Some((key, Phase::Connecting)) => Some(key.as_str()),
            _ => None,
        };
        let discovered = model::discovered_cards(self.adverts.values(), &hosts, dialing);
        let next = (bands, discovered, presets);
        if self.drawn.as_ref() != Some(&next) {
            self.draw(&next);
            self.drawn = Some(next);
        }
        self.sync_arrange(&settings, hosts.len());
        self.refresh_detail(&hosts, &settings);
    }

    fn draw(&self, (bands, discovered, presets): &(Vec<Band>, Vec<CardModel>, Vec<Preset>)) {
        let w = &self.widgets;
        while let Some(child) = w.saved.first_child() {
            w.saved.remove(&child);
        }
        for band in bands {
            let heading = gtk::Label::new(Some(band.title.as_deref().unwrap_or("Saved hosts")));
            heading.add_css_class("heading");
            heading.set_halign(gtk::Align::Start);
            w.saved.append(&heading);
            let grid = card_grid();
            for m in &band.cards {
                grid.append(&card::build(m, presets, &self.sender));
            }
            w.saved.append(&grid);
        }
        w.discovered.remove_all();
        for m in discovered {
            w.discovered.append(&card::build(m, presets, &self.sender));
        }
        let (have_saved, have_disc) = (!bands.is_empty(), !discovered.is_empty());
        w.discovered.set_visible(have_disc);
        w.searching.set_visible(!have_disc);
        w.stack.set_visible_child_name(if have_saved || have_disc {
            "grid"
        } else {
            "empty"
        });
    }

    /// The radio marks follow the settings file, which the console writes too.
    fn sync_arrange(&self, settings: &Settings, saved: usize) {
        let w = &self.widgets;
        w.arrange.set_visible(saved > 1);
        w.sort.set_state(&host_order::sort(settings).to_variant());
        w.group
            .set_state(&host_order::grouping(settings).to_variant());
    }

    /// Refresh an open host page, or leave it when its host is gone.
    fn refresh_detail(&mut self, hosts: &[KnownHost], settings: &Settings) {
        let Some(page) = &self.detail else {
            return;
        };
        let known = KnownHosts {
            hosts: hosts.to_vec(),
        };
        let Some(k) = page.host().index(&known).map(|i| &known.hosts[i]) else {
            self.nav.pop_to_tag("main");
            self.detail = None;
            return;
        };
        let live = self.detail_live(k, settings);
        page.refresh(k, &live);
    }

    fn detail_live(&self, k: &KnownHost, settings: &Settings) -> detail::Live {
        let online = self.probed.get(&k.card_key()).copied().unwrap_or(false);
        let playing = if k.fp_hex.is_empty() {
            String::new()
        } else {
            pf_client_core::library::now_playing(&k.fp_hex)
        };
        let phase = match &self.session {
            Some((key, phase)) if *key == k.card_key() => Some(*phase),
            _ => None,
        };
        detail::Live {
            status: Status::of(k, online, phase, &playing, settings.auto_wake),
            online,
            presets: self.presets(),
            actions: if k.fp_hex.is_empty() {
                Vec::new()
            } else {
                pf_client_core::host_actions::cached(&k.fp_hex)
            },
            is_default: k.id.is_some() && settings.default_host == k.id,
            start_in: pf_client_core::start::StartIn::parse(&settings.start_in),
        }
    }

    fn act(&mut self, act: Act, sender: &ComponentSender<Self>) {
        let out = |o| {
            let _ = sender.output(o);
        };
        match act {
            Act::Connect(req) => out(HostsOutput::Connect(req)),
            Act::WakeConnect(req) => out(HostsOutput::WakeConnect(req)),
            Act::Pair(req) => out(HostsOutput::Pair(req)),
            Act::SpeedTest(req) => out(HostsOutput::SpeedTest(req)),
            Act::Library(req) => out(HostsOutput::Library(req)),
            Act::SendLogs(req) => {
                let mgmt = self.mgmt_port_for(&req);
                out(HostsOutput::SendLogs(req, mgmt));
            }
            Act::HostAction {
                req,
                action_id,
                label,
                danger,
            } => {
                let mgmt = self.mgmt_port_for(&req);
                out(HostsOutput::HostAction {
                    req,
                    mgmt,
                    action_id,
                    label,
                    danger,
                });
            }
            Act::Wake { mac, addr } => crate::wol::wake(&mac, addr.parse().ok()),
            Act::CopyLink { host, preset } => {
                if let Some((url, _)) = self.link(&host, preset.as_deref()) {
                    if let Some(display) = gtk::gdk::Display::default() {
                        display.clipboard().set_text(&url);
                    }
                    out(HostsOutput::Toast("Link copied".into()));
                }
            }
            Act::CreateShortcut { host, preset } => {
                if let Some((url, label)) = self.link(&host, preset.as_deref()) {
                    self.shortcut_result(sender, &label, &url);
                }
            }
            Act::Details(host) => self.open_detail(&host),
            Act::Unpin { host, preset_id } => {
                let saved = self.store.update_hosts(|known| {
                    if let Some(i) = host.index(known) {
                        known.hosts[i].pinned_presets.retain(|p| p != &preset_id);
                    }
                });
                if let Err(e) = saved {
                    out(HostsOutput::Toast(format!("Couldn't save \u{2014} {e:#}")));
                }
            }
            Act::Toast(msg) => out(HostsOutput::Toast(msg)),
        }
    }

    /// The host's self-emitted `punktfunk://` link and a launcher label for it. It carries the
    /// stable id AND host+fp, so it still resolves after a re-address or a reinstall.
    fn link(&self, host: &HostRef, preset: Option<&str>) -> Option<(String, String)> {
        let known = self.store.hosts();
        let k = &known.hosts[host.index(&known)?];
        let url = pf_client_core::deeplink::DeepLink::for_host(k, None, preset).to_url();
        let label = match preset.and_then(|id| self.presets().into_iter().find(|p| p.id == id)) {
            Some(p) => format!("{} \u{b7} {}", k.name, p.name),
            None => k.name.clone(),
        };
        Some((url, label))
    }

    fn open_detail(&mut self, host: &HostRef) {
        let k = {
            let known = self.store.hosts();
            match host.index(&known) {
                Some(i) => known.hosts[i].clone(),
                None => return,
            }
        };
        let page = detail::DetailPage::open(&self.nav, self.store.clone(), &k, self.sender.clone());
        let settings = self.store.settings().clone();
        page.refresh(&k, &self.detail_live(&k, &settings));
        self.detail = Some(page);
    }

    /// The mgmt port for `req`'s host: a matching live advert's `mgmt` TXT first, else the port
    /// an earlier advert taught the saved record. Over a VPN or any multicast-dead network there
    /// is no advert, and a host that moved off 47990 would otherwise lose its library there.
    fn mgmt_port_for(&self, req: &ConnectRequest) -> Option<u16> {
        self.learned_mgmt_port(req.fp_hex.as_deref().unwrap_or(""), &req.addr, req.port)
    }

    fn learned_mgmt_port(&self, fp: &str, addr: &str, port: u16) -> Option<u16> {
        let matches =
            |f: &str, a: &str, p: u16| (!fp.is_empty() && f == fp) || (a == addr && p == port);
        self.adverts
            .values()
            .find(|a| matches(&a.fp_hex, &a.addr, a.port))
            .and_then(|a| a.mgmt_port)
            .or_else(|| {
                self.store
                    .hosts()
                    .hosts
                    .iter()
                    .find(|h| matches(&h.fp_hex, &h.addr, h.port))
                    .and_then(|h| h.mgmt_port)
            })
    }

    /// [`Self::mgmt_port_for`] with the default filled in.
    fn mgmt_port(&self, fp: &str, addr: &str, port: u16) -> u16 {
        self.learned_mgmt_port(fp, addr, port)
            .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT)
    }
}
