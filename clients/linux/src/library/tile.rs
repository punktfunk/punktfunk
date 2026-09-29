//! The Library's tiles: a title's poster, a host's desktop, the shelf chips. Each is built fresh
//! when its row is bound, so a recycled row never shows another title's art.

use super::rows::{Caption, Tile};
use super::{LibraryMsg, View};
use crate::hosts::os_icon_name;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use pf_client_core::library::{initials, store_label, GameEntry};
use pf_client_core::library_layout::ago;
use std::rc::Rc;

pub const POSTER_W: i32 = 150;
pub const POSTER_H: i32 = 225;

/// A poster slot: a title, or the host's desktop.
pub fn poster(view: &Rc<View>, tile: Tile, caption: Caption) -> gtk::Widget {
    match tile {
        Tile::Game(i) => match view.games.borrow().get(i) {
            Some(g) => game_poster(view, g, caption),
            None => gtk::Box::new(gtk::Orientation::Vertical, 0).upcast(),
        },
        Tile::Desktop => desktop_poster(view),
    }
}

fn game_poster(view: &Rc<View>, g: &GameEntry, caption: Caption) -> gtk::Widget {
    let running = view.running.borrow().get(&g.id).cloned();
    let face = gtk::Overlay::new();
    face.set_child(Some(&placeholder(g)));
    let pic = gtk::Picture::new();
    pic.set_content_fit(gtk::ContentFit::Cover);
    view.art.show(g, &pic);
    face.add_overlay(&pic);
    let badge = pill(store_label(&g.store), g.is_launcher());
    badge.set_halign(gtk::Align::Start);
    badge.set_valign(gtk::Align::Start);
    face.add_overlay(&badge);
    if running.is_some() {
        let up = pill("Running", false);
        up.set_halign(gtk::Align::Start);
        up.set_valign(gtk::Align::End);
        face.add_overlay(&up);
    }
    face.add_css_class("pf-poster");
    if g.is_launcher() {
        face.add_css_class("pf-launcher");
    }
    face.set_overflow(gtk::Overflow::Hidden);
    face.set_size_request(POSTER_W, POSTER_H);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
    body.append(&face);
    let title = gtk::Label::builder()
        .label(&g.title)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(16)
        .css_classes(["caption-heading"])
        .build();
    body.append(&title);
    if let Some(text) = caption_text(g, caption) {
        let line = gtk::Label::builder()
            .label(text)
            .css_classes(["caption", "dim-label"])
            .build();
        body.append(&line);
    }

    let button = gtk::Button::builder()
        .child(&body)
        .css_classes(["flat", "pf-tile"])
        .tooltip_text(&g.title)
        .build();
    {
        let (sender, id) = (view.sender.clone(), g.id.clone());
        button.connect_clicked(move |_| sender.emit(LibraryMsg::Play(id.clone())));
    }
    let endable = running.as_ref().is_some_and(|r| r.endable);
    let favorite = view.favorites.borrow().contains(&g.id);
    attach_menu(
        &button,
        &title_menu(view, &g.id, running.is_some(), favorite, endable),
    );
    button.upcast()
}

/// What stands in until the art arrives, and stays when there is none: a launcher's brand mark
/// or name on an accent face, else the title's initials.
fn placeholder(g: &GameEntry) -> gtk::Widget {
    if let Some(mark) = mark(g) {
        return mark;
    }
    let label = if g.is_launcher() {
        let l = gtk::Label::new(Some(store_label(&g.store)));
        l.add_css_class("pf-poster-launcher-name");
        l
    } else {
        let l = gtk::Label::new(Some(&initials(&g.title)));
        l.add_css_class("pf-poster-monogram");
        l
    };
    label.set_halign(gtk::Align::Center);
    label.set_valign(gtk::Align::Center);
    label.set_vexpand(true);
    label.upcast()
}

/// The launcher-tile brand marks this shell ships symbolic art for (`data/icons/…`).
const LAUNCHER_ICON_TOKENS: &[&str] = &[
    "steam", "lutris", "heroic", "playnite", "epic", "gog", "xbox",
];

/// A brand mark from the gresource, else a Lucide mark; none once real art exists.
fn mark(g: &GameEntry) -> Option<gtk::Widget> {
    if !g.art.is_empty() {
        return None;
    }
    let token = g.icon_token()?;
    let mark: gtk::Widget = if LAUNCHER_ICON_TOKENS.contains(&token) {
        let img = gtk::Image::from_icon_name(&format!("pf-launcher-{token}-symbolic"));
        img.set_pixel_size(72);
        img.upcast()
    } else if pf_client_core::lucide::path(token).is_some() {
        crate::widgets::lucide::icon(token, 56).upcast()
    } else {
        return None;
    };
    mark.add_css_class("pf-poster-launcher-mark");
    mark.set_halign(gtk::Align::Center);
    mark.set_valign(gtk::Align::Center);
    mark.set_vexpand(true);
    Some(mark)
}

fn pill(text: &str, accent: bool) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.add_css_class("pf-pill");
    l.add_css_class("pf-store-badge");
    if accent {
        l.add_css_class("pf-launcher");
    }
    l.set_margin_start(6);
    l.set_margin_top(6);
    l.set_margin_bottom(6);
    l
}

/// Under Recently played or the Recent sort, when it was last played; under Most played, how
/// long. Nothing for a title never played here.
fn caption_text(g: &GameEntry, caption: Caption) -> Option<String> {
    let s = g.stats.as_ref()?;
    match caption {
        Caption::LastPlayed if s.last_played_unix_ms > 0 => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            Some(ago(now.saturating_sub(s.last_played_unix_ms)))
        }
        Caption::PlayTime if s.play_time_ms > 0 => Some(match s.play_time_ms / 3_600_000 {
            0 => "Under an hour".into(),
            h => format!("{h} h"),
        }),
        _ => None,
    }
}

/// Four rows, End Game a fifth only while the host says it can end it.
fn title_menu(
    view: &Rc<View>,
    id: &str,
    running: bool,
    favorite: bool,
    endable: bool,
) -> (gio::Menu, gio::SimpleActionGroup) {
    let actions = gio::SimpleActionGroup::new();
    let add = |name: &str, msg: fn(String) -> LibraryMsg| {
        let a = gio::SimpleAction::new(name, None);
        let (sender, id) = (view.sender.clone(), id.to_string());
        a.connect_activate(move |_, _| sender.emit(msg(id.clone())));
        actions.add_action(&a);
    };
    add("play", LibraryMsg::Play);
    add("favorite", LibraryMsg::ToggleFavorite);
    add("details", LibraryMsg::Details);
    add("copy-link", LibraryMsg::CopyLink);
    add("end-game", LibraryMsg::EndGame);
    let menu = gio::Menu::new();
    menu.append(
        Some(if running { "Resume" } else { "Play" }),
        Some("title.play"),
    );
    menu.append(
        Some(if favorite {
            "Remove from Favorites"
        } else {
            "Add to Favorites"
        }),
        Some("title.favorite"),
    );
    menu.append(Some("Details\u{2026}"), Some("title.details"));
    menu.append(Some("Copy Link"), Some("title.copy-link"));
    if endable {
        menu.append(Some("End Game"), Some("title.end-game"));
    }
    (menu, actions)
}

/// Right-click, a long press, or the Menu key opens the tile's menu at the pointer.
fn attach_menu(button: &gtk::Button, (menu, actions): &(gio::Menu, gio::SimpleActionGroup)) {
    button.insert_action_group("title", Some(actions));
    let popover = gtk::PopoverMenu::from_model(Some(menu));
    popover.set_parent(button);
    popover.set_has_arrow(false);
    {
        let popover = popover.clone();
        button.connect_destroy(move |_| popover.unparent());
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
    button.add_controller(right);
    let long = gtk::GestureLongPress::new();
    long.set_touch_only(true);
    {
        let popover = popover.clone();
        long.connect_pressed(move |_, x, y| at(&popover, x, y));
    }
    button.add_controller(long);
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
    button.add_controller(keys);
}

/// The shelf host's desktop as a grid tile, when the Desktops section is off.
fn desktop_poster(view: &Rc<View>) -> gtk::Widget {
    let face = gtk::Box::new(gtk::Orientation::Vertical, 0);
    face.add_css_class("pf-poster");
    face.add_css_class("pf-launcher");
    face.set_size_request(POSTER_W, POSTER_H);
    let icon = crate::widgets::lucide::icon("monitor", 56);
    icon.add_css_class("pf-poster-launcher-mark");
    icon.set_vexpand(true);
    face.append(&icon);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
    body.append(&face);
    body.append(
        &gtk::Label::builder()
            .label("Desktop")
            .css_classes(["caption-heading"])
            .build(),
    );
    let button = gtk::Button::builder()
        .child(&body)
        .css_classes(["flat", "pf-tile"])
        .tooltip_text("Stream the host's desktop")
        .build();
    let sender = view.sender.clone();
    button.connect_clicked(move |_| sender.emit(LibraryMsg::Desktop(None)));
    button.upcast()
}

/// One wide tile per paired host: its mark, its name, and what it has up.
pub fn desktops(view: &Rc<View>) -> gtk::Widget {
    let band = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    for (i, d) in view.desktops.borrow().iter().enumerate() {
        let avatar = adw::Avatar::new(40, Some(&d.name), true);
        if let Some(icon) = os_icon_name(&d.os) {
            avatar.set_show_initials(false);
            avatar.set_icon_name(Some(&icon));
        }
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_valign(gtk::Align::Center);
        text.append(
            &gtk::Label::builder()
                .label(&d.name)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["heading"])
                .build(),
        );
        let what = if d.playing.is_empty() {
            "Desktop".to_string()
        } else {
            format!("Resume {}", d.playing)
        };
        text.append(
            &gtk::Label::builder()
                .label(what)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        inner.append(&avatar);
        inner.append(&text);
        let button = gtk::Button::builder()
            .child(&inner)
            .css_classes(["card", "pf-desktop-tile"])
            .tooltip_text(format!("Stream {}'s desktop", d.name))
            .build();
        button.set_size_request(240, -1);
        let sender = view.sender.clone();
        button.connect_clicked(move |_| sender.emit(LibraryMsg::Desktop(Some(i))));
        band.append(&button);
    }
    scroller(&band)
}

/// A band of posters that scrolls sideways.
pub fn band(view: &Rc<View>, tiles: &[Tile], caption: Caption) -> gtk::Widget {
    let band = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    for t in tiles {
        band.append(&poster(view, *t, caption));
    }
    scroller(&band)
}

fn scroller(child: &gtk::Box) -> gtk::Widget {
    gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .child(child)
        .propagate_natural_height(true)
        .build()
        .upcast()
}

/// One row of the grid.
pub fn posters(view: &Rc<View>, tiles: &[Tile], caption: Caption) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    for t in tiles {
        row.append(&poster(view, *t, caption));
    }
    row.upcast()
}

/// The shelf picker: one chip per shelf, the chosen one pressed.
pub fn chips(view: &Rc<View>) -> gtk::Widget {
    let wrap = adw::WrapBox::builder()
        .child_spacing(8)
        .line_spacing(8)
        .build();
    let selected = view.selected.borrow().clone();
    let mut first: Option<gtk::ToggleButton> = None;
    for shelf in view.shelves.borrow().iter() {
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        if let Some(icon) = os_icon_name(&shelf.os) {
            content.append(&gtk::Image::from_icon_name(&icon));
        }
        content.append(&gtk::Label::new(Some(&shelf.label)));
        let chip = gtk::ToggleButton::builder()
            .child(&content)
            .css_classes(["pill", "pf-chip"])
            .active(selected.as_deref() == Some(shelf.key.as_str()))
            .build();
        match &first {
            Some(f) => chip.set_group(Some(f)),
            None => first = Some(chip.clone()),
        }
        let (sender, key) = (view.sender.clone(), shelf.key.clone());
        chip.connect_toggled(move |c| {
            if c.is_active() {
                sender.emit(LibraryMsg::Select(key.clone()));
            }
        });
        wrap.append(&chip);
    }
    wrap.upcast()
}
