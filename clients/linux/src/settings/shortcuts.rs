//! The Shortcuts page (planning design/client-shortcuts.md §5): what the player presses in a
//! stream, by device, each binding a row that opens its editor. The editor takes the action
//! from the quick-action picker, records keys on the real keyboard and has controller buttons
//! chosen from a grid, since this shell reads no controller. The blob lives in the dialog's
//! state and the binder writes it to the scope in view on every change, as it does the ring's.
//! Rows, defaults and rules are `pf_client_core::shortcuts`.

use super::quick_actions::{action_picker, describe, held_modifiers, key_name, unshifted_key_name};
use crate::trust::Settings;
use adw::prelude::*;
use gtk::{gdk, glib};
use pf_client_core::overlay_actions::{
    catalogue, CatalogueEntry, CatalogueGroup, GroupId, OverlayConfig, RingPlatform, SlotId,
};
use pf_client_core::shortcuts::{
    resolve, Action, Binding, Client, Conflict, Device, Group, Item, Press, Shortcuts, Trigger,
    BUTTONS, MODIFIERS,
};
use pf_l10n::{client, common, desktop};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

/// The most buttons one controller shortcut takes (design D3).
const MAX_BUTTONS: usize = 4;
/// Columns of the button grid.
const GRID_COLUMNS: i32 = 6;

thread_local! {
    /// The shortcuts of the scope the dialog shows, for the captions that name a chord. Set
    /// as the dialog opens and on each edit.
    static LIVE: RefCell<Shortcuts> = RefCell::new(live("", "auto"));
}

/// What a stream from this scope binds: the blob, or this client's defaults.
pub fn live(blob: &str, legacy_guide_gesture: &str) -> Shortcuts {
    let game_mode = pf_client_core::gamescope::under_gamescope();
    resolve(blob, Client::Desktop, game_mode, legacy_guide_gesture)
}

/// Point the captions at the scope the dialog opens on.
pub fn show_scope(s: &Settings) {
    LIVE.with(|l| *l.borrow_mut() = live(&s.shortcuts, &s.guide_gesture));
}

/// The first chord bound to `action` on `device` in the dialog's scope.
pub fn chord(action: Action, device: Device) -> Option<String> {
    LIVE.with(|l| l.borrow().hint(&action, device))
}

/// The keyboard chord bound to `key` and the controller chord bound to `pad`, when both are.
pub fn chords(key: Action, pad: Action) -> Option<(String, String)> {
    Some((chord(key, Device::Keyboard)?, chord(pad, Device::Pad)?))
}

/// The chords that open quick actions.
pub fn ring_chords() -> Option<(String, String)> {
    chords(Action::Ring, Action::Ring)
}

type ChangedFns = Rc<RefCell<Vec<Box<dyn Fn()>>>>;

/// The page's state, shared by the row, the subpages and the binder.
#[derive(Clone)]
struct Shared {
    dialog: glib::WeakRef<adw::PreferencesDialog>,
    blob: Rc<RefCell<String>>,
    /// The scope's old *Hold Select for guide* value, which the defaults still read.
    legacy: Rc<RefCell<String>>,
    /// The ring's blob as its editor holds it: its key combinations are actions here.
    ring: Rc<RefCell<String>>,
    changed: ChangedFns,
}

impl Shared {
    fn current(&self) -> Shortcuts {
        live(&self.blob.borrow(), &self.legacy.borrow())
    }

    fn ring(&self) -> OverlayConfig {
        OverlayConfig::parse(&self.ring.borrow(), RingPlatform::Desktop)
    }

    fn write(&self, s: &Shortcuts) {
        *self.blob.borrow_mut() = s.to_json();
        self.publish();
    }

    /// Back to the defaults, which is the empty blob. Emptied before anyone hears of it: the
    /// binder saves the blob as it is notified.
    fn reset(&self) {
        self.blob.borrow_mut().clear();
        self.publish();
    }

    fn publish(&self) {
        LIVE.with(|l| *l.borrow_mut() = self.current());
        for f in self.changed.borrow().iter() {
            f();
        }
    }

    fn push(&self, page: &adw::NavigationPage) {
        if let Some(d) = self.dialog.upgrade() {
            d.push_subpage(page);
        }
    }
}

/// The dialog's handle on the page: the row that opens it, and the blob it edits.
#[derive(Clone)]
pub struct ShortcutsPage {
    shared: Shared,
    row: adw::ActionRow,
}

impl ShortcutsPage {
    /// The row for the Input page, seeded with the scope's values. `ring` is the blob the
    /// Quick actions editor holds.
    pub fn new(
        dialog: &adw::PreferencesDialog,
        seed: &Settings,
        ring: Rc<RefCell<String>>,
    ) -> ShortcutsPage {
        let spec = &super::spec::PLAYER_SHORTCUTS;
        let row = adw::ActionRow::builder()
            .title(spec.title())
            .subtitle(spec.caption())
            .use_markup(false)
            .activatable(true)
            .build();
        row.add_suffix(&crate::widgets::lucide::row_icon("chevron-right"));
        let shared = Shared {
            dialog: dialog.downgrade(),
            blob: Rc::new(RefCell::new(seed.shortcuts.clone())),
            legacy: Rc::new(RefCell::new(seed.guide_gesture.clone())),
            ring,
            changed: Rc::default(),
        };
        let s = shared.clone();
        row.connect_activated(move |_| s.push(&list_page(&s)));
        ShortcutsPage { shared, row }
    }

    pub fn row(&self) -> &adw::ActionRow {
        &self.row
    }

    /// The blob as edited so far.
    pub fn blob(&self) -> Rc<RefCell<String>> {
        self.shared.blob.clone()
    }

    /// Put another scope's values in place (a preset's reset back to the inherited ones).
    pub fn set(&self, s: &Settings) {
        *self.shared.blob.borrow_mut() = s.shortcuts.clone();
        *self.shared.legacy.borrow_mut() = s.guide_gesture.clone();
        show_scope(s);
    }

    /// Fires on every edit the user makes.
    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.shared.changed.borrow_mut().push(Box::new(f));
    }
}

/// One row of the list: a binding by index, or a row this version cannot change.
#[derive(Debug)]
enum Line {
    Binding(usize),
    Fixed(Item),
}

/// The list's groups: `rows_in()`'s, with a two-chord row split into a row per binding.
fn lines(s: &Shortcuts, cfg: &OverlayConfig) -> Vec<(Group, Vec<Line>)> {
    s.rows_in(cfg, Client::Desktop, true)
        .into_iter()
        .map(|g| {
            let mut out = Vec::new();
            for item in &g.items {
                let Some(i) = item.binding else {
                    out.push(Line::Fixed(item.clone()));
                    continue;
                };
                out.push(Line::Binding(i));
                if matches!(item.keys, Press::Either(..)) {
                    out.extend(partner(s, i).map(Line::Binding));
                }
            }
            (g, out)
        })
        .collect()
}

/// The binding an `Either` row names second: the next press of the same action on the device.
fn partner(s: &Shortcuts, i: usize) -> Option<usize> {
    let b = &s.bindings[i];
    (i + 1..s.bindings.len()).find(|&j| {
        let o = &s.bindings[j];
        o.action == b.action && o.trigger.device() == b.trigger.device() && !o.trigger.hold()
    })
}

/// An action's name, and why it does nothing here when it does not.
fn action_text(action: &Action, cfg: &OverlayConfig) -> (String, String) {
    let gone = || client::shortcuts_not_on_device().to_string();
    match action {
        Action::Ring => (client::ring_open_ring().into(), String::new()),
        Action::Slot(SlotId::Shortcut(id)) if cfg.shortcut(id).is_none() => (action.id(), gone()),
        Action::Slot(slot) => describe(cfg, slot),
        Action::Unknown(id) => (id.clone(), gone()),
    }
}

/// The quick-action catalogue without the empty slot, *Quick actions* itself first.
fn picker_groups(cfg: &OverlayConfig) -> Vec<CatalogueGroup> {
    let mut groups: Vec<CatalogueGroup> = catalogue(cfg, RingPlatform::Desktop)
        .into_iter()
        .filter(|g| g.id != GroupId::Empty)
        .collect();
    if let Some(first) = groups.first_mut() {
        first.entries.insert(
            0,
            CatalogueEntry {
                id: Action::Ring.id(),
                label: client::ring_open_ring().into(),
                note: String::new(),
            },
        );
    }
    groups
}

/// The list: the lead, a group per device with a row per binding and the fixed rows, then Add
/// and Reset. Drawn again each time it shows, so an edit made in a subpage is on it.
fn list_page(shared: &Shared) -> adw::NavigationPage {
    let page = adw::PreferencesPage::new();
    page.add(
        &adw::PreferencesGroup::builder()
            .description(glib::markup_escape_text(client::shortcuts_lead()))
            .build(),
    );
    let footer = adw::PreferencesGroup::new();
    let add = adw::ButtonRow::builder()
        .title(client::shortcuts_add())
        .build();
    let reset = adw::ButtonRow::builder()
        .title(client::shortcuts_reset())
        .css_classes(["destructive-action"])
        .build();
    footer.add(&add);
    footer.add(&reset);

    // Weak: the rows the draw makes live inside the page it draws on.
    let drawn: RefCell<Vec<adw::PreferencesGroup>> = RefCell::default();
    let draw: Rc<dyn Fn()> = {
        let (shared, page, footer) = (shared.clone(), page.downgrade(), footer.downgrade());
        Rc::new(move || {
            if let (Some(page), Some(footer)) = (page.upgrade(), footer.upgrade()) {
                draw_groups(&shared, &page, &footer, &drawn);
            }
        })
    };
    draw();
    {
        let shared = shared.clone();
        add.connect_activated(move |_| shared.push(&editor_page(&shared, None)));
    }
    {
        let (shared, draw) = (shared.clone(), draw.clone());
        reset.connect_activated(move |row| confirm_reset(row, &shared, draw.clone()));
    }
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    let nav = adw::NavigationPage::new(&view, super::spec::PLAYER_SHORTCUTS.title());
    nav.connect_showing(move |_| draw());
    nav
}

fn draw_groups(
    shared: &Shared,
    page: &adw::PreferencesPage,
    footer: &adw::PreferencesGroup,
    drawn: &RefCell<Vec<adw::PreferencesGroup>>,
) {
    for g in drawn.borrow_mut().drain(..) {
        page.remove(&g);
    }
    if footer.parent().is_some() {
        page.remove(footer);
    }
    let (s, cfg) = (shared.current(), shared.ring());
    for (g, lines) in lines(&s, &cfg) {
        if lines.is_empty() {
            continue;
        }
        let group = adw::PreferencesGroup::builder()
            .title(glib::markup_escape_text(g.title()))
            .build();
        if let Some(note) = g.note() {
            group.set_description(Some(&glib::markup_escape_text(&note)));
        }
        for line in lines {
            match line {
                Line::Binding(i) => group.add(&binding_row(shared, &s.bindings[i], i, &cfg)),
                Line::Fixed(item) => group.add(&fixed_row(&item)),
            }
        }
        page.add(&group);
        drawn.borrow_mut().push(group);
    }
    page.add(footer);
}

fn binding_row(shared: &Shared, b: &Binding, i: usize, cfg: &OverlayConfig) -> adw::ActionRow {
    let (label, note) = action_text(&b.action, cfg);
    let row = adw::ActionRow::builder()
        .title(label)
        .use_markup(false)
        .activatable(true)
        .build();
    if !note.is_empty() {
        row.set_subtitle(&note);
        row.add_css_class("dim-label");
    }
    row.add_suffix(&chord_label(&b.trigger.label(Client::Desktop)));
    row.add_suffix(&crate::widgets::lucide::row_icon("chevron-right"));
    let shared = shared.clone();
    row.connect_activated(move |_| shared.push(&editor_page(&shared, Some(i))));
    row
}

fn fixed_row(item: &Item) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(item.text().as_ref())
        .subtitle(client::shortcuts_fixed())
        .use_markup(false)
        .build();
    row.add_suffix(&chord_label(&item.keys.to_string()));
    row
}

fn chord_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes(["dim-label"])
        .valign(gtk::Align::Center)
        .build()
}

/// Close responds Cancel: only the Reset button resets.
fn confirm_reset(anchor: &adw::ButtonRow, shared: &Shared, draw: Rc<dyn Fn()>) {
    let alert = adw::AlertDialog::new(
        Some(desktop::desktop_shortcuts_reset_title()),
        Some(desktop::desktop_shortcuts_reset_body()),
    );
    alert.add_responses(&[
        ("cancel", common::common_cancel()),
        ("reset", common::common_reset()),
    ]);
    alert.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
    alert.set_default_response(Some("cancel"));
    alert.set_close_response("cancel");
    let shared = shared.clone();
    alert.connect_response(None, move |_, response| {
        if response == "reset" {
            shared.reset();
            draw();
        }
    });
    alert.present(Some(anchor));
}

/// The bindings without the one being edited.
fn without(s: &Shortcuts, editing: Option<usize>) -> Shortcuts {
    let mut rest = s.clone();
    if let Some(i) = editing {
        rest.bindings.remove(i);
    }
    rest
}

/// `rest` without binding `j`, and where the edited binding goes back in.
fn replacing(mut rest: Shortcuts, at: Option<usize>, j: usize) -> (Shortcuts, Option<usize>) {
    rest.bindings.remove(j);
    (rest, at.map(|i| if j < i { i - 1 } else { i }))
}

/// `rest` with `cand` back at `at`, or at the end.
fn place(mut rest: Shortcuts, at: Option<usize>, cand: Binding) -> Result<Shortcuts, Conflict> {
    rest.validate(&cand)?;
    match at {
        Some(i) => rest.bindings.insert(i, cand),
        None => rest.bindings.push(cand),
    }
    Ok(rest)
}

/// Why `cand` cannot join `rest`, in one line. Indices point into `rest`.
fn problem(c: Conflict, rest: &Shortcuts, cand: &Binding, cfg: &OverlayConfig) -> String {
    match c {
        Conflict::PressSubset(j) => {
            let (mine, other) = (&cand.trigger, &rest.bindings[j].trigger);
            let (first, second) = if mine.ids().len() < other.ids().len() {
                (mine, other)
            } else {
                (other, mine)
            };
            client::shortcuts_subset(first.chord(Client::Desktop), second.chord(Client::Desktop))
        }
        Conflict::Identical(j) => client::shortcuts_identical_replace(
            cand.trigger.label(Client::Desktop),
            action_text(&rest.bindings[j].action, cfg).0,
        ),
        Conflict::SinglePress => client::shortcuts_one_button_hold().into(),
        Conflict::NoModifier | Conflict::NoKey => client::shortcuts_needs_modifier().into(),
    }
}

/// A recorded key with the modifiers a shortcut may hold. Win is the host's.
fn key_trigger(held: &[String], key: &str) -> Trigger {
    Trigger::Key {
        modifiers: MODIFIERS
            .iter()
            .filter(|m| held.iter().any(|h| h == *m))
            .map(|m| m.to_string())
            .collect(),
        key: key.into(),
    }
}

/// `trigger` with `bit` pressed or let go, in the order chosen. One button is a hold; a
/// second one starts as a press.
fn with_button(trigger: Option<&Trigger>, bit: u32, on: bool) -> Option<Trigger> {
    let (mut buttons, hold) = match trigger {
        Some(Trigger::Pad { buttons, hold }) => (buttons.clone(), *hold),
        _ => (Vec::new(), false),
    };
    let was = buttons.len();
    buttons.retain(|&b| b != bit);
    if on {
        buttons.push(bit);
    }
    let hold = match buttons.len() {
        0 => return None,
        1 => true,
        _ => hold && was > 1,
    };
    Some(Trigger::Pad { buttons, hold })
}

/// One shortcut being edited.
#[derive(Clone, Debug)]
struct Draft {
    action: Action,
    trigger: Option<Trigger>,
}

/// The editor's controls. Handlers hold it weakly and the page keeps it, so no control owns
/// the closure that owns it.
struct Editor {
    shared: Shared,
    editing: Option<usize>,
    draft: RefCell<Draft>,
    /// Set while `refresh` moves the controls, so their handlers stay out.
    quiet: Cell<bool>,
    does: adw::ActionRow,
    press: adw::ActionRow,
    record: gtk::ToggleButton,
    fires: adw::ActionRow,
    fires_on: adw::ToggleGroup,
    save: adw::ButtonRow,
    problem: gtk::Label,
}

/// A handler that reaches the editor while it lives.
fn on<A>(ed: &Rc<Editor>, f: impl Fn(&Rc<Editor>, &A) + 'static) -> impl Fn(&A) + 'static {
    let w: Weak<Editor> = Rc::downgrade(ed);
    move |a| {
        if let Some(ed) = w.upgrade() {
            f(&ed, a);
        }
    }
}

/// The editor: Does, Press (Record for keys, Choose for buttons), Fires on, then Save or Add,
/// and Remove for an existing one.
fn editor_page(shared: &Shared, editing: Option<usize>) -> adw::NavigationPage {
    let original = editing.and_then(|i| shared.current().bindings.get(i).cloned());
    let draft = match &original {
        Some(b) => Draft {
            action: b.action.clone(),
            trigger: Some(b.trigger.clone()),
        },
        None => Draft {
            action: Action::Ring,
            trigger: None,
        },
    };
    let row = |title: &str| {
        adw::ActionRow::builder()
            .title(title)
            .use_markup(false)
            .build()
    };
    let does = row(client::shortcuts_does());
    does.set_activatable(true);
    does.add_suffix(&crate::widgets::lucide::row_icon("chevron-right"));
    let press = row(client::shortcuts_press());
    let record = gtk::ToggleButton::builder()
        .label(client::shortcuts_record())
        .valign(gtk::Align::Center)
        .build();
    let choose = gtk::Button::builder()
        .label(client::shortcuts_choose())
        .valign(gtk::Align::Center)
        .build();
    press.add_suffix(&record);
    press.add_suffix(&choose);
    let fires = row(client::shortcuts_fires_on());
    let fires_on = adw::ToggleGroup::builder()
        .valign(gtk::Align::Center)
        .build();
    for (name, label) in [
        ("press", client::shortcuts_fires_press()),
        ("hold", client::shortcuts_fires_hold()),
    ] {
        fires_on.add(adw::Toggle::builder().name(name).label(label).build());
    }
    fires.add_suffix(&fires_on);
    let problem = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .css_classes(["error"])
        .visible(false)
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&does);
    group.add(&press);
    group.add(&fires);
    group.add(&problem);
    let save = adw::ButtonRow::builder()
        .title(if original.is_some() {
            common::common_save()
        } else {
            common::common_add()
        })
        .css_classes(["suggested-action"])
        .build();
    let actions = adw::PreferencesGroup::new();
    actions.add(&save);
    let remove = original.is_some().then(|| {
        adw::ButtonRow::builder()
            .title(client::shortcuts_remove())
            .css_classes(["destructive-action"])
            .build()
    });
    if let Some(r) = &remove {
        actions.add(r);
    }
    let page = adw::PreferencesPage::new();
    page.add(&group);
    page.add(&actions);

    let title = match &original {
        Some(b) => action_text(&b.action, &shared.ring()).0,
        None => client::shortcuts_add().into(),
    };
    let ed = Rc::new(Editor {
        shared: shared.clone(),
        editing,
        draft: RefCell::new(draft),
        quiet: Cell::new(false),
        does,
        press,
        record,
        fires,
        fires_on,
        save,
        problem,
    });
    ed.refresh();
    let pick = on(&ed, |ed, row: &adw::ActionRow| ed.pick(row));
    ed.does.connect_activated(move |row| pick(row));
    let toggled = on(&ed, |ed, _: &()| {
        if !ed.quiet.get() {
            ed.refresh();
        }
    });
    ed.record.connect_toggled(move |_| toggled(&()));
    let chosen = on(&ed, |ed, b: &gtk::Button| ed.choose(b));
    choose.connect_clicked(move |b| chosen(b));
    let held = on(&ed, |ed, _: &()| ed.set_hold());
    ed.fires_on.connect_active_name_notify(move |_| held(&()));
    let saved = on(&ed, |ed, _: &()| ed.commit_draft());
    ed.save.connect_activated(move |_| saved(&()));
    if let Some(r) = remove {
        let removed = on(&ed, |ed, _: &()| ed.remove());
        r.connect_activated(move |_| removed(&()));
    }
    // Record takes the next key with the modifiers held. A lone modifier keeps waiting.
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let pressed = Rc::downgrade(&ed);
    key.connect_key_pressed(move |_, keyval, keycode, state| match pressed.upgrade() {
        Some(ed) => ed.key(keyval, keycode, state),
        None => glib::Propagation::Proceed,
    });
    page.add_controller(key);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&page));
    let nav = adw::NavigationPage::new(&view, &title);
    let kept = RefCell::new(Some(ed));
    nav.connect_destroy(move |_| drop(kept.borrow_mut().take()));
    nav
}

impl Editor {
    fn refresh(&self) {
        self.quiet.set(true);
        let d = self.draft.borrow().clone();
        let (label, note) = action_text(&d.action, &self.shared.ring());
        self.does.set_subtitle(&if note.is_empty() {
            label
        } else {
            format!("{label} — {note}")
        });
        let chord = d.trigger.as_ref().map(|t| t.chord(Client::Desktop));
        self.press.set_subtitle(if self.record.is_active() {
            client::shortcuts_record_prompt()
        } else {
            chord.as_deref().unwrap_or_default()
        });
        let buttons = match &d.trigger {
            Some(Trigger::Pad { buttons, .. }) => Some(buttons.len()),
            _ => None,
        };
        let hold = d.trigger.as_ref().is_some_and(Trigger::hold);
        self.fires_on
            .set_active_name(Some(if hold { "hold" } else { "press" }));
        // A key is always a press, and one button always a hold.
        self.fires_on.set_sensitive(buttons.is_some_and(|n| n > 1));
        self.fires.set_subtitle(if buttons == Some(1) {
            client::shortcuts_one_button_hold()
        } else {
            ""
        });
        self.save.set_sensitive(d.trigger.is_some());
        self.problem.set_visible(false);
        self.quiet.set(false);
    }

    fn show_problem(&self, text: &str) {
        self.problem.set_text(text);
        self.problem.set_visible(true);
    }

    fn pick(self: &Rc<Self>, row: &adw::ActionRow) {
        let current = self.draft.borrow().action.id();
        let w = Rc::downgrade(self);
        action_picker(
            row,
            picker_groups(&self.shared.ring()),
            &current,
            move |id| {
                if let Some(ed) = w.upgrade() {
                    ed.draft.borrow_mut().action = Action::parse(&id);
                    ed.refresh();
                }
            },
        );
    }

    fn key(&self, keyval: gdk::Key, keycode: u32, state: gdk::ModifierType) -> glib::Propagation {
        if !self.record.is_active() {
            return glib::Propagation::Proceed;
        }
        let Some(name) = key_name(keyval).or_else(|| unshifted_key_name(keycode)) else {
            return glib::Propagation::Stop;
        };
        self.draft.borrow_mut().trigger = Some(key_trigger(&held_modifiers(state), name));
        self.record.set_active(false);
        self.refresh();
        glib::Propagation::Stop
    }

    /// The button grid, in this client's names, each toggle adding its button to the chord.
    fn choose(self: &Rc<Self>, anchor: &gtk::Button) {
        self.record.set_active(false);
        let held = match &self.draft.borrow().trigger {
            Some(Trigger::Pad { buttons, .. }) => buttons.clone(),
            _ => Vec::new(),
        };
        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(6)
            .column_homogeneous(true)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        for (i, &(_, bit)) in BUTTONS.iter().enumerate() {
            let name = Trigger::Pad {
                buttons: vec![bit],
                hold: false,
            }
            .chord(Client::Desktop);
            let b = gtk::ToggleButton::builder()
                .label(name)
                .active(held.contains(&bit))
                .build();
            let w = Rc::downgrade(self);
            b.connect_toggled(move |b| {
                let Some(ed) = w.upgrade() else { return };
                let next = with_button(ed.draft.borrow().trigger.as_ref(), bit, b.is_active());
                ed.draft.borrow_mut().trigger = next;
                if let Some(grid) = b.parent() {
                    limit(&grid);
                }
                ed.refresh();
            });
            let i = i as i32;
            grid.attach(&b, i % GRID_COLUMNS, i / GRID_COLUMNS, 1, 1);
        }
        limit(grid.upcast_ref());
        let popover = gtk::Popover::builder().child(&grid).build();
        popover.set_parent(anchor);
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn set_hold(&self) {
        if self.quiet.get() {
            return;
        }
        let hold = self.fires_on.active_name().as_deref() == Some("hold");
        if let Some(Trigger::Pad { hold: h, .. }) = &mut self.draft.borrow_mut().trigger {
            *h = hold;
        }
        self.refresh();
    }

    fn commit_draft(self: &Rc<Self>) {
        let d = self.draft.borrow().clone();
        let Some(trigger) = d.trigger else { return };
        let cand = Binding {
            trigger,
            action: d.action,
        };
        let rest = without(&self.shared.current(), self.editing);
        match place(rest.clone(), self.editing, cand.clone()) {
            Ok(s) => self.commit(&s),
            Err(Conflict::Identical(j)) => self.offer_replace(rest, cand, j),
            Err(c) => self.show_problem(&problem(c, &rest, &cand, &self.shared.ring())),
        }
    }

    /// "Hold Select already does Guide button. Replace it?" Close responds Cancel.
    fn offer_replace(self: &Rc<Self>, rest: Shortcuts, cand: Binding, j: usize) {
        let cfg = self.shared.ring();
        let alert = adw::AlertDialog::new(
            None,
            Some(&problem(Conflict::Identical(j), &rest, &cand, &cfg)),
        );
        alert.add_responses(&[
            ("cancel", common::common_cancel()),
            ("replace", client::shortcuts_replace()),
        ]);
        alert.set_default_response(Some("cancel"));
        alert.set_close_response("cancel");
        let w = Rc::downgrade(self);
        alert.connect_response(None, move |_, response| {
            let Some(ed) = w.upgrade() else { return };
            if response != "replace" {
                return;
            }
            let (rest, at) = replacing(rest.clone(), ed.editing, j);
            match place(rest.clone(), at, cand.clone()) {
                Ok(s) => ed.commit(&s),
                Err(c) => ed.show_problem(&problem(c, &rest, &cand, &cfg)),
            }
        });
        alert.present(Some(&self.save));
    }

    fn remove(&self) {
        if let Some(i) = self.editing {
            let s = without(&self.shared.current(), Some(i));
            self.commit(&s);
        }
    }

    fn commit(&self, s: &Shortcuts) {
        self.shared.write(s);
        if let Some(d) = self.shared.dialog.upgrade() {
            d.pop_subpage();
        }
    }
}

/// At most [`MAX_BUTTONS`] on: the rest of the grid waits until one is let go.
fn limit(grid: &gtk::Widget) {
    let toggles: Vec<gtk::ToggleButton> =
        std::iter::successors(grid.first_child(), |w| w.next_sibling())
            .filter_map(|w| w.downcast::<gtk::ToggleButton>().ok())
            .collect();
    let full = toggles.iter().filter(|t| t.is_active()).count() >= MAX_BUTTONS;
    for t in toggles {
        t.set_sensitive(t.is_active() || !full);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::shortcuts::button_bit;

    fn defaults() -> Shortcuts {
        resolve("", Client::Desktop, false, "auto")
    }

    fn pad(names: &[&str], hold: bool, action: Action) -> Binding {
        Binding {
            trigger: Trigger::Pad {
                buttons: names.iter().filter_map(|n| button_bit(n)).collect(),
                hold,
            },
            action,
        }
    }

    /// The list draws every Desktop default once, F11 and Alt+Enter as two rows, and the touch
    /// gestures as fixed rows.
    #[test]
    fn the_page_lists_the_desktop_defaults() {
        let s = defaults();
        let cfg = OverlayConfig::platform_default(RingPlatform::Desktop);
        let groups = lines(&s, &cfg);
        let mut seen: Vec<usize> = groups
            .iter()
            .flat_map(|(_, l)| l.iter())
            .filter_map(|l| match l {
                Line::Binding(i) => Some(*i),
                Line::Fixed(_) => None,
            })
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..s.bindings.len()).collect::<Vec<_>>());
        let fullscreen: Vec<String> = groups
            .iter()
            .flat_map(|(_, l)| l.iter())
            .filter_map(|l| match l {
                Line::Binding(i) if s.bindings[*i].action == Action::Slot(SlotId::Fullscreen) => {
                    Some(s.bindings[*i].trigger.label(Client::Desktop))
                }
                _ => None,
            })
            .collect();
        assert_eq!(fullscreen, ["F11", "Alt+Enter"]);
        let fixed = groups
            .iter()
            .flat_map(|(_, l)| l.iter())
            .filter(|l| matches!(l, Line::Fixed(_)))
            .count();
        assert_eq!(fixed, 2, "the two touch gestures");
        assert_eq!(action_text(&s.bindings[0].action, &cfg).0, "Quick actions");
        assert_eq!(
            action_text(&Action::parse("shortcut:gone"), &cfg).1,
            "Not on this device"
        );
    }

    /// A press that a longer chord contains is refused in words; an identical trigger offers
    /// Replace, and Replace swaps the action in place.
    #[test]
    fn the_editor_refuses_a_subset_and_offers_replace() {
        let s = defaults();
        let cfg = OverlayConfig::platform_default(RingPlatform::Desktop);
        let longer = pad(&["select", "a", "b"], false, Action::Slot(SlotId::Mic));
        let rest = without(&s, None);
        let err = place(rest.clone(), None, longer.clone()).unwrap_err();
        assert!(matches!(err, Conflict::PressSubset(_)), "{err:?}");
        assert_eq!(
            problem(err, &rest, &longer, &cfg),
            "Select + A would fire before Select + A + B can complete"
        );

        let same = pad(&["select", "a"], false, Action::Slot(SlotId::Stats));
        assert_eq!(
            place(rest.clone(), None, same.clone()),
            Err(Conflict::Identical(0))
        );
        assert_eq!(
            problem(Conflict::Identical(0), &rest, &same, &cfg),
            "Select + A already does Quick actions. Replace it?"
        );
        let (kept, at) = replacing(rest, None, 0);
        let out = place(kept, at, same.clone()).expect("replaced");
        assert_eq!(out.bindings.len(), s.bindings.len());
        assert!(out.bindings.contains(&same));
        assert!(!out
            .bindings
            .iter()
            .any(|b| b.action == Action::Ring && b.trigger.device() == Device::Pad));

        // A function key stands alone: F11 takes another action as it is.
        let f11 = s
            .bindings
            .iter()
            .position(|b| b.trigger.label(Client::Desktop) == "F11")
            .unwrap();
        let mut moved = s.bindings[f11].clone();
        moved.action = Action::Slot(SlotId::Minimize);
        let out = place(without(&s, Some(f11)), Some(f11), moved.clone()).unwrap();
        assert_eq!(out.bindings[f11], moved);
        let bare = Binding {
            trigger: key_trigger(&[], "k"),
            action: Action::Ring,
        };
        assert_eq!(
            place(without(&s, None), None, bare.clone()),
            Err(Conflict::NoModifier)
        );
        assert_eq!(
            problem(Conflict::NoModifier, &without(&s, None), &bare, &cfg),
            "Add Ctrl, Alt or Shift"
        );
    }

    /// Recording keeps Ctrl, Alt and Shift in order and drops Win; choosing makes one button a
    /// hold and a second a press.
    #[test]
    fn recorded_keys_and_chosen_buttons_make_triggers() {
        let held: Vec<String> = ["shift", "win", "ctrl"].map(String::from).into();
        assert_eq!(
            key_trigger(&held, "k").label(Client::Desktop),
            "Ctrl+Shift+K"
        );
        let select = button_bit("select").unwrap();
        let a = button_bit("a").unwrap();
        let one = with_button(None, select, true);
        assert_eq!(
            one.as_ref().map(|t| t.label(Client::Desktop)).as_deref(),
            Some("Hold Select")
        );
        let two = with_button(one.as_ref(), a, true);
        assert_eq!(
            two.as_ref().map(|t| t.label(Client::Desktop)).as_deref(),
            Some("Select + A")
        );
        assert_eq!(
            with_button(with_button(None, a, true).as_ref(), a, false),
            None
        );
    }

    /// Reset leaves the empty blob, and the binder hears of it only once it is empty.
    #[test]
    fn reset_writes_the_empty_blob() {
        let shared = Shared {
            dialog: glib::WeakRef::new(),
            blob: Rc::default(),
            legacy: Rc::new(RefCell::new("auto".into())),
            ring: Rc::default(),
            changed: Rc::default(),
        };
        let heard = Rc::new(RefCell::new(Vec::<String>::new()));
        {
            let (heard, blob) = (heard.clone(), shared.blob.clone());
            shared.changed.borrow_mut().push(Box::new(move || {
                heard.borrow_mut().push(blob.borrow().clone())
            }));
        }
        let mut s = defaults();
        s.bindings.truncate(1);
        shared.write(&s);
        assert_eq!(shared.current(), s);
        shared.reset();
        assert_eq!(heard.borrow().last().map(String::as_str), Some(""));
        assert_eq!(shared.current(), defaults());
    }
}
