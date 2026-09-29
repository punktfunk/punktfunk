//! One host card: a tile, two lines and a menu of at most five rows (design §2.2–2.3). Built
//! from a [`CardModel`]; the page rebuilds cards only when the models change, so an open menu
//! survives an unchanged probe sweep. Everything else about a host is on its page.

use super::model::{CardKind, CardModel, Preset, Status};
use super::{Act, HostRef, HostsMsg};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::RefCell;
use std::collections::HashSet;

/// The card for `m`, wrapped in the FlowBox child the page appends. `presets` feeds
/// Connect with ▸.
pub fn build(
    m: &CardModel,
    presets: &[Preset],
    sender: &relm4::Sender<HostsMsg>,
) -> gtk::FlowBoxChild {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("card");
    row.add_css_class("pf-host-card");
    row.set_size_request(280, -1);
    row.append(&tile(m));

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_valign(gtk::Align::Center);
    text.set_hexpand(true);
    let name = gtk::Label::builder()
        .label(&m.name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["heading"])
        .build();
    text.append(&name);
    text.append(&status_line(m));
    row.append(&text);
    // The address is identity for a machine, not what a click does: it lives on the host page
    // and here, one hover away (design P4).
    row.set_tooltip_text(Some(&m.address));

    let host = saved_ref(m);
    if let Some(host) = &host {
        let info = crate::widgets::lucide::button("info");
        info.set_tooltip_text(Some("Host details"));
        info.update_property(&[gtk::accessible::Property::Label(&format!(
            "{} details",
            m.name
        ))]);
        info.set_valign(gtk::Align::Center);
        info.add_css_class("flat");
        info.add_css_class("circular");
        let (sender, host) = (sender.clone(), host.clone());
        info.connect_clicked(move |_| sender.emit(HostsMsg::Act(Act::Details(host.clone()))));
        row.append(&info);
    } else {
        row.add_css_class("pf-discovered");
    }

    let child = gtk::FlowBoxChild::new();
    child.set_child(Some(&row));
    child.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, {}",
        m.name,
        line_text(m)
    ))]);
    if m.status == Status::Connecting {
        child.set_sensitive(false);
    }
    {
        let (sender, req, wake) = (sender.clone(), m.request.clone(), m.wake_first());
        child.connect_activate(move |_| {
            sender.emit(HostsMsg::Act(if wake {
                Act::WakeConnect(req.clone())
            } else {
                Act::Connect(req.clone())
            }))
        });
    }
    if let Some(host) = host {
        let (menu, actions) = menu(m, &host, presets, sender);
        row.insert_action_group("card", Some(&actions));
        attach_menu(&child, &row, &menu);
    }
    child
}

/// A saved card's record, `None` for a discovered one.
fn saved_ref(m: &CardModel) -> Option<HostRef> {
    let CardKind::Saved { id, .. } = &m.kind else {
        return None;
    };
    Some(HostRef {
        id: id.clone(),
        addr: m.request.addr.clone(),
        port: m.request.port,
    })
}

/// The OS mark in a circle — which machine this is, not the letter it starts with — dimmed
/// while it cannot answer, a spinner while connecting, a star on the default host's own card.
fn tile(m: &CardModel) -> gtk::Widget {
    if m.status == Status::Connecting {
        let spinner = adw::Spinner::new();
        spinner.set_size_request(48, 48);
        return spinner.upcast();
    }
    let avatar = adw::Avatar::new(48, Some(&m.name), true);
    if let Some(icon) = os_icon_name(&m.os) {
        // Adwaita's Avatar prefers initials whenever it may; the icon shows once they are off.
        avatar.set_show_initials(false);
        avatar.set_icon_name(Some(&icon));
    }
    if matches!(m.kind, CardKind::Saved { .. }) && !m.status.live() {
        avatar.set_opacity(0.55);
    }
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&avatar));
    overlay.set_valign(gtk::Align::Center);
    if matches!(
        m.kind,
        CardKind::Saved {
            is_default: true,
            pinned: None,
            ..
        }
    ) {
        let star = crate::widgets::lucide::icon("star", 12);
        star.add_css_class("pf-star");
        star.set_halign(gtk::Align::End);
        star.set_valign(gtk::Align::Start);
        star.set_tooltip_text(Some("Default host"));
        overlay.add_overlay(&star);
    }
    overlay.upcast()
}

/// Line two's words, also the card's accessible description.
fn line_text(m: &CardModel) -> String {
    match &m.kind {
        CardKind::Saved { .. } => m.status.sentence(),
        CardKind::Discovered { .. } if m.status == Status::Connecting => m.status.sentence(),
        CardKind::Discovered {
            pair_optional: true,
        } => "Found on this network".into(),
        CardKind::Discovered { .. } => "Pairing required".into(),
    }
}

/// Line two: a presence dot and one sentence, then the preset chip. The chip says what a click
/// connects with; when space runs short the sentence shortens, never the chip.
fn status_line(m: &CardModel) -> gtk::Box {
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    if matches!(m.kind, CardKind::Saved { .. }) {
        let pip = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        pip.add_css_class("pf-pip");
        if m.status.live() {
            pip.add_css_class("pf-online");
        }
        pip.set_valign(gtk::Align::Center);
        line.append(&pip);
    }
    let words = gtk::Label::builder()
        .label(line_text(m))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption"])
        .build();
    if !m.status.live() || matches!(m.kind, CardKind::Discovered { .. }) {
        words.add_css_class("dim-label");
    }
    line.append(&words);
    if let Some(p) = &m.chip {
        line.append(&preset_pill(p));
    }
    line
}

/// At most five rows and one submenu (design §2.3). A pinned card is a shortcut: it browses,
/// hands out its link and unpins itself.
fn menu(
    m: &CardModel,
    host: &HostRef,
    presets: &[Preset],
    sender: &relm4::Sender<HostsMsg>,
) -> (gio::Menu, gio::SimpleActionGroup) {
    let CardKind::Saved { paired, pinned, .. } = &m.kind else {
        unreachable!("only saved cards carry a menu");
    };
    let actions = gio::SimpleActionGroup::new();
    let add = |name: &str, act: Box<dyn Fn() -> Act>| {
        let a = gio::SimpleAction::new(name, None);
        let sender = sender.clone();
        a.connect_activate(move |_, _| sender.emit(HostsMsg::Act(act())));
        actions.add_action(&a);
    };
    let pin_id = pinned.as_ref().map(|(id, _)| id.clone());
    {
        let req = m.request.clone();
        add("library", Box::new(move || Act::Library(req.clone())));
    }
    {
        let (host, preset) = (host.clone(), pin_id.clone());
        add(
            "copy-link",
            Box::new(move || Act::CopyLink {
                host: host.clone(),
                preset: preset.clone(),
            }),
        );
    }
    {
        let host = host.clone();
        add("details", Box::new(move || Act::Details(host.clone())));
    }
    {
        let (mac, addr) = (m.request.mac.clone(), m.request.addr.clone());
        add(
            "wake",
            Box::new(move || Act::Wake {
                mac: mac.clone(),
                addr: addr.clone(),
            }),
        );
    }
    if let Some(preset_id) = pin_id.clone() {
        let host = host.clone();
        add(
            "unpin",
            Box::new(move || Act::Unpin {
                host: host.clone(),
                preset_id: preset_id.clone(),
            }),
        );
    }
    {
        // A one-off: it shapes this connect and never rebinds the host. `""` is Default
        // settings, a real choice on a bound host.
        let a = gio::SimpleAction::new("connect-with", Some(glib::VariantTy::STRING));
        let (sender, req, wake) = (sender.clone(), m.request.clone(), m.wake_first());
        a.connect_activate(move |_, param| {
            let mut req = req.clone();
            req.preset = Some(param.and_then(|p| p.str()).unwrap_or("").to_string());
            sender.emit(HostsMsg::Act(if wake {
                Act::WakeConnect(req)
            } else {
                Act::Connect(req)
            }));
        });
        actions.add_action(&a);
    }

    let menu = gio::Menu::new();
    if pinned.is_some() {
        if *paired {
            menu.append(Some("Browse Library"), Some("card.library"));
        }
        menu.append(Some("Copy Link"), Some("card.copy-link"));
        menu.append(Some("Unpin Card"), Some("card.unpin"));
        return (menu, actions);
    }
    if !presets.is_empty() {
        let with = gio::Menu::new();
        for (label, id) in std::iter::once(("Default settings", ""))
            .chain(presets.iter().map(|p| (p.name.as_str(), p.id.as_str())))
        {
            let item = gio::MenuItem::new(Some(label), None);
            item.set_action_and_target_value(Some("card.connect-with"), Some(&id.to_variant()));
            with.append_item(&item);
        }
        menu.append_submenu(Some("Connect With"), &with);
    }
    // The library fetch authenticates as this device, so only a paired host serves it.
    if *paired {
        menu.append(Some("Browse Library"), Some("card.library"));
    }
    if !m.status.live() && !m.request.mac.is_empty() {
        menu.append(Some("Wake Host"), Some("card.wake"));
    }
    menu.append(Some("Copy Link"), Some("card.copy-link"));
    menu.append(Some("Host Details\u{2026}"), Some("card.details"));
    (menu, actions)
}

/// Right-click, a long press, or the Menu key (Shift+F10) opens the card's menu at the pointer.
fn attach_menu(child: &gtk::FlowBoxChild, anchor: &gtk::Box, menu: &gio::Menu) {
    let popover = gtk::PopoverMenu::from_model(Some(menu));
    popover.set_parent(anchor);
    popover.set_has_arrow(false);
    {
        let popover = popover.clone();
        anchor.connect_destroy(move |_| popover.unparent());
    }
    let at = |popover: &gtk::PopoverMenu, x: f64, y: f64| {
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.popup();
    };
    let right = gtk::GestureClick::builder().button(3).build();
    {
        let popover = popover.clone();
        right.connect_pressed(move |_, _, x, y| at(&popover, x, y));
    }
    anchor.add_controller(right);
    let long = gtk::GestureLongPress::new();
    long.set_touch_only(true);
    {
        let popover = popover.clone();
        long.connect_pressed(move |_, x, y| at(&popover, x, y));
    }
    anchor.add_controller(long);
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(move |_, key, _, state| {
        let menu_key = key == gdk::Key::Menu
            || (key == gdk::Key::F10 && state.contains(gdk::ModifierType::SHIFT_MASK));
        if !menu_key {
            return glib::Propagation::Proceed;
        }
        popover.set_pointing_to(None);
        popover.popup();
        glib::Propagation::Stop
    });
    child.add_controller(keys);
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
/// one / nothing in the chain is recognized-and-drawable.
pub fn os_icon_name(chain: &str) -> Option<String> {
    let token = crate::os::os_icon_tokens(chain)
        .into_iter()
        .find(|t| OS_ICON_TOKENS.contains(&t.as_str()))?;
    Some(format!("pf-os-{token}-symbolic"))
}

/// A preset chip in that preset's colour. `accent` is the field the catalog schema reserved
/// for this — without it every preset is the same grey. No colour keeps the neutral pill, so
/// the palette stays opt-in.
pub fn preset_pill(p: &Preset) -> gtk::Widget {
    let label = gtk::Label::new(Some(&p.name));
    label.add_css_class("pf-pill");
    label.set_valign(gtk::Align::Center);
    let Some(hex) = p.accent.as_deref().filter(|h| is_hex_colour(h)) else {
        label.add_css_class("pf-neutral");
        return label.upcast();
    };
    label.add_css_class(&tint_class(hex));
    label.upcast()
}

/// The CSS class that tints a pill with `hex`, registering its rule on the display the first
/// time that colour is seen. The colour is user data, so the rule is generated once per
/// distinct colour and added display-wide; a redraw reuses it.
fn tint_class(hex: &str) -> String {
    thread_local! {
        static REGISTERED: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    }
    let class = format!("pf-tint-{}", &hex[1..]);
    REGISTERED.with_borrow_mut(|seen| {
        if seen.contains(&class) {
            return;
        }
        if let Some(display) = gdk::Display::default() {
            let provider = gtk::CssProvider::new();
            provider.load_from_string(&format!(
                ".pf-pill.{class} {{ color: {hex}; background: alpha({hex}, 0.18); }}"
            ));
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            seen.insert(class.clone());
        }
    });
    class
}

/// `#RRGGBB` only — the value is interpolated into CSS, so anything else is refused rather
/// than injected.
fn is_hex_colour(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}
