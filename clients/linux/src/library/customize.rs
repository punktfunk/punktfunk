//! The Library's Customize panel: which sections show, in what order (design §2.5). One layout
//! per device, stored in `library_sections`, which the console reads too. The console's
//! Collections band is not drawn here; its entry keeps its stored place and state.

use crate::store::Store;
use adw::prelude::*;
use gtk::glib;
use pf_client_core::library_layout::{sections, stored_sections, Section};
use std::rc::Rc;

fn drawn(s: Section) -> bool {
    s != Section::Collections
}

pub fn popover(store: &Rc<Store>) -> gtk::Popover {
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let restore = gtk::Button::builder()
        .label("Restore Defaults")
        .css_classes(["flat"])
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(12);
    body.set_margin_end(12);
    body.set_size_request(320, -1);
    body.append(
        &gtk::Label::builder()
            .label("Library Sections")
            .css_classes(["heading"])
            .xalign(0.0)
            .build(),
    );
    body.append(&list);
    body.append(&restore);
    let popover = gtk::Popover::builder().child(&body).build();
    {
        let (list, store) = (list.clone(), store.clone());
        popover.connect_show(move |_| fill(&list, &store));
    }
    {
        let (list, store) = (list.clone(), store.clone());
        restore.connect_clicked(move |_| {
            store.update_settings(|s| s.library_sections.clear());
            fill(&list, &store);
        });
    }
    popover
}

/// Rebuild the rows from the store. Called after every change, from an idle so the row whose
/// handler asked is not torn down inside its own signal.
fn fill(list: &gtk::ListBox, store: &Rc<Store>) {
    list.remove_all();
    let all = sections(&store.settings().library_sections);
    let shown: Vec<(Section, bool)> = all.iter().copied().filter(|(s, _)| drawn(*s)).collect();
    for (i, (section, on)) in shown.iter().copied().enumerate() {
        let row = adw::SwitchRow::builder()
            .title(section.label())
            .active(on)
            .build();
        let mover = |icon: &str, tip: &str, to: Option<Section>| {
            let b = crate::widgets::lucide::button(icon);
            b.add_css_class("flat");
            b.set_valign(gtk::Align::Center);
            b.set_tooltip_text(Some(tip));
            b.set_sensitive(to.is_some());
            if let Some(to) = to {
                let (list, store) = (list.clone(), store.clone());
                b.connect_clicked(move |_| {
                    edit(&store, |v| swap(v, section, to));
                    refill(&list, &store);
                });
            }
            b
        };
        let up = i.checked_sub(1).map(|j| shown[j].0);
        let down = shown.get(i + 1).map(|(s, _)| *s);
        row.add_prefix(&mover("chevron-up", "Move up", up));
        row.add_prefix(&mover("chevron-down", "Move down", down));
        {
            let store = store.clone();
            row.connect_active_notify(move |r| {
                let on = r.is_active();
                edit(&store, |v| {
                    if let Some(entry) = v.iter_mut().find(|(s, _)| *s == section) {
                        entry.1 = on;
                    }
                });
            });
        }
        list.append(&row);
    }
}

fn refill(list: &gtk::ListBox, store: &Rc<Store>) {
    let (list, store) = (list.clone(), store.clone());
    glib::idle_add_local_once(move || fill(&list, &store));
}

fn edit(store: &Store, f: impl FnOnce(&mut Vec<(Section, bool)>)) {
    store.update_settings(|s| {
        let mut v = sections(&s.library_sections);
        f(&mut v);
        s.library_sections = stored_sections(&v);
    });
}

fn swap(v: &mut [(Section, bool)], a: Section, b: Section) {
    let at = |s: Section| v.iter().position(|(x, _)| *x == s);
    if let (Some(i), Some(j)) = (at(a), at(b)) {
        v.swap(i, j);
    }
}
