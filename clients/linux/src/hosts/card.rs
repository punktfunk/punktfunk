//! One host card: a saved or discovered host's tile, its menu and its preset chip.

use super::*;

/// One card's full render input — rebuilt (clear + repopulate) on every state change,
/// exactly like the pre-relm4 full-grid rebuild (a handful of widgets; simpler than row
/// surgery and keeps every derived view consistent).
#[derive(Debug)]
pub struct HostCard {
    pub(super) kind: CardKind,
    pub(super) connecting: bool,
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
pub(super) enum CardKind {
    Saved {
        host: KnownHost,
        online: bool,
        recent: bool,
        /// This record is the one `Settings::default_host` names.
        is_default: bool,
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
                } else if k.fp_hex.is_empty() {
                    // Added by address and never dialed: nothing is pinned yet.
                    pill("Not paired", "pf-neutral")
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
                is_default,
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
                        manage.append(
                            Some(if *is_default {
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
                    .child(&crate::widgets::lucide::row_icon("ellipsis"))
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
