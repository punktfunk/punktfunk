//! The quick-action ring's editor in the GTK shell (design/touch-client-overlay.md §3.3): the
//! editor IS the ring — six round buttons on the ring's own geometry over a flat card stage,
//! built from GTK's own widgets so the toolkit's focus, drag-and-drop and screen reader carry
//! it. Each disc wears the slot's Lucide mark, the same mark the in-stream ring draws for it.
//! A click (or Enter) on a button picks what it holds from the catalogue by group; a button
//! dragged onto another swaps the two, and the picker offers the same swap for a keyboard.
//! Under the ring the shortcuts sit as rows; a row opens the shortcut editor — a name, the four
//! modifiers, the key on a keyboard-shaped grid or pressed on the real keyboard — as subpages
//! of the preferences dialog. The blob lives in the dialog's state and is written with the other
//! rows when the dialog closes; the model (catalogue, geometry, chords, icons) is
//! `pf_client_core`'s.

use adw::prelude::*;
use gtk::gdk;
use gtk::glib;
use pf_client_core::overlay_actions::{
    catalogue, chord_chip, key_legend, slot_icon, Chord, OverlayConfig, RingPlatform, Shortcut,
    SlotId, CHORD_MODIFIERS, KEY_GRID, RING_SLOTS,
};
use pf_client_core::ring::{slot_offset, CENTRE_DIAMETER, RING_RADIUS, SLOT_DIAMETER};
use std::cell::RefCell;
use std::rc::Rc;

/// The stage the ring sits on, in px: the ring's diameter plus a disc plus a margin each way.
const STAGE_W: i32 = 440;
const STAGE_H: i32 = 340;
/// The Lucide mark on a disc, in px. The console draws it at 1.05× the disc's radius; this is
/// that, so a disc reads the same weight in the editor as it does in the stream.
const ICON_PX: i32 = (SLOT_DIAMETER * 1.05 / 2.0) as i32;

const CLOCK: [&str; RING_SLOTS] = [
    "12 o'clock",
    "2 o'clock",
    "4 o'clock",
    "6 o'clock",
    "8 o'clock",
    "10 o'clock",
];

type ChangedFns = Rc<RefCell<Vec<Box<dyn Fn()>>>>;
/// The editor page's "draw it all again from the blob", handed to every control that edits.
type Rebuild = Rc<RefCell<Option<Rc<dyn Fn()>>>>;

/// The editor's state, shared by the row, the subpages and the dialog's close handler.
#[derive(Clone)]
struct Shared {
    dialog: glib::WeakRef<adw::PreferencesDialog>,
    blob: Rc<RefCell<String>>,
    changed: ChangedFns,
    row: adw::ActionRow,
}

impl Shared {
    fn cfg(&self) -> OverlayConfig {
        OverlayConfig::parse(&self.blob.borrow(), RingPlatform::Desktop)
    }

    /// A change from the editor: the blob, the row's summary, and whoever asked to know.
    fn write(&self, cfg: &OverlayConfig) {
        *self.blob.borrow_mut() = cfg.to_json();
        self.row.set_subtitle(&summary(cfg));
        for f in self.changed.borrow().iter() {
            f();
        }
    }

    fn dialog(&self) -> Option<adw::PreferencesDialog> {
        self.dialog.upgrade()
    }
}

/// The row's subtitle: the six buttons in ring order, so the list already says what is
/// configured.
fn summary(cfg: &OverlayConfig) -> String {
    cfg.ring
        .iter()
        .map(|slot| {
            slot.as_ref()
                .map_or("·".to_string(), |s| short_label(cfg, s))
        })
        .collect::<Vec<_>>()
        .join("  ")
}

/// The word a disc carries — the console ring's short labels, so the two desktop rings read
/// alike. A shortcut carries its chord.
fn short_label(cfg: &OverlayConfig, slot: &SlotId) -> String {
    match slot {
        SlotId::EndStream => "End".into(),
        SlotId::EndGame => "Quit".into(),
        SlotId::DisconnectLinger => "Leave".into(),
        SlotId::TouchMode => "Touch".into(),
        SlotId::Keyboard => "Keys".into(),
        SlotId::Stats => "Stats".into(),
        SlotId::Mic => "Mic".into(),
        SlotId::Pad => "Pad".into(),
        SlotId::SendText => "Text".into(),
        SlotId::Guide => "Guide".into(),
        SlotId::Qam => "QAM".into(),
        SlotId::PadMouse => "Mouse".into(),
        SlotId::StreamMute => "Mute".into(),
        SlotId::Host(_) => "Power".into(),
        SlotId::Shortcut(id) => cfg
            .shortcut(id)
            .map(|s| chord_chip(&s.keys))
            .unwrap_or_default(),
    }
}

/// A slot's full name and its availability note on this shell, from the shared catalogue.
fn describe(cfg: &OverlayConfig, slot: &SlotId) -> (String, String) {
    let id = slot.id();
    catalogue(cfg, RingPlatform::Desktop)
        .into_iter()
        .flat_map(|g| g.entries)
        .find(|e| e.id == id)
        .map(|e| (e.label, e.note))
        .unwrap_or((id, String::new()))
}

/// Our name for a GDK key, or `None` for one the wire does not carry.
fn key_name(key: gdk::Key) -> Option<&'static str> {
    use gdk::Key as K;
    Some(match key.to_lower() {
        K::Escape => "escape",
        K::F1 => "f1",
        K::F2 => "f2",
        K::F3 => "f3",
        K::F4 => "f4",
        K::F5 => "f5",
        K::F6 => "f6",
        K::F7 => "f7",
        K::F8 => "f8",
        K::F9 => "f9",
        K::F10 => "f10",
        K::F11 => "f11",
        K::F12 => "f12",
        K::Tab | K::ISO_Left_Tab => "tab",
        K::space => "space",
        K::Return | K::KP_Enter => "enter",
        K::BackSpace => "backspace",
        K::Delete | K::KP_Delete => "delete",
        K::Insert | K::KP_Insert => "insert",
        K::Home | K::KP_Home => "home",
        K::End | K::KP_End => "end",
        K::Page_Up | K::KP_Page_Up => "pageup",
        K::Page_Down | K::KP_Page_Down => "pagedown",
        K::Up | K::KP_Up => "up",
        K::Down | K::KP_Down => "down",
        K::Left | K::KP_Left => "left",
        K::Right | K::KP_Right => "right",
        K::Print => "printscreen",
        K::Pause => "pause",
        K::Caps_Lock => "capslock",
        k => {
            let c = k.to_unicode()?;
            return KEY_GRID
                .iter()
                .flat_map(|row| row.iter())
                .find(|n| n.len() == 1 && n.starts_with(c))
                .copied();
        }
    })
}

/// The name of the key at this physical position with NOTHING held, for a chord whose
/// shifted symbol the grid cannot name — `Shift+1` arrives as `!` on a US layout, and the
/// whole chord used to be dropped instead of read as Shift plus `1`.
fn unshifted_key_name(keycode: u32) -> Option<&'static str> {
    let entries = gdk::Display::default()?.map_keycode(keycode)?;
    entries
        .iter()
        .find(|(k, _)| k.group() == 0 && k.level() == 0)
        .and_then(|(_, keyval)| key_name(*keyval))
}

/// The modifiers held with a key, as the chord names them.
fn held_modifiers(state: gdk::ModifierType) -> Vec<String> {
    let mut v = Vec::new();
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        v.push("ctrl".into());
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        v.push("alt".into());
    }
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        v.push("shift".into());
    }
    if state.contains(gdk::ModifierType::SUPER_MASK) {
        v.push("win".into());
    }
    v
}

/// The dialog's handle on the editor: the row that opens it, and the blob it edits.
#[derive(Clone)]
pub struct QuickActions {
    shared: Shared,
}

impl QuickActions {
    /// The row for the Input page, seeded with the scope's effective blob.
    pub fn new(dialog: &adw::PreferencesDialog, blob: &str) -> QuickActions {
        let row = adw::ActionRow::builder()
            .title(super::spec::QUICK_ACTIONS.title)
            .use_markup(false)
            .activatable(true)
            .build();
        row.add_suffix(&crate::widgets::lucide::row_icon("chevron-right"));
        let shared = Shared {
            dialog: dialog.downgrade(),
            blob: Rc::new(RefCell::new(blob.to_string())),
            changed: Rc::new(RefCell::new(Vec::new())),
            row: row.clone(),
        };
        row.set_subtitle(&summary(&shared.cfg()));
        {
            let shared = shared.clone();
            row.connect_activated(move |_| {
                if let Some(dialog) = shared.dialog() {
                    dialog.push_subpage(&editor_page(&shared));
                }
            });
        }
        QuickActions { shared }
    }

    pub fn row(&self) -> &adw::ActionRow {
        &self.shared.row
    }

    /// The blob as edited so far.
    pub fn blob(&self) -> Rc<RefCell<String>> {
        self.shared.blob.clone()
    }

    /// Put another blob in place (a preset's reset back to the inherited ring). The next
    /// open of the editor shows it; the row says so at once.
    pub fn set_blob(&self, blob: &str) {
        *self.shared.blob.borrow_mut() = blob.to_string();
        self.shared.row.set_subtitle(&summary(&self.shared.cfg()));
    }

    /// Fires on every edit the user makes — the preset scope's "now overridden" hook.
    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.shared.changed.borrow_mut().push(Box::new(f));
    }
}

/// The subpage: the ring on its stage, the caption, the shortcuts, the reset.
fn editor_page(shared: &Shared) -> adw::NavigationPage {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();
    let caption = gtk::Label::builder()
        .label(
            "Click a button to change it, or drag one onto another to swap the two. \
             Ctrl+Alt+Shift+O, a two-finger twist or Select+A opens this dial in a stream.",
        )
        .wrap(true)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    content.append(&caption);

    let stage = gtk::Fixed::builder()
        .width_request(STAGE_W)
        .height_request(STAGE_H)
        .halign(gtk::Align::Center)
        .css_classes(["pf-ring-stage"])
        .build();
    content.append(&stage);
    let hint = gtk::Label::builder()
        .label("")
        .css_classes(["dim-label"])
        .build();
    content.append(&hint);
    let shortcuts = adw::PreferencesGroup::builder().title("Shortcuts").build();
    content.append(&shortcuts);
    let reset_group = adw::PreferencesGroup::builder().build();
    content.append(&reset_group);

    // Everything the ring and the list draw is rebuilt from the blob after every edit, so the
    // preview is never a stale copy of the setting.
    let rebuild: Rebuild = Rc::default();
    {
        let (shared, stage, hint, shortcuts) = (
            shared.clone(),
            stage.clone(),
            hint.clone(),
            shortcuts.clone(),
        );
        let rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
        let rebuild_ref = rebuild.clone();
        let f: Rc<dyn Fn()> = Rc::new(move || {
            let cfg = shared.cfg();
            let again = rebuild_ref.borrow().clone();
            build_ring(&stage, &hint, &shared, &cfg, again.clone());
            build_shortcuts(&shortcuts, &rows, &shared, &cfg, again);
        });
        *rebuild.borrow_mut() = Some(f.clone());
        f();
    }

    let reset = gtk::Button::builder()
        .label("Reset to default")
        .halign(gtk::Align::Start)
        .css_classes(["destructive-action"])
        .build();
    {
        let (shared, content, rebuild) = (shared.clone(), content.clone(), rebuild.clone());
        reset.connect_clicked(move |_| {
            let alert = adw::AlertDialog::new(
                Some("Reset quick actions?"),
                Some("The dial goes back to the platform default and the shortcuts are removed."),
            );
            alert.add_responses(&[("cancel", "Cancel"), ("reset", "Reset")]);
            alert.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
            alert.set_default_response(Some("cancel"));
            let (shared, rebuild) = (shared.clone(), rebuild.clone());
            alert.connect_response(None, move |_, response| {
                if response != "reset" {
                    return;
                }
                // The platform default is the EMPTY blob, like every other client's reset.
                *shared.blob.borrow_mut() = String::new();
                shared.write(&shared.cfg());
                *shared.blob.borrow_mut() = String::new();
                if let Some(f) = rebuild.borrow().as_ref() {
                    f();
                }
            });
            alert.present(Some(&content));
        });
    }
    reset_group.add(&reset);

    let clamp = adw::Clamp::builder()
        .child(&content)
        .maximum_size(680)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&scroll));
    adw::NavigationPage::new(&view, "Quick actions")
}

/// A disc's face: the slot's Lucide mark, or a stacked keycap for a shortcut (the modifiers
/// small on top, the key large under them). The mark comes from the SHARED slot table, so a
/// disc here carries exactly what the in-stream ring draws for the same slot. Only an id the
/// table cannot know — an unknown host action, a future slot — falls back to its short word,
/// which is what the console does too.
fn disc_face(cfg: &OverlayConfig, slot: Option<&SlotId>) -> gtk::Widget {
    let Some(slot) = slot else {
        return crate::widgets::lucide::icon("plus", 22).upcast();
    };
    if let SlotId::Shortcut(id) = slot {
        let keys = cfg.shortcut(id).map(|s| s.keys.clone()).unwrap_or_default();
        return keycap(&keys).upcast();
    }
    if let Some(name) = slot_icon(&slot.id(), "") {
        return crate::widgets::lucide::icon(name, ICON_PX).upcast();
    }
    gtk::Label::builder()
        .label(short_label(cfg, slot))
        .css_classes(["pf-ring-word"])
        .build()
        .upcast()
}

/// A chord as a stacked keycap: `Ctrl Shift` small over `Esc` large.
fn keycap(keys: &[String]) -> gtk::Box {
    // Expands, so that centring it means anything. Every holder this goes into is a fixed-size
    // disc wider than the chord — a `gtk::Box` is horizontal by default and packs one child at
    // the start, so without this the text sits on the disc's left edge whatever its alignment
    // says. With it the chord takes the whole disc and its own centre alignment lands it.
    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .build();
    let (mods, key) = match keys.split_last() {
        Some((key, mods)) => (
            mods.iter()
                .map(|k| key_legend(k))
                .collect::<Vec<_>>()
                .join(" "),
            key_legend(key),
        ),
        None => (String::new(), String::new()),
    };
    if !mods.is_empty() {
        b.append(
            &gtk::Label::builder()
                .label(mods)
                .css_classes(["pf-keycap-mods"])
                .build(),
        );
    }
    b.append(
        &gtk::Label::builder()
            .label(key)
            .css_classes(["pf-keycap-key"])
            .build(),
    );
    b
}

/// The ring itself: six discs on the ring's geometry, the inert centre, the hover label.
fn build_ring(
    stage: &gtk::Fixed,
    hint: &gtk::Label,
    shared: &Shared,
    cfg: &OverlayConfig,
    rebuild: Option<Rc<dyn Fn()>>,
) {
    while let Some(child) = stage.first_child() {
        stage.remove(&child);
    }
    let (cx, cy) = (STAGE_W as f64 / 2.0, STAGE_H as f64 / 2.0);
    let disc = SLOT_DIAMETER as f64;

    // The centre: what the sheet opens from in-stream, not editable here — dimmed and inert.
    // The ring's own `more` mark, so the editor's centre is the centre people press.
    let centre = gtk::Box::builder()
        .css_classes(["pf-ring-centre"])
        .width_request(CENTRE_DIAMETER as i32)
        .height_request(CENTRE_DIAMETER as i32)
        .tooltip_text("More — the rest of the actions, in the stream")
        .build();
    // The mark has to EXPAND to be centred. A `gtk::Box` is horizontal by default and packs a
    // single child at the start, so a child narrower than the box's requested width sits on its
    // left edge however it aligns itself; expanding gives it the whole box to centre within.
    let centre_mark = crate::widgets::lucide::icon("ellipsis", ICON_PX);
    centre_mark.set_hexpand(true);
    centre_mark.set_vexpand(true);
    centre.append(&centre_mark);
    stage.put(
        &centre,
        cx - CENTRE_DIAMETER as f64 / 2.0,
        cy - CENTRE_DIAMETER as f64 / 2.0,
    );

    for (k, clock) in CLOCK.iter().enumerate() {
        let (dx, dy) = slot_offset(k, RING_RADIUS);
        let slot = cfg.ring[k].clone();
        let (label, note) = slot
            .as_ref()
            .map(|s| describe(cfg, s))
            .unwrap_or(("Empty".into(), String::new()));
        let button = gtk::Button::builder()
            .css_classes(["circular", "pf-ring-disc"])
            .width_request(disc as i32)
            .height_request(disc as i32)
            .build();
        if !note.is_empty() {
            button.add_css_class("pf-dim");
            button.set_tooltip_text(Some(&format!("{label} — {note}")));
        } else {
            button.set_tooltip_text(Some(&label));
        }
        button.set_child(Some(&disc_face(cfg, slot.as_ref())));
        button.update_property(&[gtk::accessible::Property::Label(&format!(
            "{clock}, {label}"
        ))]);
        stage.put(
            &button,
            cx + dx as f64 - disc / 2.0,
            cy + dy as f64 - disc / 2.0,
        );

        // The name under the ring while the pointer or the focus is on a disc.
        {
            let motion = gtk::EventControllerMotion::new();
            let (on_enter, l) = (hint.clone(), label.clone());
            motion.connect_enter(move |_, _, _| on_enter.set_text(&l));
            let on_leave = hint.clone();
            motion.connect_leave(move |_| on_leave.set_text(""));
            button.add_controller(motion);
            let focus = gtk::EventControllerFocus::new();
            let (on_focus, l) = (hint.clone(), label.clone());
            focus.connect_enter(move |_| on_focus.set_text(&l));
            button.add_controller(focus);
        }

        // A click picks; the picker also carries the keyboard's swap.
        {
            let (shared, rebuild) = (shared.clone(), rebuild.clone());
            button.connect_clicked(move |b| picker(b, k, &shared, rebuild.clone()));
        }

        // Drag a disc onto another to swap the two. The dragged slot travels as a `u32`.
        let drag = gtk::DragSource::builder()
            .actions(gdk::DragAction::MOVE)
            .build();
        drag.set_content(Some(&gdk::ContentProvider::for_value(
            &(k as u32).to_value(),
        )));
        // The controller's own widget, not a captured clone: the closure is owned by the
        // controller the button owns, so holding the button here is a cycle and the whole
        // page leaks on every rebuild.
        drag.connect_drag_begin(move |source, _| {
            let Some(w) = source.widget() else { return };
            source.set_icon(
                Some(&gtk::WidgetPaintable::new(Some(&w))),
                (disc / 2.0) as i32,
                (disc / 2.0) as i32,
            );
        });
        button.add_controller(drag);
        let target = gtk::DropTarget::new(u32::static_type(), gdk::DragAction::MOVE);
        {
            let (shared, rebuild) = (shared.clone(), rebuild.clone());
            target.connect_drop(move |_, value, _, _| {
                let Ok(from) = value.get::<u32>() else {
                    return false;
                };
                let from = from as usize;
                if from == k || from >= RING_SLOTS {
                    return false;
                }
                let mut cfg = shared.cfg();
                cfg.ring.swap(from, k);
                shared.write(&cfg);
                if let Some(f) = &rebuild {
                    f();
                }
                true
            });
        }
        button.add_controller(target);
    }
}

/// The picker over a slot: the catalogue by group with each entry's note and the current
/// pick marked, then the swap targets for a keyboard.
fn picker(button: &gtk::Button, k: usize, shared: &Shared, rebuild: Option<Rc<dyn Fn()>>) {
    let cfg = shared.cfg();
    let current = cfg.ring[k].as_ref().map(SlotId::id).unwrap_or_default();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let popover = gtk::Popover::builder().build();
    let header = |title: &str| {
        gtk::ListBoxRow::builder()
            .activatable(false)
            .selectable(false)
            .child(
                &gtk::Label::builder()
                    .label(title)
                    .xalign(0.0)
                    .css_classes(["heading", "dim-label"])
                    .margin_top(8)
                    .margin_start(8)
                    .build(),
            )
            .build()
    };
    for group in catalogue(&cfg, RingPlatform::Desktop) {
        list.append(&header(group.title));
        for entry in group.entries {
            let row = adw::ActionRow::builder()
                .title(&entry.label)
                .use_markup(false)
                .activatable(true)
                .build();
            if !entry.note.is_empty() {
                row.set_subtitle(&entry.note);
            }
            if entry.id == current {
                row.add_suffix(&crate::widgets::lucide::row_icon("check"));
            }
            let (shared, rebuild, popover, id) =
                (shared.clone(), rebuild.clone(), popover.clone(), entry.id);
            row.connect_activated(move |_| {
                let mut cfg = shared.cfg();
                cfg.ring[k] = SlotId::parse(&id);
                shared.write(&cfg);
                popover.popdown();
                if let Some(f) = &rebuild {
                    f();
                }
            });
            list.append(&row);
        }
    }
    // `min_content_width` is ignored here: a ScrolledWindow with hscroll `Never` propagates its
    // child's minimum, and the ListBox's wrapping titles bottom out at one word. Only
    // `set_size_request` is a minimum GTK cannot ignore; `max_content_width` caps how far the
    // natural width lets one long label stretch the popover.
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .propagate_natural_width(true)
        .max_content_height(420)
        .max_content_width(560)
        .child(&list)
        .build();
    scroll.set_size_request(380, -1);
    popover.set_child(Some(&scroll));
    popover.set_parent(button);
    // A popover must leave its parent once closed, or GTK complains when the disc goes; the
    // rebuild that follows a pick drops the disc, so unparent a beat later.
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    popover.popup();
}

/// The shortcuts under the ring: a row each, Add in the group's header.
fn build_shortcuts(
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    shared: &Shared,
    cfg: &OverlayConfig,
    rebuild: Option<Rc<dyn Fn()>>,
) {
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    if group.header_suffix().is_none() {
        let add = gtk::Button::builder()
            .label("Add shortcut")
            .css_classes(["flat"])
            .build();
        let (shared, rebuild) = (shared.clone(), rebuild.clone());
        add.connect_clicked(move |_| {
            if let Some(dialog) = shared.dialog() {
                dialog.push_subpage(&shortcut_page(&shared, None, rebuild.clone()));
            }
        });
        group.set_header_suffix(Some(&add));
    }
    if cfg.shortcuts.is_empty() {
        group.set_description(Some("A new shortcut takes the first empty dial slot."));
    } else {
        group.set_description(None);
    }
    for sc in &cfg.shortcuts {
        let chip = chord_chip(&sc.keys);
        let row = adw::ActionRow::builder()
            .title(if sc.label.is_empty() {
                chip.clone()
            } else {
                sc.label.clone()
            })
            .subtitle(if sc.label.is_empty() {
                ""
            } else {
                chip.as_str()
            })
            .use_markup(false)
            .activatable(true)
            .build();
        let face = keycap(&sc.keys);
        face.add_css_class("pf-ring-disc");
        face.add_css_class("pf-keycap-small");
        face.set_size_request(36, 36);
        row.add_prefix(&face);
        row.add_suffix(&crate::widgets::lucide::row_icon("chevron-right"));
        let (shared, rebuild, sc) = (shared.clone(), rebuild.clone(), sc.clone());
        row.connect_activated(move |_| {
            if let Some(dialog) = shared.dialog() {
                dialog.push_subpage(&shortcut_page(&shared, Some(sc.clone()), rebuild.clone()));
            }
        });
        group.add(&row);
        rows.borrow_mut().push(row);
    }
}

/// One shortcut being edited.
#[derive(Clone, Default)]
struct Draft {
    id: Option<String>,
    label: String,
    chord: Chord,
}

impl Draft {
    fn of(sc: &Shortcut) -> Draft {
        Draft {
            id: Some(sc.id.clone()),
            label: sc.label.clone(),
            chord: Chord::parse(&sc.keys),
        }
    }
}

/// The shortcut editor: the disc as the ring will draw it, the name, the modifiers as toggles,
/// the key on a keyboard-shaped grid or captured from the real keyboard, Save and Remove.
fn shortcut_page(
    shared: &Shared,
    existing: Option<Shortcut>,
    rebuild: Option<Rc<dyn Fn()>>,
) -> adw::NavigationPage {
    let draft = Rc::new(RefCell::new(
        existing.as_ref().map(Draft::of).unwrap_or_default(),
    ));
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();

    // The preview: the disc and its legend, live.
    let preview = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(16)
        .build();
    let face_holder = gtk::Box::builder()
        .css_classes(["pf-ring-disc"])
        .width_request(SLOT_DIAMETER as i32)
        .height_request(SLOT_DIAMETER as i32)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    let legend = gtk::Label::builder()
        .css_classes(["title-2"])
        .xalign(0.0)
        .build();
    let legend_note = gtk::Label::builder()
        .label("How the dial will draw it")
        .css_classes(["dim-label"])
        .xalign(0.0)
        .build();
    let legend_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .build();
    legend_box.append(&legend);
    legend_box.append(&legend_note);
    preview.append(&face_holder);
    preview.append(&legend_box);
    content.append(&preview);

    let refresh_preview: Rc<dyn Fn()> = {
        let (draft, face_holder, legend) = (draft.clone(), face_holder.clone(), legend.clone());
        Rc::new(move || {
            while let Some(c) = face_holder.first_child() {
                face_holder.remove(&c);
            }
            let keys = draft.borrow().chord.keys();
            face_holder.append(&keycap(&keys));
            let chip = chord_chip(&keys);
            legend.set_text(if chip.is_empty() { "Pick a key" } else { &chip });
        })
    };
    refresh_preview();

    // The name.
    let name_group = adw::PreferencesGroup::new();
    let name = adw::EntryRow::builder()
        .title("Name")
        .text(&draft.borrow().label)
        .build();
    {
        let draft = draft.clone();
        name.connect_changed(move |e| draft.borrow_mut().label = e.text().to_string());
    }
    name_group.add(&name);
    content.append(&name_group);

    // The modifiers as toggles, and "press the chord" for a real keyboard.
    let mods_group = adw::PreferencesGroup::builder()
        .title("Chord")
        .description("Hold the modifiers marked on, then the key.")
        .build();
    let mods_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .build();
    let mut mod_buttons = Vec::new();
    for (i, m) in CHORD_MODIFIERS.iter().enumerate() {
        let b = gtk::ToggleButton::builder()
            .label(key_legend(m))
            .active(draft.borrow().chord.mods[i])
            .build();
        let (draft, refresh) = (draft.clone(), refresh_preview.clone());
        b.connect_toggled(move |b| {
            draft.borrow_mut().chord.mods[i] = b.is_active();
            refresh();
        });
        mods_row.append(&b);
        mod_buttons.push(b);
    }
    let capture = gtk::ToggleButton::builder()
        .label("Press the chord…")
        .tooltip_text("Then press the keys on your keyboard")
        .css_classes(["suggested-action"])
        .margin_start(12)
        .build();
    mods_row.append(&capture);
    mods_group.add(&mods_row);
    content.append(&mods_group);

    // The key grid: one group of toggles, so exactly one key is on.
    let grid_group = adw::PreferencesGroup::builder().title("Key").build();
    let grid = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .build();
    let mut first: Option<gtk::ToggleButton> = None;
    let key_buttons: Rc<RefCell<Vec<(&'static str, gtk::ToggleButton)>>> = Rc::default();
    for row in KEY_GRID {
        let line = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .halign(gtk::Align::Center)
            .build();
        for name in row {
            let b = gtk::ToggleButton::builder()
                .label(key_legend(name))
                .css_classes(["pf-key"])
                .active(draft.borrow().chord.key.as_deref() == Some(*name))
                .build();
            if let Some(f) = &first {
                b.set_group(Some(f));
            } else {
                first = Some(b.clone());
            }
            let (draft, refresh) = (draft.clone(), refresh_preview.clone());
            b.connect_toggled(move |b| {
                if b.is_active() {
                    draft.borrow_mut().chord.key = Some(name.to_string());
                    refresh();
                }
            });
            line.append(&b);
            key_buttons.borrow_mut().push((name, b));
        }
        grid.append(&line);
    }
    grid_group.add(&grid);
    content.append(&grid_group);

    // Pressing the chord on the real keyboard fills the modifiers and the key in one go.
    {
        let key = gtk::EventControllerKey::new();
        key.set_propagation_phase(gtk::PropagationPhase::Capture);
        let (capture, draft, refresh, mod_buttons, key_buttons) = (
            capture.clone(),
            draft.clone(),
            refresh_preview.clone(),
            mod_buttons.clone(),
            key_buttons.clone(),
        );
        key.connect_key_pressed(move |_, keyval, keycode, state| {
            if !capture.is_active() {
                return glib::Propagation::Proceed;
            }
            let Some(name) = key_name(keyval).or_else(|| unshifted_key_name(keycode)) else {
                // A lone modifier: keep waiting for the key.
                return glib::Propagation::Stop;
            };
            let mods = held_modifiers(state);
            {
                let mut d = draft.borrow_mut();
                for (i, m) in CHORD_MODIFIERS.iter().enumerate() {
                    d.chord.mods[i] = mods.iter().any(|x| x == m);
                }
                d.chord.key = Some(name.to_string());
            }
            // Copy out first: `set_active` emits `toggled` synchronously, and that handler
            // takes the draft mutably — a borrow still live inside the call panics.
            let mods = draft.borrow().chord.mods;
            for (i, b) in mod_buttons.iter().enumerate() {
                b.set_active(mods[i]);
            }
            for (n, b) in key_buttons.borrow().iter() {
                if *n == name {
                    b.set_active(true);
                }
            }
            capture.set_active(false);
            refresh();
            glib::Propagation::Stop
        });
        content.add_controller(key);
    }

    // Save, and Remove for an existing one.
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    let save = gtk::Button::builder()
        .label(if existing.is_some() {
            "Save"
        } else {
            "Add shortcut"
        })
        .css_classes(["suggested-action"])
        .build();
    {
        let (shared, draft, rebuild, content) = (
            shared.clone(),
            draft.clone(),
            rebuild.clone(),
            content.clone(),
        );
        save.connect_clicked(move |_| {
            let d = draft.borrow().clone();
            if d.chord.key.is_none() {
                let toast = adw::AlertDialog::new(Some("Pick a key first"), None);
                toast.add_responses(&[("ok", "OK")]);
                toast.present(Some(&content));
                return;
            }
            let mut cfg = shared.cfg();
            cfg.upsert_shortcut(d.id.as_deref(), &d.label, d.chord.keys());
            shared.write(&cfg);
            if let Some(f) = &rebuild {
                f();
            }
            if let Some(dialog) = shared.dialog() {
                dialog.pop_subpage();
            }
        });
    }
    actions.append(&save);
    if let Some(sc) = &existing {
        let remove = gtk::Button::builder()
            .label("Remove shortcut")
            .css_classes(["destructive-action"])
            .build();
        let (shared, rebuild, id) = (shared.clone(), rebuild.clone(), sc.id.clone());
        remove.connect_clicked(move |_| {
            let mut cfg = shared.cfg();
            cfg.remove_shortcut(&id);
            shared.write(&cfg);
            if let Some(f) = &rebuild {
                f();
            }
            if let Some(dialog) = shared.dialog() {
                dialog.pop_subpage();
            }
        });
        actions.append(&remove);
    }
    content.append(&actions);

    let clamp = adw::Clamp::builder()
        .child(&content)
        .maximum_size(720)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(&scroll));
    adw::NavigationPage::new(
        &view,
        if existing.is_some() {
            "Shortcut"
        } else {
            "New shortcut"
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every GDK key the grid lists maps to its own name, a letter maps whatever its case,
    /// and a key the wire does not carry maps to nothing.
    #[test]
    fn gdk_keys_map_to_the_chord_names() {
        assert_eq!(key_name(gdk::Key::Escape), Some("escape"));
        assert_eq!(key_name(gdk::Key::F12), Some("f12"));
        assert_eq!(key_name(gdk::Key::a), Some("a"));
        assert_eq!(key_name(gdk::Key::A), Some("a"));
        assert_eq!(key_name(gdk::Key::_7), Some("7"));
        assert_eq!(key_name(gdk::Key::Page_Down), Some("pagedown"));
        assert_eq!(key_name(gdk::Key::KP_Enter), Some("enter"));
        assert_eq!(
            key_name(gdk::Key::Control_L),
            None,
            "a modifier is not a key"
        );
        assert_eq!(key_name(gdk::Key::odiaeresis), None, "not on the wire");
        let mods = held_modifiers(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK);
        assert_eq!(mods, vec!["ctrl", "shift"]);
    }

    /// The row's summary is the ring in order, a dot for an empty slot, the chord for a
    /// shortcut — and the draft reads a shortcut back the way the editor wrote it.
    #[test]
    fn the_summary_and_the_draft_read_the_blob_back() {
        let blob = r#"{"v":2,"ring":["end_stream",null,"shortcut:s1",null,null,null],"shortcuts":[{"id":"s1","label":"Tasks","keys":["ctrl","shift","escape"]}]}"#;
        let cfg = OverlayConfig::parse(blob, RingPlatform::Desktop);
        assert_eq!(summary(&cfg), "End  ·  Ctrl+Shift+Esc  ·  ·  ·");
        let d = Draft::of(&cfg.shortcuts[0]);
        assert_eq!(d.chord.mods, [true, false, true, false]);
        assert_eq!(d.chord.key.as_deref(), Some("escape"));
        assert_eq!(d.chord.keys(), vec!["ctrl", "shift", "escape"]);
        let (label, note) = describe(&cfg, &SlotId::Pad);
        assert_eq!(
            (label.as_str(), note.as_str()),
            ("Virtual controller", "Phones and tablets only")
        );
    }
}
