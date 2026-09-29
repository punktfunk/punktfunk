//! The hosts page as a relm4 component: adaptive card grids for saved (trusted/paired)
//! and mDNS-discovered hosts — avatar + name + `addr:port` + status pills, online pips,
//! dashed discovered cards, an overflow menu, an add-host dialog, and a connect-failure
//! banner. Cards are a [`FactoryVecDeque`]; both grids re-populate from one state
//! snapshot (known hosts on disk + the live advert map) on every change, so dedup and
//! the online pips stay consistent. Actions leave as typed [`HostsOutput`]s — the
//! callback bag and `Rc<RefCell<HostsUi>>` pokes of the pre-relm4 shell are gone.

use crate::discovery::{self, DiscoveredHost, DiscoveryEvent};
use crate::trust::{self, KnownHost, KnownHosts, Settings};
use adw::prelude::*;
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

// --- The card factory ---------------------------------------------------------------------

/// One card's full render input — rebuilt (clear + repopulate) on every state change,
/// exactly like the pre-relm4 full-grid rebuild (a handful of widgets; simpler than row
/// surgery and keeps every derived view consistent).
#[derive(Debug)]
pub struct HostCard {
    kind: CardKind,
    connecting: bool,
}

/// One catalog entry as the cards need it: what to call the preset, and the colour its chips
/// carry (`StreamPreset.accent`) — the field the schema reserved for exactly this.
#[derive(Clone, Debug)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub accent: Option<String>,
}

#[derive(Debug)]
enum CardKind {
    Saved {
        host: KnownHost,
        online: bool,
        recent: bool,
        /// The preset catalog as `(id, name)`, for this card's menus and chip. Shared per
        /// refresh rather than re-read per card.
        presets: Rc<Vec<Preset>>,
        /// `Some((id, name))` when this card is a PINNED host+preset pair rather than the
        /// host's primary card (design §5.2a): a one-click shortcut for a preset the user
        /// reaches for often. It is presentation state on the host record — never a second
        /// host entry, which would fork pairing, WoL and renames.
        pinned: Option<(String, String)>,
    },
    Discovered(DiscoveredHost),
}

#[derive(Debug)]
pub enum CardOutput {
    Connect(ConnectRequest),
    WakeConnect(ConnectRequest),
    Pair(ConnectRequest),
    SpeedTest(ConnectRequest),
    Library(ConnectRequest),
    /// Upload this device's recent log ring to the host (`logring::send_to_host`).
    SendLogs(ConnectRequest),
    /// Run one of the host's OWN actions — sleep / restart / shut down it
    /// (`design/host-actions.md` §7). `label` is what the menu called it, so the confirmation
    /// and the toast say the same words the row did.
    HostAction {
        req: ConnectRequest,
        action_id: String,
        label: String,
        /// Ask before running: the action loses whatever is on that machine.
        danger: bool,
    },
    /// Open the host edit sheet (name, preset binding, pinned cards, clipboard).
    ///
    /// Identified by the stable record id with `addr:port` behind it, like [`MakeDefault`] —
    /// NOT by fingerprint, which an unpaired card leaves empty and which then names every
    /// other unpaired record.
    Edit {
        id: Option<String>,
        addr: String,
        port: u16,
        name: String,
    },
    /// Drop one saved record. Identified like [`CardOutput::Edit`], and for the same reason:
    /// keyed by fingerprint, forgetting one unpaired host forgot all of them.
    Forget {
        id: Option<String>,
        addr: String,
        port: u16,
        name: String,
    },
    /// Point `Settings::default_host` at this record, or clear it when it already names it.
    /// `id` is `None` on a record old enough to predate the minted ids; the handler says so.
    MakeDefault {
        id: Option<String>,
        name: String,
    },
    Wake {
        mac: Vec<String>,
        addr: String,
    },
    /// Put this card's `punktfunk://` URL on the clipboard.
    CopyLink(String),
    /// A one-line message for the window's toast overlay — a card that has something to say
    /// and nothing to do (a host action the host has already told us it cannot run).
    Toast(String),
    /// Write a desktop entry that launches this card's URL.
    CreateShortcut {
        label: String,
        url: String,
    },
    /// Add or remove a pinned host+preset card (design §5.2a). Presentation only — it never
    /// changes the host's default preset, and unpinning never touches the preset itself.
    TogglePin {
        fp_hex: String,
        addr: String,
        port: u16,
        preset_id: String,
        pin: bool,
    },
}

impl HostCard {
    fn request(&self) -> ConnectRequest {
        match &self.kind {
            CardKind::Saved {
                host: k, pinned, ..
            } => ConnectRequest {
                // A pinned card IS its preset: clicking it connects with that one, without
                // touching the host's default.
                preset: pinned.as_ref().map(|(id, _)| id.clone()),
                ..saved_request(k)
            },
            CardKind::Discovered(a) => ConnectRequest {
                name: a.name.clone(),
                addr: a.addr.clone(),
                port: a.port,
                fp_hex: (!a.fp_hex.is_empty()).then(|| a.fp_hex.clone()),
                // TOFU only when the host explicitly opts in with pair=optional.
                pair_optional: a.pair == "optional",
                launch: None,
                mac: a.mac.clone(),
                preset: None,
            },
        }
    }
}

impl relm4::factory::FactoryComponent for HostCard {
    type Init = HostCard;
    type Input = ();
    type Output = CardOutput;
    type CommandOutput = ();
    type ParentWidget = gtk::FlowBox;
    type Root = gtk::Overlay;
    type Widgets = ();
    type Index = relm4::factory::DynamicIndex;

    fn init_model(
        init: Self::Init,
        _index: &Self::Index,
        _sender: relm4::FactorySender<Self>,
    ) -> Self {
        init
    }

    fn init_root(&self) -> Self::Root {
        gtk::Overlay::new()
    }

    fn init_widgets(
        &mut self,
        _index: &Self::Index,
        overlay: Self::Root,
        returned: &gtk::FlowBoxChild,
        sender: relm4::FactorySender<Self>,
    ) -> Self::Widgets {
        let req = self.request();

        // The shared scaffold: avatar (spinner while connecting) / name / addr / status.
        let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
        if self.connecting {
            let spinner = adw::Spinner::new();
            spinner.set_size_request(48, 48);
            spinner.set_halign(gtk::Align::Center);
            content.append(&spinner);
        } else {
            // The card's one big circle carries the host's OS mark, not its initial: which
            // machine this is answers a more useful question than which letter it starts with,
            // and the name is already spelled out directly underneath. A host that advertises
            // no OS chain — an older one — keeps the initial, so nothing regresses to a blank.
            let avatar = adw::Avatar::new(48, Some(&req.name), true);
            let os_chain = match &self.kind {
                CardKind::Saved { host: k, .. } => k.os.as_str(),
                CardKind::Discovered(a) => a.os.as_str(),
            };
            if let Some(icon) = os_icon_name(os_chain) {
                // Adwaita's Avatar prefers initials whenever it is allowed to; the icon only
                // shows once they are turned off. The generated background colour stays.
                avatar.set_show_initials(false);
                avatar.set_icon_name(Some(&icon));
                avatar.set_tooltip_text(Some(os_chain));
            }
            avatar.set_halign(gtk::Align::Center);
            content.append(&avatar);
        }
        let name_label = gtk::Label::new(Some(&req.name));
        name_label.add_css_class("heading");
        name_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        content.append(&name_label);
        let addr_label = gtk::Label::new(Some(&format!("{}:{}", req.addr, req.port)));
        addr_label.add_css_class("caption");
        addr_label.add_css_class("dim-label");
        addr_label.add_css_class("numeric");
        addr_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        content.append(&addr_label);

        let status = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        status.set_halign(gtk::Align::Center);
        status.set_margin_top(4);
        // No OS mark here any more: it moved up into the avatar, where it is the card's
        // leading visual rather than the smallest glyph in the status row.
        let pill = |text: &str, class: &str| {
            let l = gtk::Label::new(Some(text));
            l.add_css_class("pf-pill");
            l.add_css_class(class);
            l
        };
        match &self.kind {
            CardKind::Saved {
                host: k,
                online,
                presets,
                pinned,
                ..
            } => {
                // Presence pip + spelled-out state, then the trust pill.
                let pip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
                pip.add_css_class("pf-pip");
                if *online {
                    pip.add_css_class("pf-online");
                }
                pip.set_valign(gtk::Align::Center);
                status.append(&pip);
                let presence = gtk::Label::new(Some(if *online { "Online" } else { "Offline" }));
                presence.add_css_class("caption");
                presence.add_css_class("dim-label");
                status.append(&presence);
                status.append(&if k.paired {
                    pill("Paired", "pf-green")
                } else {
                    pill("Trusted", "pf-accent")
                });
                // The chip says what a plain click on THIS card will do: its own preset on
                // a pinned card, the host's binding on the primary one. Both resolve through
                // the catalog so the chip can carry the preset's colour; a binding whose
                // preset was deleted shows nothing and resolves as the defaults, which is
                // exactly what will happen on connect (design §6).
                let chip = pinned
                    .as_ref()
                    .map(|(id, _)| id.as_str())
                    .or(k.preset_id.as_deref())
                    .and_then(|id| presets.iter().find(|p| p.id == id));
                if let Some(p) = chip {
                    status.append(&preset_pill(p));
                }
            }
            CardKind::Discovered(_) => {
                status.append(&if req.pair_optional {
                    pill("Open", "pf-neutral")
                } else {
                    pill("PIN", "pf-accent")
                });
            }
        }
        content.append(&status);

        overlay.set_child(Some(&content));
        overlay.add_css_class("card");
        overlay.add_css_class("pf-host-card");
        if self.connecting {
            returned.set_sensitive(false);
        }

        match &self.kind {
            CardKind::Saved {
                host: k,
                online,
                recent,
                presets,
                pinned,
            } => {
                if *recent {
                    overlay.add_css_class("pf-recent");
                }
                // Overflow menu (top-right; also on right-click).
                let actions = gio::SimpleActionGroup::new();
                let add = |name: &str, out: Box<dyn Fn() -> CardOutput>| {
                    let a = gio::SimpleAction::new(name, None);
                    let sender = sender.clone();
                    a.connect_activate(move |_, _| {
                        let _ = sender.output(out());
                    });
                    actions.add_action(&a);
                };
                {
                    let req = req.clone();
                    add(
                        "connect",
                        Box::new(move || CardOutput::Connect(req.clone())),
                    );
                }
                {
                    let req = req.clone();
                    add("pair", Box::new(move || CardOutput::Pair(req.clone())));
                }
                {
                    let req = req.clone();
                    add(
                        "speed",
                        Box::new(move || CardOutput::SpeedTest(req.clone())),
                    );
                }
                {
                    let req = req.clone();
                    add(
                        "library",
                        Box::new(move || CardOutput::Library(req.clone())),
                    );
                }
                {
                    let req = req.clone();
                    add(
                        "send-logs",
                        Box::new(move || CardOutput::SendLogs(req.clone())),
                    );
                }
                {
                    let (id, addr, port, name) =
                        (k.id.clone(), k.addr.clone(), k.port, k.name.clone());
                    add(
                        "edit",
                        Box::new(move || CardOutput::Edit {
                            id: id.clone(),
                            addr: addr.clone(),
                            port,
                            name: name.clone(),
                        }),
                    );
                }
                {
                    let (id, addr, port, name) =
                        (k.id.clone(), k.addr.clone(), k.port, k.name.clone());
                    add(
                        "forget",
                        Box::new(move || CardOutput::Forget {
                            id: id.clone(),
                            addr: addr.clone(),
                            port,
                            name: name.clone(),
                        }),
                    );
                }
                {
                    let (id, name) = (k.id.clone(), k.name.clone());
                    add(
                        "make-default",
                        Box::new(move || CardOutput::MakeDefault {
                            id: id.clone(),
                            name: name.clone(),
                        }),
                    );
                }
                {
                    let (mac, addr) = (k.mac.clone(), k.addr.clone());
                    add(
                        "wake",
                        Box::new(move || CardOutput::Wake {
                            mac: mac.clone(),
                            addr: addr.clone(),
                        }),
                    );
                }
                // The host's own actions, one registered action per offered row. Read from
                // the shared cache the hosts page keeps warm, so the menu's rows and these
                // handlers are built from the SAME answer — a menu whose rows outlived their
                // handlers would run the wrong verb, and two of these verbs are irreversible.
                let host_actions = pf_client_core::host_actions::cached(&k.fp_hex);
                for (i, a) in host_actions.iter().enumerate() {
                    let (req, id, label, danger) =
                        (req.clone(), a.id.clone(), a.label().to_string(), a.danger);
                    let available = a.available;
                    let reason = a.unavailable_reason.clone().unwrap_or_default();
                    add(
                        &format!("action{i}"),
                        Box::new(move || {
                            if available {
                                CardOutput::HostAction {
                                    req: req.clone(),
                                    action_id: id.clone(),
                                    label: label.clone(),
                                    danger,
                                }
                            } else {
                                // The host already said it cannot do this right now; say why
                                // rather than send a request we know it will refuse.
                                CardOutput::Toast(if reason.is_empty() {
                                    format!("{label} isn't available right now")
                                } else {
                                    reason.clone()
                                })
                            }
                        }),
                    );
                }
                // "Copy link" / "Create shortcut…": the self-emitted URL for this card, which
                // is what an external tool (a Playnite entry, a Stream Deck macro) is
                // configured with. It carries the stable id AND host+fp, so it still resolves
                // after a re-address or a reinstall (design/client-deep-links.md §2/§5).
                {
                    let (host, preset) = (k.clone(), pinned.clone());
                    let a = gio::SimpleAction::new("copy-link", None);
                    let sender = sender.clone();
                    a.connect_activate(move |_, _| {
                        let url = pf_client_core::deeplink::DeepLink::for_host(
                            &host,
                            None,
                            preset.as_ref().map(|(id, _)| id.as_str()),
                        )
                        .to_url();
                        let _ = sender.output(CardOutput::CopyLink(url));
                    });
                    actions.add_action(&a);
                }
                {
                    let (host, preset) = (k.clone(), pinned.clone());
                    let a = gio::SimpleAction::new("shortcut", None);
                    let sender = sender.clone();
                    a.connect_activate(move |_, _| {
                        let url = pf_client_core::deeplink::DeepLink::for_host(
                            &host,
                            None,
                            preset.as_ref().map(|(id, _)| id.as_str()),
                        )
                        .to_url();
                        let label = match &preset {
                            Some((_, name)) => format!("{} \u{00b7} {name}", host.name),
                            None => host.name.clone(),
                        };
                        let _ = sender.output(CardOutput::CreateShortcut { label, url });
                    });
                    actions.add_action(&a);
                }
                // A one-off connect ("Connect with") never rebinds the host — the whole
                // predictability rule is that it can't change what the card does next time.
                // Rebinding, and pinning, live in the edit sheet (design §5.2).
                {
                    let preset_action =
                        |name: &str, out: Box<dyn Fn(Option<String>) -> CardOutput>| {
                            let a = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
                            let sender = sender.clone();
                            a.connect_activate(move |_, param| {
                                // The empty string is "Default settings" — a real choice, not
                                // an absent one, so it has to survive as a value.
                                let id = param.and_then(|p| p.str()).unwrap_or("").to_string();
                                let _ = sender.output(out(Some(id).filter(|s| !s.is_empty())));
                            });
                            actions.add_action(&a);
                        };
                    let req_for_connect = req.clone();
                    preset_action(
                        "connect-with",
                        Box::new(move |id| {
                            let mut req = req_for_connect.clone();
                            // `Some("")` — not `None` — so a bound host really does connect
                            // with the defaults when the user asks for them.
                            req.preset = Some(id.unwrap_or_default());
                            CardOutput::Connect(req)
                        }),
                    );
                    // The same action pins from a primary card and unpins from a pinned one —
                    // which of the two this card is decides the direction.
                    let (fp, addr, port) = (k.fp_hex.clone(), k.addr.clone(), k.port);
                    let pinning = pinned.is_none();
                    preset_action(
                        "toggle-pin",
                        Box::new(move |id| CardOutput::TogglePin {
                            fp_hex: fp.clone(),
                            addr: addr.clone(),
                            port,
                            preset_id: id.unwrap_or_default(),
                            pin: pinning,
                        }),
                    );
                }
                overlay.insert_action_group("card", Some(&actions));

                // Keep this menu short: anything that CONFIGURES the host — the default preset,
                // the pinned cards — belongs in the edit sheet, not here. What remains is
                // grouped into sections: start something, view something, take a link, manage
                // the host.
                let menu = gio::Menu::new();
                if let Some((pin_id, pin_name)) = pinned {
                    // A pinned card is a shortcut, not a second host: it starts a stream, hands
                    // out its link, and removes itself. Pair/edit/forget belong to the host and
                    // offering them here would blur what the card is.
                    let launch = gio::Menu::new();
                    launch.append(Some("Connect"), Some("card.connect"));
                    // Browse library starts this card with its own preset, not the binding's,
                    // so it belongs to a shortcut as much as Connect does. Paired only: the
                    // fetch authenticates as this device, so a merely trusted host refuses it.
                    if k.paired {
                        launch.append(Some("Browse library\u{2026}"), Some("card.library"));
                    }
                    menu.append_section(None, &launch);

                    let links = gio::Menu::new();
                    links.append(Some("Copy link"), Some("card.copy-link"));
                    links.append(Some("Create shortcut\u{2026}"), Some("card.shortcut"));
                    menu.append_section(None, &links);

                    let manage = gio::Menu::new();
                    let unpin = gio::MenuItem::new(
                        Some(&format!("Unpin \u{201c}{pin_name}\u{201d}")),
                        None,
                    );
                    unpin.set_action_and_target_value(
                        Some("card.toggle-pin"),
                        Some(&pin_id.as_str().to_variant()),
                    );
                    manage.append_item(&unpin);
                    menu.append_section(None, &manage);
                } else {
                    // Starting a stream: a plain click already connects with the host's own
                    // preset, so the menu only needs the one-offs — and only when there are
                    // presets to pick between.
                    if !presets.is_empty() {
                        let with = gio::Menu::new();
                        for (label, id) in std::iter::once(("Default settings", ""))
                            .chain(presets.iter().map(|p| (p.name.as_str(), p.id.as_str())))
                        {
                            let item = gio::MenuItem::new(Some(label), None);
                            item.set_action_and_target_value(
                                Some("card.connect-with"),
                                Some(&id.to_variant()),
                            );
                            with.append_item(&item);
                        }
                        let launch = gio::Menu::new();
                        launch.append_submenu(Some("Connect with"), &with);
                        menu.append_section(None, &launch);
                    }

                    let look = gio::Menu::new();
                    // Browse the host's game library — offered on any paired host, but only a
                    // paired one: a saved card can be merely "Trusted" (the pill above), and
                    // the fetch authenticates as this device, so there it would only be refused.
                    if k.paired {
                        look.append(Some("Browse library\u{2026}"), Some("card.library"));
                    }
                    look.append(Some("Test network speed\u{2026}"), Some("card.speed"));
                    // The same row the console's host menu carries, on the same gate: the
                    // upload authenticates with the paired identity, and an offline host
                    // could only ever toast an error. The bundle lands on the host's web
                    // console (Logs page) beside the host's own log.
                    if k.paired && *online {
                        look.append(Some("Send logs to host"), Some("card.send-logs"));
                    }
                    // An explicit wake only when offline and a MAC is known.
                    if !online && !k.mac.is_empty() {
                        look.append(Some("Wake host"), Some("card.wake"));
                    }
                    // The host's power rows (sleep, restart, shut down), already filtered to
                    // this device's grant: nothing is gated here, and an empty list means the
                    // host never answered or granted nothing. Indexed to match the handlers
                    // registered above, so a later host can add an action with no client release.
                    for (i, a) in host_actions.iter().enumerate() {
                        let label = if a.available {
                            a.label().to_string()
                        } else {
                            format!("{} (unavailable)", a.label())
                        };
                        look.append(Some(&label), Some(&format!("card.action{i}")));
                    }
                    menu.append_section(None, &look);

                    let links = gio::Menu::new();
                    links.append(Some("Copy link"), Some("card.copy-link"));
                    links.append(Some("Create shortcut\u{2026}"), Some("card.shortcut"));
                    menu.append_section(None, &links);

                    let manage = gio::Menu::new();
                    // Which host the app opens on. Needs a pairing to point at — the start
                    // screen skips an unpaired host, so writing one would set a pointer that
                    // never resolves. Unchecked is not "not the default": a lone paired host
                    // is the default with nothing written.
                    if k.paired {
                        // One settings read per card build. Cards rebuild on store changes,
                        // not per frame, so this is a file read per host per change.
                        let named = k.id.is_some()
                            && Settings::load().default_host.as_deref() == k.id.as_deref();
                        manage.append(
                            Some(if named {
                                "Default host \u{2713}"
                            } else {
                                "Make default host"
                            }),
                            Some("card.make-default"),
                        );
                    }
                    manage.append(Some("Edit\u{2026}"), Some("card.edit"));
                    manage.append(Some("Pair with PIN\u{2026}"), Some("card.pair"));
                    manage.append(Some("Forget"), Some("card.forget"));
                    menu.append_section(None, &manage);
                }
                let menu_btn = gtk::MenuButton::builder()
                    .child(&crate::lucide::row_icon("ellipsis"))
                    .menu_model(&menu)
                    .halign(gtk::Align::End)
                    .valign(gtk::Align::Start)
                    .build();
                menu_btn.add_css_class("flat");
                overlay.add_overlay(&menu_btn);
                let right_click = gtk::GestureClick::builder().button(3).build();
                {
                    let menu_btn = menu_btn.clone();
                    right_click.connect_pressed(move |_, _, _, _| menu_btn.popup());
                }
                overlay.add_controller(right_click);

                // Auto-wake: the probe did not reach it + a known MAC routes to WakeConnect,
                // which dials first (a routed/Tailscale host is mDNS-blind, not asleep) and only
                // falls into the wake-and-wait when the dial fails.
                let wake_first = !online && !req.mac.is_empty();
                let sender = sender.clone();
                returned.connect_activate(move |_| {
                    let _ = sender.output(if wake_first {
                        CardOutput::WakeConnect(req.clone())
                    } else {
                        CardOutput::Connect(req.clone())
                    });
                });
            }
            CardKind::Discovered(_) => {
                overlay.add_css_class("pf-discovered");
                // Tap-to-connect only (parity with Android's discovered cards).
                let sender = sender.clone();
                returned.connect_activate(move |_| {
                    let _ = sender.output(CardOutput::Connect(req.clone()));
                });
            }
        }
    }
}

// --- The page component ---------------------------------------------------------------------

/// How long each saved-host reachability probe waits, and how often the sweep runs. The pip reads
/// this sweep and nothing else, so a host reached only over a routed network (Tailscale/VPN) —
/// which never appears on mDNS — shows Online, and a sleeping one shows Offline within a cycle.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(2500);
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(12);

/// The OS-icon tokens this shell ships symbolic art for (`data/icons/.../pf-os-<t>-symbolic.svg`,
/// embedded via gresource): the families a chain can land on, plus the distro leaves that earn
/// their own mark because "a Bazzite box" and "a Fedora box" are different machines to the person
/// reading the card. Chains walk most-specific-first, so a distro without a mark of its own still
/// lands on its family's and finally on plain Tux.
const OS_ICON_TOKENS: &[&str] = &[
    "windows", "apple", "linux", "steam", "ubuntu", "fedora", "arch", "debian", "nixos",
    "opensuse", "bazzite", "cachyos", "nobara", "omarchy",
];

/// The symbolic icon name for an advertised chain, or `None` when the host doesn't advertise
/// one / nothing in the chain is recognized-and-drawable. Chains walk most-specific-first.
fn os_icon_name(chain: &str) -> Option<String> {
    let token = crate::os::os_icon_tokens(chain)
        .into_iter()
        .find(|t| OS_ICON_TOKENS.contains(&t.as_str()))?;
    Some(format!("pf-os-{token}-symbolic"))
}

/// A preset chip in that preset's colour. `accent` is the field the catalog schema reserved
/// for this — without it every preset is the same grey, and telling them apart across a grid
/// at a glance is the whole reason the chip exists. No colour set keeps the neutral pill, so
/// the palette stays opt-in.
fn preset_pill(p: &Preset) -> gtk::Widget {
    let label = gtk::Label::new(Some(&p.name));
    label.add_css_class("pf-pill");
    let Some(hex) = p.accent.as_deref().filter(|h| is_hex_colour(h)) else {
        label.add_css_class("pf-neutral");
        return label.upcast();
    };
    label.add_css_class(&tint_class(hex));
    label.upcast()
}

/// The CSS class that tints a pill with `hex`, registering its rule on the display the first
/// time that colour is seen.
///
/// Per-widget providers are gone since GTK 4.10, and the colour is user data rather than one of
/// a fixed set, so the rule is generated once per distinct colour and added display-wide. A
/// handful of presets means a handful of tiny rules, and re-rendering the grid (which happens
/// on every state change) reuses them instead of allocating more.
fn tint_class(hex: &str) -> String {
    thread_local! {
        static REGISTERED: RefCell<HashMap<String, ()>> = RefCell::new(HashMap::new());
    }
    let class = format!("pf-tint-{}", &hex[1..]);
    REGISTERED.with_borrow_mut(|seen| {
        if seen.contains_key(&class) {
            return;
        }
        if let Some(display) = gtk::gdk::Display::default() {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(&format!(
                ".pf-pill.{class} {{ color: {hex}; background: alpha({hex}, 0.18); }}"
            ));
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            seen.insert(class.clone(), ());
        }
    });
    class
}

/// `#RRGGBB` only — the value is interpolated into CSS, so anything else is refused rather
/// than injected.
fn is_hex_colour(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

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
    type Init = ();
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
        _init: Self::Init,
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
            // Scopes the concentric hover-highlight radius (see app.rs CSS).
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
            crate::ui_flow::bridge_child_activation(flow);
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
        let add_host_btn = crate::lucide::button("plus");
        add_host_btn.set_tooltip_text(Some("Add host"));
        add_host_btn.set_action_name(Some("win.add-host"));
        header.pack_start(&add_host_btn);
        let rescan_btn = crate::lucide::button("refresh-cw");
        rescan_btn.set_tooltip_text(Some("Scan the network for hosts again"));
        {
            let sender = sender.clone();
            rescan_btn.connect_clicked(move |_| sender.input(HostsMsg::Rescan));
        }
        header.pack_start(&rescan_btn);
        // The couch UI's front door, beside the page's other actions (same placement the
        // WinUI shell gives it). It was previously reachable only as `--browse` on the
        // command line, which is no way to find a mode.
        let console_btn = crate::lucide::button("gamepad-2");
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
            .child(&crate::lucide::row_icon("menu"))
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
            let sender = sender.clone();
            glib::spawn_future_local(async move {
                loop {
                    let hosts: Vec<KnownHost> = KnownHosts::load()
                        .hosts
                        .into_iter()
                        .filter(|h| !h.addr.is_empty())
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
        };
        model.rebuild();

        ComponentParts { model, widgets: () }
    }

    fn update(&mut self, msg: HostsMsg, sender: ComponentSender<Self>) {
        match msg {
            HostsMsg::Advert(h) => {
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
                    let mut settings = trust::Settings::load();
                    let on = settings.default_host.as_deref() != Some(id.as_str());
                    settings.default_host = on.then_some(id);
                    settings.save();
                    let opens = start::StartIn::parse(&settings.start_in) != start::StartIn::Hosts;
                    let _ = sender.output(HostsOutput::Toast(match (on, opens) {
                        (true, true) => format!("{name} opens on launch"),
                        (true, false) => format!("{name} is the default host"),
                        (false, _) => format!("{name} is no longer the default host"),
                    }));
                    sender.input(HostsMsg::Refresh);
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
                    let mut known = KnownHosts::load();
                    if let Some(h) = known.hosts.iter_mut().find(|h| {
                        (!fp_hex.is_empty() && h.fp_hex == fp_hex)
                            || (h.addr == addr && h.port == port)
                    }) {
                        h.pinned_presets.retain(|p| p != &preset_id);
                        if pin {
                            h.pinned_presets.push(preset_id);
                        }
                        if let Err(e) = known.save() {
                            tracing::warn!(error = %format!("{e:#}"), "saving the pinned cards");
                        }
                    }
                    self.rebuild();
                }
            },
        }
    }
}

impl HostsPage {
    /// Re-populate both factories from disk + the advert map. Cheap (a handful of
    /// widgets) and keeps every derived view — online pips, dedup, most-recent accent,
    /// spinner — in one straight-line pass.
    fn rebuild(&mut self) {
        let known = KnownHosts::load();
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
            pf_client_core::presets::PresetsFile::load()
                .presets
                .into_iter()
                .map(|p| Preset {
                    id: p.id,
                    name: p.name,
                    accent: p.accent,
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
                // Learn what this host's live advert teaches: its wake MAC(s), its OS chain (so
                // the icon survives it going offline), its management port, and an address the
                // probe sweep asks — the card moves there only once its pin answers.
                let advert = self.adverts.values().find(|a| matches(k, a));
                if let Some(a) = advert {
                    crate::trust::learn_from_advert(
                        &k.fp_hex,
                        &k.addr,
                        k.port,
                        &a.addr,
                        &a.mac,
                        &a.os,
                        a.mgmt_port,
                    );
                }
                // Keep this host's advertised actions warm, so the card's menu is built from a
                // settled answer rather than one that arrives while the menu is open. Gated on
                // the TTL inside, so an ordinary refresh costs nothing. Same three rungs for
                // the port as everything else here: live advert, then the stored one, then the
                // default.
                if k.paired && online {
                    let mgmt = advert
                        .and_then(|a| a.mgmt_port)
                        .unwrap_or_else(|| k.effective_mgmt_port());
                    pf_client_core::host_actions::refresh(&k.addr, mgmt, &k.fp_hex);
                }
                saved.push_back(HostCard {
                    // The key `ConnectRequest::card_key` mints. A bare `fp_hex` is empty for
                    // every unpaired record.
                    connecting: self.connecting.as_deref() == Some(k.card_key().as_str()),
                    kind: CardKind::Saved {
                        host: k.clone(),
                        online,
                        presets: presets.clone(),
                        recent: most_recent.as_deref() == Some(k.fp_hex.as_str()),
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
        crate::trust::KnownHosts::load()
            .hosts
            .iter()
            .find(|h| matches_req(&h.fp_hex, &h.addr, h.port))
            .and_then(|h| h.mgmt_port)
    }

    /// Write the shortcut, or — inside the flatpak sandbox, which cannot reach
    /// `~/.local/share/applications` — hand the user the URL to place themselves. The
    /// DynamicLauncher portal is the intended upgrade for that case (design §5); until then
    /// the fallback is the one the design already sanctions, not a dead end.
    fn shortcut_result(&self, sender: &ComponentSender<Self>, label: &str, url: &str) {
        if crate::shortcuts::sandboxed() {
            let dialog = adw::AlertDialog::new(
                Some("Create Shortcut"),
                Some(
                    "Punktfunk is sandboxed here, so it can't add the shortcut itself. Copy \
                     this link and make a launcher for it \u{2014} it opens the same stream.",
                ),
            );
            let entry = gtk::Entry::builder().text(url).editable(false).build();
            dialog.set_extra_child(Some(&entry));
            dialog.add_responses(&[("close", "Close"), ("copy", "Copy link")]);
            dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("copy"));
            dialog.set_close_response("close");
            {
                let url = url.to_string();
                dialog.connect_response(Some("copy"), move |_, _| {
                    if let Some(display) = gtk::gdk::Display::default() {
                        display.clipboard().set_text(&url);
                    }
                });
            }
            dialog.present(Some(&self.widgets.stack));
            return;
        }
        let msg = match crate::shortcuts::write_desktop_entry(label, url) {
            Ok(_) => format!("Shortcut for \u{201c}{label}\u{201d} added to your applications"),
            Err(e) => {
                tracing::warn!(error = %e, "writing the shortcut");
                format!("Couldn't create the shortcut \u{2014} {e}")
            }
        };
        let _ = sender.output(HostsOutput::Toast(msg));
    }

    /// The host edit sheet — the per-host settings that are properties of the HOST, not of
    /// the stream: its name, whether this machine shares its clipboard with it, and which
    /// settings preset it defaults to.
    ///
    /// Linux had only "Rename" until now; the clipboard toggle in particular existed in the
    /// store and on the Apple and Windows clients but had no Linux surface at all, so a Linux
    /// user could not turn on a feature they were already paying the storage for.
    fn edit_host_dialog(
        &self,
        sender: &ComponentSender<Self>,
        id: Option<&str>,
        addr: &str,
        port: u16,
        current: &str,
    ) {
        let known = KnownHosts::load();
        let stored = known
            .index_of_card(id, addr, port)
            .and_then(|i| known.hosts.get(i))
            .cloned();
        let name_row = adw::EntryRow::builder().title("Name").build();
        name_row.set_text(current);
        let clipboard_row = adw::SwitchRow::builder()
            .title("Share clipboard")
            .subtitle(
                "Copy and paste between this machine and that host. Per host \u{2014} handing a \
                 host your clipboard is a decision about that host.",
            )
            .build();
        clipboard_row.set_active(stored.as_ref().is_some_and(|h| h.clipboard_sync));

        // Preset picker: "Default settings" plus the catalog, seeded to the current binding.
        let catalog = pf_client_core::presets::PresetsFile::load();
        let mut labels = vec!["Default settings".to_string()];
        let mut ids: Vec<String> = vec![String::new()];
        for p in &catalog.presets {
            labels.push(p.name.clone());
            ids.push(p.id.clone());
        }
        let bound = stored.as_ref().and_then(|h| h.preset_id.clone());
        // A binding whose preset is gone reads as Default settings and is cleaned up on save
        // — the same "dangling resolves as none" rule the connect path follows.
        let selected = bound
            .as_ref()
            .and_then(|id| ids.iter().position(|i| i == id))
            .unwrap_or(0);
        let preset_row = adw::ComboRow::builder()
            .title("Preset")
            .subtitle("The settings a plain click uses for this host")
            .model(&gtk::StringList::new(
                &labels.iter().map(String::as_str).collect::<Vec<_>>(),
            ))
            .build();
        preset_row.set_selected(selected as u32);

        // Pinned cards: which presets get their own one-click card for this host. They used
        // to be a third submenu on the card, which is what tipped that menu over — and this is
        // where they belong anyway, next to the default they sit beside (design §5.2a).
        let pin_rows: Vec<(String, adw::SwitchRow)> = catalog
            .presets
            .iter()
            .map(|p| {
                let row = adw::SwitchRow::builder()
                    .title(&p.name)
                    .subtitle("Show as its own card")
                    .build();
                row.set_active(
                    stored
                        .as_ref()
                        .is_some_and(|h| h.pinned_presets.iter().any(|id| id == &p.id)),
                );
                (p.id.clone(), row)
            })
            .collect();

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        list.append(&name_row);
        list.append(&preset_row);
        list.append(&clipboard_row);
        for (_, row) in &pin_rows {
            list.append(row);
        }

        let dialog = adw::AlertDialog::new(Some("Edit Host"), None);
        dialog.set_extra_child(Some(&list));
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        {
            let sender = sender.clone();
            let (id, addr, port) = (id.map(str::to_string), addr.to_string(), port);
            dialog.connect_response(Some("save"), move |_, _| {
                let name = name_row.text().trim().to_string();
                let mut known = KnownHosts::load();
                let target = known.index_of_card(id.as_deref(), &addr, port);
                if let Some(h) = target.and_then(|i| known.hosts.get_mut(i)) {
                    if !name.is_empty() {
                        h.name = name;
                    }
                    h.clipboard_sync = clipboard_row.is_active();
                    h.preset_id = ids
                        .get(preset_row.selected() as usize)
                        .filter(|id| !id.is_empty())
                        .cloned();
                    // Rebuilt from the switches rather than toggled, so the card order follows
                    // the catalog and a preset deleted meanwhile simply drops out.
                    h.pinned_presets = pin_rows
                        .iter()
                        .filter(|(_, row)| row.is_active())
                        .map(|(id, _)| id.clone())
                        .collect();
                    if let Err(e) = known.save() {
                        let _ = sender.output(HostsOutput::Toast(format!("Couldn't save — {e:#}")));
                    }
                }
                sender.input(HostsMsg::Refresh);
            });
        }
        dialog.present(Some(&self.widgets.stack));
    }

    /// Forget this host (drops the pinned fingerprint — a later connect re-pairs).
    fn forget_dialog(
        &self,
        sender: &ComponentSender<Self>,
        id: Option<&str>,
        addr: &str,
        port: u16,
        name: &str,
    ) {
        let dialog = adw::AlertDialog::new(
            Some("Remove saved host?"),
            Some(&format!(
                "Forget “{name}”? You'll need to pair (or trust) it again to reconnect."
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        {
            let sender = sender.clone();
            let (id, addr, port) = (id.map(str::to_string), addr.to_string(), port);
            dialog.connect_response(Some("remove"), move |_, _| {
                let mut known = KnownHosts::load();
                if let Some(i) = known.index_of_card(id.as_deref(), &addr, port) {
                    if let Err(e) = pf_client_core::orchestrate::forget_host(&mut known, i) {
                        let _ = sender.output(HostsOutput::Toast(format!("Couldn't save — {e:#}")));
                    }
                }
                sender.input(HostsMsg::Refresh);
            });
        }
        dialog.present(Some(&self.widgets.stack));
    }

    /// "+": name (optional) / address / port. Submit runs the normal trust gate.
    fn add_host_dialog(&self, sender: &ComponentSender<Self>) {
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        let name_row = adw::EntryRow::builder().title("Name (optional)").build();
        let addr_row = adw::EntryRow::builder().title("Address").build();
        let port_row = adw::EntryRow::builder().title("Port").text("9777").build();
        list.append(&name_row);
        list.append(&addr_row);
        list.append(&port_row);
        list.set_size_request(320, -1);

        let dialog = adw::AlertDialog::new(Some("Add Host"), None);
        dialog.set_extra_child(Some(&list));
        dialog.add_responses(&[("cancel", "Cancel"), ("connect", "Connect")]);
        dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("connect"));
        dialog.set_close_response("cancel");
        dialog.set_response_enabled("connect", false);
        {
            let dialog = dialog.clone();
            addr_row.connect_changed(move |row| {
                dialog.set_response_enabled("connect", !row.text().trim().is_empty());
            });
        }
        {
            let sender = sender.clone();
            let (name_row, addr_row, port_row) =
                (name_row.clone(), addr_row.clone(), port_row.clone());
            dialog.connect_response(Some("connect"), move |_, _| {
                let text = addr_row.text().trim().to_string();
                if text.is_empty() {
                    return;
                }
                // A pasted `host:port` wins over the port field; else the field. The shared
                // parser, so a pasted `::1` stays one address instead of host `:` port `1`.
                let field = port_row.text().trim().parse::<u16>().unwrap_or(9777);
                let (addr, port) = match pf_client_core::deeplink::split_host_port(&text) {
                    Some((a, spelled)) => (a, spelled.unwrap_or(field)),
                    None => (text.clone(), field),
                };
                let name = name_row.text().trim().to_string();
                let _ = sender.output(HostsOutput::Connect(ConnectRequest {
                    name: if name.is_empty() { addr.clone() } else { name },
                    addr,
                    port,
                    fp_hex: None,
                    // Manual entry carries no advertised policy — never TOFU-eligible.
                    pair_optional: false,
                    launch: None,
                    mac: Vec::new(),
                    preset: None,
                }));
            });
        }
        dialog.present(Some(&self.widgets.stack));
    }
}
