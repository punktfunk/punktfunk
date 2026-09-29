//! The scope switcher heading General: which layer the dialog edits, and the preset in scope's
//! colour, name and lifecycle. Changing scope closes the dialog and the app opens it again in
//! the new one, so one path builds the rows.

use super::choice::{restore_selected, ChoiceRow};
use super::pages::Build;
use super::Scope;
use crate::store::Store;
use adw::prelude::*;
use pf_client_core::presets::{PresetsFile, StreamPreset};
use std::cell::RefCell;
use std::rc::Rc;

/// Where a scope change wants the dialog to open again once it has closed.
pub type NextScope = Rc<RefCell<Option<Scope>>>;

pub fn scope_group(
    b: &Build,
    scope: &Scope,
    active: Option<&StreamPreset>,
    next: &NextScope,
    parent: &gtk::Widget,
) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .description(
            "A preset overrides only what you change here; everything else follows Default \
             settings.",
        )
        .build();
    let catalog = b.store.presets().clone();
    let mut labels: Vec<String> = vec!["Default settings".into()];
    labels.extend(catalog.presets.iter().map(|p| p.name.clone()));
    labels.push("New preset\u{2026}".into());
    let new_index = (labels.len() - 1) as u32;
    let current = match scope {
        Scope::Defaults => 0,
        Scope::Preset(id) => catalog
            .presets
            .iter()
            .position(|p| &p.id == id)
            .map_or(0, |i| i as u32 + 1),
    };
    let row = ChoiceRow::new(
        &b.dialog,
        b.inline,
        "Editing",
        "Which layer these settings belong to",
        &labels.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    row.set_selected(current);
    {
        let (dialog, next, parent, store) = (
            b.dialog.clone(),
            next.clone(),
            parent.clone(),
            b.store.clone(),
        );
        let ids: Vec<String> = catalog.presets.iter().map(|p| p.id.clone()).collect();
        let restore = row.restorer();
        row.connect_changed(move |i| {
            if i != new_index {
                *next.borrow_mut() =
                    Some(match i.checked_sub(1).and_then(|n| ids.get(n as usize)) {
                        Some(id) => Scope::Preset(id.clone()),
                        None => Scope::Defaults,
                    });
                dialog.close();
                return;
            }
            // Back on the layer being edited before asking: the prompt can be cancelled, and a
            // row parked on "New preset…" names the wrong layer and cannot be picked again.
            restore_selected(&restore, current);
            let (dialog, next, store) = (dialog.clone(), next.clone(), store.clone());
            prompt_name(
                &parent,
                "New preset",
                "Create",
                "",
                true,
                move |name, accent| {
                    let created = store.update_presets(|c| {
                        if c.name_taken(&name, None) {
                            return None;
                        }
                        let mut preset = StreamPreset::new(name.clone());
                        preset.accent = accent.clone();
                        let id = preset.id.clone();
                        c.presets.push(preset);
                        Some(id)
                    });
                    if let Some(Some(id)) = saved(&dialog, created) {
                        *next.borrow_mut() = Some(Scope::Preset(id));
                        dialog.close();
                    }
                },
            );
        });
    }
    g.add(row.widget());

    if let Some(active) = active {
        // Colour first: the preset's chips on host cards carry it.
        g.add(&colour_row(
            "Colour \u{2014} tints this preset's chips on host cards",
            active.accent.as_deref(),
            {
                let (dialog, store, id) =
                    (b.dialog.downgrade(), b.store.clone(), active.id.clone());
                move |hex| {
                    let r = store.update_presets(|c| {
                        if let Some(p) = c.presets.iter_mut().find(|p| p.id == id) {
                            p.accent = (!hex.is_empty()).then(|| hex.clone());
                        }
                    });
                    if let Some(d) = dialog.upgrade() {
                        saved(&d, r);
                    }
                }
            },
        ));
        let actions = adw::ActionRow::builder()
            .title(&active.name)
            .subtitle("This preset")
            .use_markup(false)
            .build();
        let buttons = gtk::Box::builder()
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        for (label, action) in [
            ("Rename\u{2026}", PresetAction::Rename),
            ("Duplicate", PresetAction::Duplicate),
            ("Delete\u{2026}", PresetAction::Delete),
        ] {
            let button = gtk::Button::builder().label(label).build();
            if matches!(action, PresetAction::Delete) {
                button.add_css_class("destructive-action");
            }
            let ctx = Ctx {
                store: b.store.clone(),
                dialog: b.dialog.clone(),
                next: next.clone(),
                parent: parent.clone(),
            };
            let (id, name) = (active.id.clone(), active.name.clone());
            button.connect_clicked(move |_| run(action, &ctx, &id, &name));
            buttons.append(&button);
        }
        actions.add_suffix(&buttons);
        g.add(&actions);
    }
    g
}

#[derive(Clone, Copy)]
enum PresetAction {
    Rename,
    Duplicate,
    Delete,
}

#[derive(Clone)]
struct Ctx {
    store: Rc<Store>,
    dialog: adw::PreferencesDialog,
    next: NextScope,
    parent: gtk::Widget,
}

impl Ctx {
    /// Open the dialog again on `scope` once this catalog edit has landed.
    fn reopen(&self, scope: Scope) {
        *self.next.borrow_mut() = Some(scope);
        self.dialog.close();
    }
}

/// Rename, duplicate or delete the preset in scope. Each reopens the dialog, so the switcher
/// and the catalog cannot disagree.
fn run(action: PresetAction, ctx: &Ctx, id: &str, name: &str) {
    let id = id.to_string();
    match action {
        PresetAction::Rename => {
            let (ctx, parent) = (ctx.clone(), ctx.parent.clone());
            prompt_name(
                &parent,
                "Rename preset",
                "Rename",
                name,
                false,
                move |new, _| {
                    let r = ctx.store.update_presets(|c| {
                        if c.name_taken(&new, Some(&id)) {
                            return false;
                        }
                        if let Some(p) = c.presets.iter_mut().find(|p| p.id == id) {
                            p.name = new.clone();
                        }
                        true
                    });
                    if saved(&ctx.dialog, r) == Some(true) {
                        ctx.reopen(Scope::Preset(id.clone()));
                    }
                },
            );
        }
        // Every row wrote as it changed, so the copy is of what the user sees.
        PresetAction::Duplicate => {
            let copy = ctx.store.update_presets(|c| c.duplicate(&id));
            if let Some(Some(new_id)) = saved(&ctx.dialog, copy) {
                ctx.reopen(Scope::Preset(new_id));
            }
        }
        PresetAction::Delete => {
            // The warning counts what breaks: hosts that fall back to the defaults, and pinned
            // cards that disappear (design §6).
            let (bound, pinned) = {
                let known = ctx.store.hosts();
                let bound = known
                    .hosts
                    .iter()
                    .filter(|h| h.preset_id.as_deref() == Some(id.as_str()))
                    .count();
                let pinned = known
                    .hosts
                    .iter()
                    .filter(|h| h.pinned_presets.iter().any(|p| p == &id))
                    .count();
                (bound, pinned)
            };
            let mut body = format!("\u{201c}{name}\u{201d} will be removed.");
            if bound > 0 {
                body.push_str(&format!(
                    "\n\n{bound} host{} will fall back to Default settings.",
                    if bound == 1 { "" } else { "s" }
                ));
            }
            if pinned > 0 {
                body.push_str(&format!(
                    "\n{pinned} pinned card{} will disappear.",
                    if pinned == 1 { "" } else { "s" }
                ));
            }
            let confirm = adw::AlertDialog::new(Some("Delete preset?"), Some(&body));
            confirm.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
            confirm.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
            confirm.set_default_response(Some("cancel"));
            confirm.set_close_response("cancel");
            let (ctx, parent) = (ctx.clone(), ctx.parent.clone());
            confirm.connect_response(Some("delete"), move |_, _| {
                // Bindings and pins stay: they resolve as no preset everywhere.
                let r = ctx
                    .store
                    .update_presets(|c| c.presets.retain(|p| p.id != id));
                if saved(&ctx.dialog, r).is_some() {
                    ctx.reopen(Scope::Defaults);
                }
            });
            confirm.present(Some(&parent));
        }
    }
}

/// Report a failed catalog write on the dialog that asked for it, so a button that did nothing
/// does not just look dead.
pub fn saved<T>(dialog: &adw::PreferencesDialog, r: anyhow::Result<T>) -> Option<T> {
    match r {
        Ok(v) => Some(v),
        Err(e) => {
            dialog.add_toast(adw::Toast::new(&format!("Couldn't save \u{2014} {e:#}")));
            None
        }
    }
}

/// The chip palette a preset can carry (`StreamPreset.accent`). Eight entries rather than a
/// full colour picker: the point is telling presets apart at a glance on a host card, which a
/// small set of legible, contrast-checked colours does better than free choice — and the
/// schema's `#RRGGBB` still accepts anything a future picker or a hand-edit writes.
const SWATCHES: &[(&str, &str, &str)] = &[
    ("", "pf-swatch-none", "No colour"),
    ("#e01b24", "pf-swatch-red", "Red"),
    ("#ff7800", "pf-swatch-orange", "Orange"),
    ("#f6d32d", "pf-swatch-yellow", "Yellow"),
    ("#33d17a", "pf-swatch-green", "Green"),
    ("#3584e4", "pf-swatch-blue", "Blue"),
    ("#9141ac", "pf-swatch-purple", "Purple"),
    ("#d16d9e", "pf-swatch-pink", "Pink"),
    ("#77767b", "pf-swatch-slate", "Slate"),
];

/// A colour picker as its own full-width row: the caption above, the swatches on their own
/// line beneath, wrapping if the dialog is narrow.
///
/// They started as an [`adw::ActionRow`] suffix, which is where a control that size normally
/// goes — but nine swatches squeeze the row's title until it is unreadable, and squeezing the
/// label to fit the control is backwards. A `FlowBox` beneath keeps both legible at any width.
fn colour_row(
    title: &str,
    current: Option<&str>,
    on_pick: impl Fn(String) + 'static,
) -> adw::PreferencesRow {
    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    box_.append(
        &gtk::Label::builder()
            .label(title)
            .halign(gtk::Align::Start)
            .build(),
    );
    box_.append(&swatch_row(current, on_pick));
    adw::PreferencesRow::builder()
        .activatable(false)
        .child(&box_)
        .build()
}

/// The swatches themselves, calling back with the chosen `#RRGGBB` (empty = none).
fn swatch_row(current: Option<&str>, on_pick: impl Fn(String) + 'static) -> gtk::FlowBox {
    let row = gtk::FlowBox::builder()
        .orientation(gtk::Orientation::Horizontal)
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(8)
        .row_spacing(8)
        .min_children_per_line(5)
        .max_children_per_line(9)
        // NOT homogeneous, and start-aligned: a homogeneous FlowBox stretches every child to
        // the widest one, which turns 26px circles into wide rounded rectangles the moment the
        // row has space to spare.
        .homogeneous(false)
        .halign(gtk::Align::Start)
        .build();
    let on_pick = Rc::new(on_pick);
    let buttons: Rc<RefCell<Vec<(String, gtk::Button)>>> = Rc::default();
    for (hex, class, name) in SWATCHES {
        let b = gtk::Button::builder()
            .css_classes(["pf-swatch", class, "circular"])
            .tooltip_text(*name)
            .valign(gtk::Align::Center)
            .build();
        if current.unwrap_or("") == *hex {
            b.add_css_class("pf-swatch-on");
        }
        {
            let (on_pick, buttons, hex) = (on_pick.clone(), buttons.clone(), hex.to_string());
            b.connect_clicked(move |me| {
                // The selection ring is exclusive, so clear every sibling before setting ours.
                for (_, other) in buttons.borrow().iter() {
                    other.remove_css_class("pf-swatch-on");
                }
                me.add_css_class("pf-swatch-on");
                on_pick(hex.clone());
            });
        }
        buttons.borrow_mut().push((hex.to_string(), b.clone()));
        row.insert(&b, -1);
    }
    row
}

/// A one-line name prompt (create/rename). Refuses empty and duplicate names in place —
/// menus keyed by name are ambiguous otherwise (design §6) — rather than failing after the
/// dialog is gone.
fn prompt_name(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    accept: &str,
    initial: &str,
    with_colour: bool,
    on_ok: impl Fn(String, Option<String>) + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), None);
    let entry = adw::EntryRow::builder().title("Name").build();
    entry.set_text(initial);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&entry);
    // Creating a preset picks its colour here, in the same breath as its name — going hunting
    // for it afterwards is exactly the friction that leaves every preset grey.
    let accent: Rc<RefCell<Option<String>>> = Rc::default();
    if with_colour {
        list.append(&colour_row("Colour", None, {
            let accent = accent.clone();
            move |hex| *accent.borrow_mut() = (!hex.is_empty()).then(|| hex.clone())
        }));
    }
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", accept)]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    let taken_against = initial.to_string();
    let e = entry.clone();
    let d = dialog.clone();
    let validate = move || {
        let name = e.text().trim().to_string();
        let catalog = PresetsFile::load();
        let dup = !name.eq_ignore_ascii_case(&taken_against) && catalog.name_taken(&name, None);
        d.set_response_enabled("ok", !name.is_empty() && !dup);
        e.set_title(if dup {
            "Name — already used by another preset"
        } else {
            "Name"
        });
    };
    validate();
    {
        let validate = validate.clone();
        entry.connect_changed(move |_| validate());
    }
    let e = entry.clone();
    dialog.connect_response(Some("ok"), move |_, _| {
        let name = e.text().trim().to_string();
        if !name.is_empty() {
            on_ok(name, accent.borrow().clone());
        }
    });
    dialog.present(Some(parent));
}
