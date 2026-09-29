//! The Library's rows as widgets. A row keeps its widgets, and binding it to another row of the
//! same kind shows the new content in them, so scrolling a large library builds nothing new.

use super::poster::Poster;
use super::rows::{Caption, Row, Tile};
use super::{LibraryMsg, View};
use crate::hosts::os_icon_name;
use adw::prelude::*;
use std::rc::Rc;

/// The space between posters, across and down.
pub const GAP: i32 = 16;

/// Show `row` in `holder`, keeping what the holder shows when it is the same kind of row.
pub fn bind(holder: &gtk::Box, view: &Rc<View>, row: &Row) {
    match row {
        Row::Heading(text) => {
            let label = reuse(holder, "heading", || {
                gtk::Label::builder()
                    .xalign(0.0)
                    .css_classes(["heading"])
                    .margin_top(12)
                    .build()
            });
            label.set_label(text);
        }
        // A few hosts: rebuilt on each bind.
        Row::Desktops => {
            let slot = reuse(holder, "desktops", || {
                gtk::Box::new(gtk::Orientation::Vertical, 0)
            });
            while let Some(c) = slot.first_child() {
                slot.remove(&c);
            }
            slot.append(&desktops(view));
        }
        Row::Band { tiles, caption } => band(holder, view, tiles, *caption),
        Row::Posters { tiles, caption } => posters(holder, view, tiles, *caption),
    }
}

/// The holder's widget named `name`, shown and the others hidden; made by `make` the first
/// time. A holder keeps one widget per kind of row, so a list item bound to a heading and then
/// to a grid row builds each once.
fn reuse<W: IsA<gtk::Widget>>(holder: &gtk::Box, name: &str, make: impl FnOnce() -> W) -> W {
    let mut kept = None;
    let mut child = holder.first_child();
    while let Some(c) = child {
        let mine = c.widget_name() == name;
        c.set_visible(mine);
        if mine {
            kept = c.clone().downcast::<W>().ok();
        }
        child = c.next_sibling();
    }
    if let Some(w) = kept {
        return w;
    }
    let w = make();
    w.set_widget_name(name);
    holder.append(&w);
    w
}

fn show(poster: &Poster, view: &View, tile: Tile, caption: Caption) {
    match tile {
        Tile::Game(i) => match view.games.borrow().get(i) {
            Some(g) => poster.show_game(view, g, caption),
            None => poster.show_nothing(),
        },
        Tile::Desktop => poster.show_desktop(),
    }
}

/// One row of the grid: as many equal slots as the grid has columns, so posters stretch to the
/// row's width and a short last row keeps the others' size. Slots past the column count stay,
/// hidden, for when the window widens again.
fn posters(holder: &gtk::Box, view: &Rc<View>, tiles: &[Tile], caption: Caption) {
    let row = reuse(holder, "posters", || {
        gtk::Box::builder().homogeneous(true).spacing(GAP).build()
    });
    let slots = view.columns.get().max(tiles.len()).max(1);
    let mut kids: Vec<gtk::Widget> =
        std::iter::successors(row.first_child(), |w| w.next_sibling()).collect();
    while kids.len() < slots {
        let p = Poster::new(view);
        row.append(p.widget());
        kids.push(p.widget().clone().upcast());
    }
    for (i, w) in kids.iter().enumerate() {
        w.set_visible(i < slots);
        if i >= slots {
            continue;
        }
        let Some(p) = Poster::of(w) else { continue };
        match tiles.get(i) {
            Some(tile) => show(&p, view, *tile, caption),
            None => p.show_nothing(),
        }
    }
}

/// A band of posters that scrolls sideways, each at its natural width. Its tiles are kept and
/// shown again like a grid row's, and it measures the same every time, so a band's height is
/// known before it is drawn.
fn band(holder: &gtk::Box, view: &Rc<View>, tiles: &[Tile], caption: Caption) {
    let scroller = reuse(holder, "band", || {
        gtk::ScrolledWindow::builder()
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .child(&gtk::Box::new(gtk::Orientation::Horizontal, GAP))
            .build()
    });
    let Some(row) = scroller
        .child()
        .and_downcast::<gtk::Viewport>()
        .and_then(|v| v.child())
        .and_downcast::<gtk::Box>()
    else {
        return;
    };
    let mut kids: Vec<gtk::Widget> =
        std::iter::successors(row.first_child(), |w| w.next_sibling()).collect();
    while kids.len() < tiles.len() {
        let p = Poster::new(view);
        p.set_fixed();
        row.append(p.widget());
        kids.push(p.widget().clone().upcast());
    }
    for (i, w) in kids.iter().enumerate() {
        w.set_visible(i < tiles.len());
        if let (Some(tile), Some(p)) = (tiles.get(i), Poster::of(w)) {
            show(&p, view, *tile, caption);
        }
    }
}

/// One wide tile per paired host: its mark, its name, and what it has up.
fn desktops(view: &Rc<View>) -> gtk::Widget {
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

fn scroller(child: &gtk::Box) -> gtk::Widget {
    gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::Never)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .child(child)
        .propagate_natural_height(true)
        .build()
        .upcast()
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
