//! The hosts page as a relm4 component: adaptive card grids for saved (trusted/paired)
//! and mDNS-discovered hosts — avatar + name + `addr:port` + status pills, online pips,
//! dashed discovered cards, an overflow menu, an add-host dialog, and a connect-failure
//! banner. Cards are a [`FactoryVecDeque`]; both grids re-populate from one state
//! snapshot (the [`Store`] + the live advert map) on every change, so dedup and the online
//! pips stay consistent. What an advert or a probe teaches is written when it arrives,
//! never while drawing. Actions leave as typed [`HostsOutput`]s.

mod card;
mod dialogs;
mod form;

use crate::discovery::{self, DiscoveredHost, DiscoveryEvent};
use crate::store::{Changed, Store};
use crate::trust::{self, HostEdit, KnownHost, KnownHosts};
use adw::prelude::*;
use card::{CardKind, CardOutput, HostCard, Preset};
use gtk::{gio, glib};
use pf_client_core::start;
use relm4::factory::FactoryVecDeque;
use relm4::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// What the user asked to connect to. `fp_hex` comes from the mDNS TXT record when the
/// host was discovered (drives the trust decision *before* connecting); manual entries
/// have none. `pair_optional` is true ONLY when a discovered host advertised
/// `pair=optional` — the sole case in which the reduced-security TOFU path may be
/// offered; every other case mandates PIN pairing.
#[derive(Clone, Debug)]
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
    /// only through an explicit "Default preset" pick (design/client-settings-profiles.md §5.2).
    pub preset: Option<String>,
}

/// A saved host's plain connect: its fingerprint is already pinned, so this is the silent
/// pinned dial a card's click makes. `preset: None` honours the host's own binding, which
/// is what a click without "Connect with" does — the card overrides it for a pinned card.
///
/// Free rather than a method so the shell's start screen can build one before any card exists.
pub fn saved_request(k: &trust::KnownHost) -> ConnectRequest {
    ConnectRequest {
        name: k.name.clone(),
        addr: k.addr.clone(),
        port: k.port,
        // `None` for a record saved by address and never paired, so `card_key` keys it by
        // address. Same shape the Discovered arm already uses.
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

/// How long each saved-host reachability probe waits, and how often the sweep runs. The pip reads
/// this sweep and nothing else, so a host reached only over a routed network (Tailscale/VPN) —
/// which never appears on mDNS — shows Online, and a sleeping one shows Offline within a cycle.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(12);

pub struct HostsPage {
    adverts: HashMap<String, DiscoveredHost>,
    /// Saved hosts proven reachable by the periodic QUIC probe (mDNS-independent), keyed by
    /// [`KnownHost::card_key`]. OR'd with live-advert presence to drive the Online pip.
    probed: HashMap<String, bool>,
    connecting: Option<String>,
    saved: FactoryVecDeque<HostCard>,
    discovered: FactoryVecDeque<HostCard>,
    widgets: PageWidgets,
    /// Forces the mDNS browse to re-query (the header's Refresh button). `None` only if the
    /// browse never started — the button then just re-renders, which is what it did before.
    rescan: Option<discovery::Rescan>,
    store: Rc<Store>,
}

struct PageWidgets {
    stack: gtk::Stack,
    banner: adw::Banner,
    saved_heading: gtk::Label,
    disc_heading: gtk::Label,
    searching: gtk::Box,
}

#[derive(Debug)]
pub enum HostsMsg {
    /// A resolved mDNS advert (also the CI scenes' injection path).
    Advert(DiscoveredHost),
    AdvertRemoved {
        fullname: String,
    },
    /// Reload the disk store and re-render (fresh pairings, renames).
    Refresh,
    /// Re-query mDNS *and* re-render — the header's Refresh button. Distinct from [`Self::Refresh`],
    /// which only re-reads local state: after a while `mdns-sd` re-queries about once an hour, so a
    /// host that appeared since (or whose announcement was lost) needs an actual query to show up.
    Rescan,
    /// A completed reachability sweep: saved-host key → reachable. Merged into the online pips.
    Probed(HashMap<String, bool>),
    /// Mark the card matching `ConnectRequest::card_key` as connecting; `None` restores.
    SetConnecting(Option<String>),
    ShowError(String),
    ClearError,
    ShowAddHost,
    /// Forwarded card actions (factory outputs).
    Card(CardOutput),
}

#[derive(Debug)]
pub enum HostsOutput {
    Connect(ConnectRequest),
    /// A one-line confirmation for the window's toast overlay.
    Toast(String),
    WakeConnect(ConnectRequest),
    Pair(ConnectRequest),
    SpeedTest(ConnectRequest),
    /// With the advertised mgmt port when a live advert carries one.
    Library(ConnectRequest, Option<u16>),
    /// With the mgmt port resolved the same way as [`HostsOutput::Library`]'s.
    SendLogs(ConnectRequest, Option<u16>),
    /// Run one of the host's own actions (`design/host-actions.md` §7) — same mgmt-port
    /// resolution as the two above.
    HostAction {
        req: ConnectRequest,
        mgmt: Option<u16>,
        action_id: String,
        label: String,
        danger: bool,
    },
}

impl SimpleComponent for HostsPage {
    type Init = Rc<Store>;
    type Input = HostsMsg;
    type Output = HostsOutput;
    type Root = adw::NavigationPage;
    type Widgets = ();

    fn init_root() -> Self::Root {
        adw::NavigationPage::builder()
            .title("Punktfunk")
            .tag("hosts")
            .build()
    }

    fn init(
        store: Self::Init,
        page: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let make_flow = || {
            let f = gtk::FlowBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .activate_on_single_click(true)
                .homogeneous(true)
                .min_children_per_line(1)
                .max_children_per_line(4)
                .column_spacing(12)
                .row_spacing(12)
                .build();
            // Scopes the concentric hover-highlight radius (see data/style.css).
            f.add_css_class("pf-host-grid");
            f
        };
        let heading = |text: &str| {
            let l = gtk::Label::new(Some(text));
            l.add_css_class("heading");
            l.set_halign(gtk::Align::Start);
            l
        };
        let saved_heading = heading("Saved hosts");
        let disc_heading = heading("On this network");

        let saved = FactoryVecDeque::<HostCard>::builder()
            .launch(make_flow())
            .forward(sender.input_sender(), HostsMsg::Card);
        let discovered = FactoryVecDeque::<HostCard>::builder()
            .launch(make_flow())
            .forward(sender.input_sender(), HostsMsg::Card);

        // A pointer click (and keyboard activate) emits `child-activated` on the
        // *FlowBox*, never the child's own `activate` signal — bridge it back to the
        // child, where each card wires its connect handler. The guard inside the bridge
        // breaks the child-activated ↔ activate ping-pong that otherwise recurses forever
        // (a real stack overflow on every card click; see `ui_flow`'s display test).
        for flow in [saved.widget(), discovered.widget()] {
            crate::widgets::flow::bridge_child_activation(flow);
        }

        // Shown under the discovered heading while no (unsaved) advert is live yet.
        let searching = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        searching.append(&adw::Spinner::new());
        let searching_label = gtk::Label::new(Some("Searching the LAN…"));
        searching_label.add_css_class("dim-label");
        searching.append(&searching_label);
        searching.set_margin_top(6);
        searching.set_margin_bottom(6);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(24);
        content.set_margin_bottom(24);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.append(&saved_heading);
        content.append(saved.widget());
        content.append(&disc_heading);
        content.append(&searching);
        content.append(discovered.widget());

        let clamp = adw::Clamp::builder()
            .maximum_size(1100)
            .child(&content)
            .build();
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&clamp)
            .build();

        // No saved hosts AND nothing on the LAN → the whole page is the empty state.
        let empty = adw::StatusPage::builder()
            .icon_name("network-workgroup-symbolic")
            .title("No hosts yet")
            .description(
                "Hosts on your network appear here automatically.\nAdd one by address with +.",
            )
            .build();
        let add_btn = gtk::Button::with_label("Add host");
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

        let header = adw::HeaderBar::new();
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
        // The couch UI's front door, beside the page's other actions (same placement the
        // WinUI shell gives it). It was previously reachable only as `--browse` on the
        // command line, which is no way to find a mode.
        let console_btn = crate::widgets::lucide::button("gamepad-2");
        console_btn.set_tooltip_text(Some("Console UI — the controller-driven couch interface"));
        console_btn.set_action_name(Some("win.console"));
        let menu = gio::Menu::new();
        if cfg!(feature = "console") {
            menu.append(Some("Console UI"), Some("win.console"));
        }
        menu.append(Some("Preferences"), Some("win.preferences"));
        menu.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
        menu.append(Some("About Punktfunk"), Some("win.about"));
        let menu_btn = gtk::MenuButton::builder()
            .child(&crate::widgets::lucide::row_icon("menu"))
            .menu_model(&menu)
            .primary(true)
            .tooltip_text("Main menu")
            .build();
        // Packed after the menu so the hamburger stays rightmost (pack_end fills inward).
        header.pack_end(&menu_btn);
        if cfg!(feature = "console") {
            header.pack_end(&console_btn);
        }

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&banner);
        toolbar.set_content(Some(&stack));
        page.set_child(Some(&toolbar));

        // Rebuilt every time the page is shown, so fresh TOFU/pairing entries appear on
        // return.
        {
            let sender = sender.clone();
            page.connect_shown(move |_| sender.input(HostsMsg::Refresh));
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

        // Periodic reachability sweep — the ONLY thing presence is made of, since an advert
        // outlives the machine it describes. Each cycle probes every saved host off the main
        // thread (bounded QUIC handshake, then the addresses a silent host left) and feeds results
        // back as `Probed`; the first sweep runs immediately, then every `PROBE_INTERVAL`.
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
            adverts: HashMap::new(),
            probed: HashMap::new(),
            connecting: None,
            saved,
            discovered,
            widgets: PageWidgets {
                stack,
                banner,
                saved_heading,
                disc_heading,
                searching,
            },
            rescan: Some(rescan),
            store: store.clone(),
        };
        {
            let sender = sender.clone();
            store.subscribe(move |_| sender.input(HostsMsg::Refresh));
        }
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
                // Adverts stream in as they answer; re-render now so the local half is current
                // either way.
                self.rebuild();
            }
            HostsMsg::Probed(map) => {
                self.probed = map;
                // A host saved after its advert arrived learns from it here, within a sweep.
                let adverts: Vec<DiscoveredHost> = self.adverts.values().cloned().collect();
                for a in &adverts {
                    self.learn(a);
                }
                self.refresh_host_actions();
                self.rebuild();
            }
            HostsMsg::SetConnecting(key) => {
                self.connecting = key;
                self.rebuild();
            }
            HostsMsg::ShowError(msg) => {
                self.widgets.banner.set_title(&msg);
                self.widgets.banner.set_revealed(true);
            }
            HostsMsg::ClearError => self.widgets.banner.set_revealed(false),
            HostsMsg::ShowAddHost => self.add_host_dialog(&sender),
            HostsMsg::Card(out) => match out {
                CardOutput::Connect(req) => {
                    let _ = sender.output(HostsOutput::Connect(req));
                }
                CardOutput::WakeConnect(req) => {
                    let _ = sender.output(HostsOutput::WakeConnect(req));
                }
                CardOutput::Pair(req) => {
                    let _ = sender.output(HostsOutput::Pair(req));
                }
                CardOutput::SpeedTest(req) => {
                    let _ = sender.output(HostsOutput::SpeedTest(req));
                }
                CardOutput::Library(req) => {
                    let mgmt = self.mgmt_port_for(&req);
                    let _ = sender.output(HostsOutput::Library(req, mgmt));
                }
                CardOutput::SendLogs(req) => {
                    let mgmt = self.mgmt_port_for(&req);
                    let _ = sender.output(HostsOutput::SendLogs(req, mgmt));
                }
                CardOutput::HostAction {
                    req,
                    action_id,
                    label,
                    danger,
                } => {
                    let mgmt = self.mgmt_port_for(&req);
                    let _ = sender.output(HostsOutput::HostAction {
                        req,
                        mgmt,
                        action_id,
                        label,
                        danger,
                    });
                }
                CardOutput::Toast(msg) => {
                    let _ = sender.output(HostsOutput::Toast(msg));
                }
                CardOutput::Edit {
                    id,
                    addr,
                    port,
                    name,
                } => self.edit_host_dialog(&sender, id.as_deref(), &addr, port, &name),
                CardOutput::Forget {
                    id,
                    addr,
                    port,
                    name,
                } => self.forget_dialog(&sender, id.as_deref(), &addr, port, &name),
                // Whole-file writer: rebase on the store before mutating, or a setting another
                // surface just wrote is reverted.
                CardOutput::MakeDefault { id, name } => {
                    let Some(id) = id else {
                        let _ = sender.output(HostsOutput::Toast(format!(
                            "{name} has no record id \u{2014} re-save it"
                        )));
                        return;
                    };
                    let (on, opens) = self.store.update_settings(|s| {
                        let on = s.default_host.as_deref() != Some(id.as_str());
                        s.default_host = on.then_some(id);
                        (
                            on,
                            start::StartIn::parse(&s.start_in) != start::StartIn::Hosts,
                        )
                    });
                    let _ = sender.output(HostsOutput::Toast(match (on, opens) {
                        (true, true) => format!("{name} opens on launch"),
                        (true, false) => format!("{name} is the default host"),
                        (false, _) => format!("{name} is no longer the default host"),
                    }));
                }
                CardOutput::Wake { mac, addr } => crate::wol::wake(&mac, addr.parse().ok()),
                CardOutput::CopyLink(url) => {
                    if let Some(display) = gtk::gdk::Display::default() {
                        display.clipboard().set_text(&url);
                    }
                    let _ = sender.output(HostsOutput::Toast("Link copied".into()));
                }
                CardOutput::CreateShortcut { label, url } => {
                    self.shortcut_result(&sender, &label, &url);
                }
                CardOutput::TogglePin {
                    fp_hex,
                    addr,
                    port,
                    preset_id,
                    pin,
                } => {
                    let saved = self.store.update_hosts(|known| {
                        if let Some(h) = known.hosts.iter_mut().find(|h| {
                            (!fp_hex.is_empty() && h.fp_hex == fp_hex)
                                || (h.addr == addr && h.port == port)
                        }) {
                            h.pinned_presets.retain(|p| p != &preset_id);
                            if pin {
                                h.pinned_presets.push(preset_id);
                            }
                        }
                    });
                    if let Err(e) = saved {
                        tracing::warn!(error = %format!("{e:#}"), "pinned cards not saved");
                    }
                }
            },
        }
    }
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

    /// Keep each paired, reachable host's advertised actions warm, so its menu is built from
    /// a settled answer rather than one that arrives while the menu is open. Gated on the TTL
    /// inside. The port: live advert, then the stored one, then the default.
    fn refresh_host_actions(&self) {
        let online = |k: &KnownHost| self.probed.get(&k.card_key()).copied().unwrap_or(false);
        for k in self
            .store
            .hosts()
            .hosts
            .iter()
            .filter(|k| k.paired && online(k))
        {
            let mgmt = self
                .adverts
                .values()
                .find(|a| discovery::same_host(k, a))
                .and_then(|a| a.mgmt_port)
                .unwrap_or_else(|| k.effective_mgmt_port());
            pf_client_core::host_actions::refresh(&k.addr, mgmt, &k.fp_hex);
        }
    }

    /// Re-populate both factories from the store and the advert map. Cheap (a handful of
    /// widgets) and keeps every derived view — online pips, dedup, most-recent accent,
    /// spinner — in one straight-line pass. Reads only memory.
    fn rebuild(&mut self) {
        let known = KnownHosts {
            hosts: self.store.hosts().hosts.clone(),
        };
        let default_host = self.store.settings().default_host.clone();
        // A saved host is ONLINE iff a live advert matches it — `same_host` holds the rule
        // (two known fingerprints decide it alone) for every client that browses mDNS.
        let matches = |k: &KnownHost, a: &DiscoveredHost| discovery::same_host(k, a);
        let most_recent = known
            .hosts
            .iter()
            .filter_map(|h| h.last_used.map(|t| (h.fp_hex.clone(), t)))
            .max_by_key(|&(_, t)| t)
            .map(|(fp, _)| fp);
        // One catalog read per refresh, shared by every card's menus and chip.
        let presets: Rc<Vec<Preset>> = Rc::new(
            self.store
                .presets()
                .presets
                .iter()
                .map(|p| Preset {
                    id: p.id.clone(),
                    name: p.name.clone(),
                    accent: p.accent.clone(),
                })
                .collect(),
        );

        {
            let mut saved = self.saved.guard();
            saved.clear();
            for k in &known.hosts {
                // Online = the last probe sweep reached it, and nothing else. An advert is NOT
                // presence: it is a cache entry with a 75-minute PTR TTL that a suspending host
                // sends no goodbye for, so counting it kept a sleeping machine's pip green — and
                // the wake gate reads `!online`, which is how Wake-on-LAN stayed silent for
                // exactly the host it was meant to wake.
                let online = self.probed.get(&k.card_key()).copied().unwrap_or(false);
                let is_default = k.id.is_some() && default_host.as_deref() == k.id.as_deref();
                saved.push_back(HostCard {
                    // The key `ConnectRequest::card_key` mints. A bare `fp_hex` is empty for
                    // every unpaired record.
                    connecting: self.connecting.as_deref() == Some(k.card_key().as_str()),
                    kind: CardKind::Saved {
                        host: k.clone(),
                        online,
                        presets: presets.clone(),
                        recent: most_recent.as_deref() == Some(k.fp_hex.as_str()),
                        is_default,
                        pinned: None,
                    },
                });
                // …then its pinned host+preset cards, in the order the user pinned them.
                // They share the host's live status because they read the same record, and a
                // pin whose preset is gone simply doesn't render (design §5.2a).
                for id in &k.pinned_presets {
                    let Some(p) = presets.iter().find(|p| &p.id == id) else {
                        continue;
                    };
                    let (id, name) = (p.id.clone(), p.name.clone());
                    saved.push_back(HostCard {
                        // The spinner belongs to whichever card was clicked; a pin has its own
                        // key so two cards for one host don't both spin.
                        connecting: false,
                        kind: CardKind::Saved {
                            host: k.clone(),
                            online,
                            presets: presets.clone(),
                            recent: false,
                            is_default,
                            pinned: Some((id, name)),
                        },
                    });
                }
            }
        }

        // The discovered grid only surfaces genuinely-new hosts: anything matching a
        // saved entry renders as that saved card (with its pip now green) instead.
        let mut fresh: Vec<&DiscoveredHost> = self
            .adverts
            .values()
            .filter(|a| !known.hosts.iter().any(|k| matches(k, a)))
            .collect();
        fresh.sort_by(|a, b| a.name.cmp(&b.name).then(a.key.cmp(&b.key)));
        let have_disc = !fresh.is_empty();
        {
            let mut discovered = self.discovered.guard();
            discovered.clear();
            for a in fresh {
                let key = if a.fp_hex.is_empty() {
                    format!("{}:{}", a.addr, a.port)
                } else {
                    a.fp_hex.clone()
                };
                discovered.push_back(HostCard {
                    connecting: self.connecting.as_deref() == Some(key.as_str()),
                    kind: CardKind::Discovered(a.clone()),
                });
            }
        }

        let have_saved = !known.hosts.is_empty();
        let w = &self.widgets;
        w.saved_heading.set_visible(have_saved);
        self.saved.widget().set_visible(have_saved);
        w.disc_heading.set_visible(true);
        self.discovered.widget().set_visible(have_disc);
        w.searching.set_visible(!have_disc);
        w.stack.set_visible_child_name(if have_saved || have_disc {
            "grid"
        } else {
            "empty"
        });
    }

    /// The mgmt port for the host `req` points at: a matching live advert's `mgmt` TXT first,
    /// else the port a previous advert taught us and we saved on the host record.
    ///
    /// The saved rung is not redundant. Reading the advert alone meant a host that had moved its
    /// mgmt port off 47990 served its library on the LAN and nowhere else — over a VPN, a routed
    /// subnet, or any multicast-dead network there is no advert to read, and the fallback silently
    /// went back to a port nothing was listening on. `None` here still means "assume the default".
    fn mgmt_port_for(&self, req: &ConnectRequest) -> Option<u16> {
        let matches_req = |fp: &str, addr: &str, port: u16| {
            req.fp_hex
                .as_deref()
                .is_some_and(|want| !fp.is_empty() && fp == want)
                || (addr == req.addr && port == req.port)
        };
        if let Some(p) = self
            .adverts
            .values()
            .find(|a| matches_req(&a.fp_hex, &a.addr, a.port))
            .and_then(|a| a.mgmt_port)
        {
            return Some(p);
        }
        self.store
            .hosts()
            .hosts
            .iter()
            .find(|h| matches_req(&h.fp_hex, &h.addr, h.port))
            .and_then(|h| h.mgmt_port)
    }
}
