//! The scope switcher heading General: which layer the dialog edits, and the preset in scope's
//! colour, name and lifecycle. Changing scope closes the dialog and the app opens it again in
//! the new one, so one path builds the rows.

use super::choice::{restore_selected, ChoiceRow};
use super::pages::Build;
use super::tables::Text;
use super::Scope;
use crate::store::Store;
use adw::prelude::*;
use pf_client_core::presets::{PresetsFile, StreamPreset};
use pf_l10n::{client, common, desktop};
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
        .description(desktop::desktop_prefs_scope_hint())
        .build();
    let catalog = b.store.presets().clone();
    let mut labels: Vec<String> = vec![client::prefs_scope_defaults().into()];
    labels.extend(catalog.presets.iter().map(|p| p.name.clone()));
    labels.push(client::preset_new().into());
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
        client::prefs_scope_editing(),
        desktop::desktop_prefs_scope_editing_hint(),
        &labels.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    row.set_selected(current);
    {
        let ctx = Ctx {
            store: b.store.clone(),
            dialog: b.dialog.clone(),
            next: next.clone(),
            parent: parent.clone(),
        };
        let ids: Vec<String> = catalog.presets.iter().map(|p| p.id.clone()).collect();
        let restore = row.restorer();
        row.connect_changed(move |i| {
            if i != new_index {
                ctx.reopen(match i.checked_sub(1).and_then(|n| ids.get(n as usize)) {
                    Some(id) => Scope::Preset(id.clone()),
                    None => Scope::Defaults,
                });
                return;
            }
            // Back on the layer being edited before asking: the prompt can be cancelled, and a
            // row parked on "New preset…" names the wrong layer and cannot be picked again.
            restore_selected(&restore, current);
            new_preset(&ctx);
        });
    }
    g.add(row.widget());

    if let Some(active) = active {
        // Colour first: the preset's chips on host cards carry it.
        g.add(&colour_row(
            client::preset_color(),
            Some(client::preset_color_hint()),
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
            .subtitle(desktop::desktop_preset_this())
            .use_markup(false)
            .build();
        let buttons = gtk::Box::builder()
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();
        for (label, action) in [
            (client::preset_menu_rename(), PresetAction::Rename),
            (common::common_duplicate(), PresetAction::Duplicate),
            (client::preset_menu_delete(), PresetAction::Delete),
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

/// Where a new preset starts: Empty asks its name and colour, Second screen saves core's
/// starter under a free name. Either reopens the dialog on the new preset.
fn new_preset(ctx: &Ctx) {
    let empty = {
        let ctx = ctx.clone();
        move || {
            let on_ok = {
                let ctx = ctx.clone();
                move |name: String, accent: Option<String>| {
                    let created = ctx.store.update_presets(|c| {
                        if c.name_taken(&name, None) {
                            return None;
                        }
                        let mut preset = StreamPreset::new(name.clone());
                        preset.accent = accent.clone();
                        let id = preset.id.clone();
                        c.presets.push(preset);
                        Some(id)
                    });
                    if let Some(Some(id)) = saved(&ctx.dialog, created) {
                        ctx.reopen(Scope::Preset(id));
                    }
                }
            };
            let (title, accept) = (client::preset_new_title(), common::common_create());
            prompt_name(&ctx.parent, title, accept, "", true, on_ok);
        }
    };
    let second = {
        let ctx = ctx.clone();
        move || {
            let created = ctx.store.update_presets(|c| {
                let preset = StreamPreset::second_screen(|n| c.name_taken(n, None));
                let id = preset.id.clone();
                c.presets.push(preset);
                id
            });
            if let Some(id) = saved(&ctx.dialog, created) {
                ctx.reopen(Scope::Preset(id));
            }
        }
    };
    prompt_starter(&ctx.parent, empty, second);
}

/// The starters as two rows under the New preset heading, each saying what it starts from.
/// Picking one closes the prompt and runs it.
fn prompt_starter(parent: &gtk::Widget, empty: impl Fn() + 'static, second: impl Fn() + 'static) {
    let dialog = adw::AlertDialog::new(Some(client::preset_new_title()), None);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let (e, e_hint) = (client::preset_starter_empty(), client::preset_new_hint());
    list.append(&starter_row(&dialog, e, e_hint, empty));
    let (s, s_hint) = (
        client::screen_role_second(),
        client::screen_role_second_hint(),
    );
    list.append(&starter_row(&dialog, s, s_hint, second));
    dialog.set_extra_child(Some(&list));
    dialog.add_response("cancel", common::common_cancel());
    dialog.set_close_response("cancel");
    dialog.present(Some(parent));
}

/// One starter: its name, what it starts from, and `start` after the prompt closes.
fn starter_row(
    dialog: &adw::AlertDialog,
    title: &str,
    hint: &str,
    start: impl Fn() + 'static,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(hint)
        .use_markup(false)
        .activatable(true)
        .build();
    row.add_suffix(&crate::widgets::lucide::row_icon("chevron-right"));
    let dialog = dialog.downgrade();
    row.connect_activated(move |_| {
        if let Some(d) = dialog.upgrade() {
            d.close();
        }
        start();
    });
    row
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
                client::preset_rename_title(),
                common::common_rename(),
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
            let mut body = desktop::desktop_preset_delete_body(name);
            if bound > 0 {
                body.push_str("\n\n");
                body.push_str(&client::preset_delete_hosts(bound as u64));
            }
            if pinned > 0 {
                body.push('\n');
                body.push_str(&client::preset_delete_cards(pinned as u64));
            }
            let confirm =
                adw::AlertDialog::new(Some(desktop::desktop_preset_delete_title()), Some(&body));
            confirm.add_responses(&[
                ("cancel", common::common_cancel()),
                ("delete", common::common_delete()),
            ]);
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
            dialog.add_toast(adw::Toast::new(&desktop::desktop_save_failed(format!(
                "{e:#}"
            ))));
            None
        }
    }
}

/// The chip palette a preset can carry (`StreamPreset.accent`). Eight entries rather than a
/// full colour picker: the point is telling presets apart at a glance on a host card, which a
/// small set of legible, contrast-checked colours does better than free choice — and the
/// schema's `#RRGGBB` still accepts anything a future picker or a hand-edit writes.
const SWATCHES: &[(&str, &str, Text)] = &[
    ("", "pf-swatch-none", client::preset_color_none),
    ("#e01b24", "pf-swatch-red", client::preset_color_red),
    ("#ff7800", "pf-swatch-orange", client::preset_color_orange),
    (
        "#f6d32d",
        "pf-swatch-yellow",
        desktop::desktop_preset_color_yellow,
    ),
    ("#33d17a", "pf-swatch-green", client::preset_color_green),
    ("#3584e4", "pf-swatch-blue", client::preset_color_blue),
    (
        "#9141ac",
        "pf-swatch-purple",
        desktop::desktop_preset_color_purple,
    ),
    ("#d16d9e", "pf-swatch-pink", client::preset_color_pink),
    (
        "#77767b",
        "pf-swatch-slate",
        desktop::desktop_preset_color_slate,
    ),
];

/// A colour picker as its own full-width row: the title and caption above, the swatches on
/// their own line beneath, wrapping if the dialog is narrow.
///
/// They started as an [`adw::ActionRow`] suffix, which is where a control that size normally
/// goes — but nine swatches squeeze the row's title until it is unreadable, and squeezing the
/// label to fit the control is backwards. A `FlowBox` beneath keeps both legible at any width.
fn colour_row(
    title: &str,
    caption: Option<&str>,
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
    if let Some(caption) = caption {
        box_.append(
            &gtk::Label::builder()
                .label(caption)
                .halign(gtk::Align::Start)
                .xalign(0.0)
                .wrap(true)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }
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
            .tooltip_text(name())
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
    let entry = adw::EntryRow::builder()
        .title(client::preset_name())
        .build();
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
        list.append(&colour_row(client::preset_color(), None, None, {
            let accent = accent.clone();
            move |hex| *accent.borrow_mut() = (!hex.is_empty()).then(|| hex.clone())
        }));
    }
    dialog.set_extra_child(Some(&list));
    dialog.add_responses(&[("cancel", common::common_cancel()), ("ok", accept)]);
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
            desktop::desktop_preset_name_taken()
        } else {
            client::preset_name()
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
