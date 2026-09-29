//! A title's details (design §2.5): its cover and facts, what this device has played of it, and
//! what it does — play, favorite, the preset it always streams with, its link. An `AdwDialog`,
//! so a narrow window gets it as a bottom sheet.

use super::{LibraryMsg, Shelf, View};
use crate::store::Store;
use adw::prelude::*;
use pf_client_core::library::store_label;
use pf_client_core::library_layout::stats_line;
use std::rc::Rc;

pub fn show(
    parent: &impl IsA<gtk::Widget>,
    view: &Rc<View>,
    store: &Rc<Store>,
    shelf: &Shelf,
    id: &str,
) {
    let games = view.games.borrow();
    let Some(g) = games.iter().find(|g| g.id == id) else {
        return;
    };
    let running = view.running.borrow().get(id).cloned();
    let dialog = adw::Dialog::builder()
        .title(&g.title)
        .content_width(440)
        .build();

    let cover = gtk::Picture::new();
    cover.set_content_fit(gtk::ContentFit::Cover);
    cover.set_size_request(180, 270);
    cover.set_halign(gtk::Align::Center);
    cover.add_css_class("pf-poster");
    cover.set_overflow(gtk::Overflow::Hidden);
    view.art.show(g, &cover);

    let body = gtk::Box::new(gtk::Orientation::Vertical, 6);
    body.set_margin_top(12);
    body.set_margin_bottom(24);
    body.set_margin_start(24);
    body.set_margin_end(24);
    body.append(&cover);
    body.append(
        &gtk::Label::builder()
            .label(&g.title)
            .css_classes(["title-2"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .margin_top(6)
            .build(),
    );
    let year = g.release_year.map(|y| y.to_string());
    let facts: Vec<&str> = [
        Some(store_label(&g.store)),
        g.platform.as_deref(),
        year.as_deref(),
    ]
    .into_iter()
    .flatten()
    .collect();
    let dim = |text: &str| {
        gtk::Label::builder()
            .label(text)
            .css_classes(["dim-label"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build()
    };
    body.append(&dim(&facts.join(" \u{b7} ")));
    if let Some(dev) = g.developer.as_deref().filter(|d| !d.is_empty()) {
        body.append(&dim(dev));
    }
    if !g.genres.is_empty() {
        body.append(&dim(&g.genres.join(", ")));
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    if let Some(line) = g.stats.as_ref().and_then(|s| stats_line(s, now)) {
        body.append(&dim(&line));
    }

    let play = gtk::Button::builder()
        .label(if running.is_some() { "Resume" } else { "Play" })
        .css_classes(["pill", "suggested-action"])
        .build();
    {
        let (sender, id, dialog) = (view.sender.clone(), g.id.clone(), dialog.clone());
        play.connect_clicked(move |_| {
            sender.emit(LibraryMsg::Play(id.clone()));
            dialog.close();
        });
    }
    let heart = gtk::ToggleButton::builder()
        .child(&crate::widgets::lucide::row_icon("heart"))
        .active(view.favorites.borrow().contains(&g.id))
        .tooltip_text("Favorite")
        .css_classes(["circular"])
        .valign(gtk::Align::Center)
        .build();
    {
        let (sender, id) = (view.sender.clone(), g.id.clone());
        heart.connect_toggled(move |_| sender.emit(LibraryMsg::ToggleFavorite(id.clone())));
    }
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    buttons.set_halign(gtk::Align::Center);
    buttons.set_margin_top(12);
    buttons.set_margin_bottom(12);
    buttons.append(&play);
    buttons.append(&heart);
    body.append(&buttons);

    let group = adw::PreferencesGroup::new();
    group.add(&preset_row(store, shelf, &g.id));
    let row = |title: &str, msg: fn(String) -> LibraryMsg, close: bool| {
        let r = adw::ActionRow::builder()
            .title(title)
            .activatable(true)
            .build();
        let (sender, id, dialog) = (view.sender.clone(), g.id.clone(), dialog.clone());
        r.connect_activated(move |_| {
            sender.emit(msg(id.clone()));
            if close {
                dialog.close();
            }
        });
        r
    };
    group.add(&row("Copy Link", LibraryMsg::CopyLink, false));
    group.add(&row("Create Shortcut\u{2026}", LibraryMsg::Shortcut, false));
    if running.as_ref().is_some_and(|r| r.endable) {
        let end = row("End Game", LibraryMsg::EndGame, true);
        end.add_css_class("error");
        group.add(&end);
    }
    body.append(&group);

    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .child(&body)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroll));
    dialog.set_child(Some(&toolbar));
    dialog.present(Some(parent));
}

/// The preset this title always streams with, beating the host's own binding; "Same as the
/// host" clears it. The session resolves it on every launch of the title (design D6).
fn preset_row(store: &Rc<Store>, shelf: &Shelf, id: &str) -> adw::ComboRow {
    let presets: Vec<(String, String)> = store
        .presets()
        .presets
        .iter()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();
    let bound = {
        let known = store.hosts();
        shelf
            .host
            .index(&known)
            .and_then(|i| known.hosts[i].preset_for_game(id).map(str::to_string))
    };
    let mut labels = vec!["Same as the host"];
    labels.extend(presets.iter().map(|(_, n)| n.as_str()));
    let combo = adw::ComboRow::builder()
        .title("Always stream with")
        .subtitle("For this title on this host")
        .model(&gtk::StringList::new(&labels))
        .build();
    let at = bound
        .and_then(|b| presets.iter().position(|(pid, _)| *pid == b))
        .map_or(0, |i| i + 1);
    combo.set_selected(at as u32);
    let (store, host, id) = (store.clone(), shelf.host.clone(), id.to_string());
    combo.connect_selected_notify(move |row| {
        let pick = (row.selected() as usize)
            .checked_sub(1)
            .and_then(|i| presets.get(i))
            .map(|(pid, _)| pid.clone());
        let saved = store.update_hosts(|known| {
            if let Some(i) = host.index(known) {
                known.hosts[i].bind_game_preset(&id, pick.as_deref());
            }
        });
        if let Err(e) = saved {
            tracing::warn!(error = %format!("{e:#}"), "title preset not saved");
        }
    });
    combo
}
