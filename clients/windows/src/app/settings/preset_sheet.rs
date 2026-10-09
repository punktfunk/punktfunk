//! The preset UI: the scope bar, the Edit-preset sheet and the delete confirmation.

use crate::app::lucide;
use crate::app::style::*;
use crate::trust::KnownHosts;
use pf_client_core::presets::{PresetsFile, StreamPreset};
use windows_reactor::*;

/// The chip palette a preset can carry (`StreamPreset.accent`), same set as the GTK client so
/// a preset looks the same on both. Eight legible colours rather than a free picker: the job is
/// telling presets apart at a glance on a host tile, and the schema still accepts any
/// `#RRGGBB` a hand-edit writes.
const SWATCHES: &[(&str, &str)] = &[
    ("", "None"),
    ("#e01b24", "Red"),
    ("#ff7800", "Orange"),
    ("#f6d32d", "Yellow"),
    ("#33d17a", "Green"),
    ("#3584e4", "Blue"),
    ("#9141ac", "Purple"),
    ("#d16d9e", "Pink"),
    ("#77767b", "Slate"),
];

/// `#RRGGBB` to a brush colour. Anything else is refused rather than guessed at — the value is
/// user data and reaches the renderer.
pub(crate) fn hex_color(hex: &str) -> Option<Color> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(Color {
        a: 255,
        r: u8::from_str_radix(&h[0..2], 16).ok()?,
        g: u8::from_str_radix(&h[2..4], 16).ok()?,
        b: u8::from_str_radix(&h[4..6], 16).ok()?,
    })
}

/// The colour row: one tappable swatch per palette entry, the current one ringed.
fn colour_swatches(preset: &StreamPreset, rev: u64, set_rev: &AsyncSetState<u64>) -> Element {
    let current = preset.accent.clone().unwrap_or_default();
    let mut row: Vec<Element> = vec![text_block("Colour")
        .font_size(12.0)
        .foreground(ThemeRef::SecondaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .margin(edges(0.0, 0.0, 6.0, 0.0))
        .into()];
    for (hex, name) in SWATCHES {
        let selected = current == *hex;
        // "None" (and anything unparsable) draws as a faint neutral disc, so the row still
        // reads as a palette with a clear "no colour" end.
        let fill = hex_color(hex).unwrap_or(Color {
            a: 40,
            r: 128,
            g: 128,
            b: 128,
        });
        let (id, set_rev, hex_owned) = (preset.id.clone(), set_rev.clone(), hex.to_string());
        row.push(
            // Size on the BORDER itself: sized only via its child, the border gets squeezed
            // by the sheet's layout and the discs render as squashed ovals.
            border(vstack(Vec::<Element>::new()))
                .width(20.0)
                .height(20.0)
                .background(fill)
                .corner_radius(10.0)
                .border_brush(if selected {
                    ThemeRef::Accent
                } else {
                    ThemeRef::CardStroke
                })
                .border_thickness(uniform(if selected { 2.0 } else { 1.0 }))
                .tooltip(*name)
                .on_tapped(move || {
                    let mut catalog = PresetsFile::load();
                    if let Some(p) = catalog.presets.iter_mut().find(|p| p.id == id) {
                        p.accent = (!hex_owned.is_empty()).then(|| hex_owned.clone());
                        if let Err(e) = catalog.save() {
                            tracing::warn!(error = %format!("{e:#}"), "saving the preset colour");
                        }
                    }
                    set_rev.call(rev + 1);
                })
                .into(),
        );
    }
    hstack(row).spacing(8.0).into()
}

/// The Edit-preset modal: a scrim + centered card, the same in-tree overlay the Add-host
/// modal uses (ContentDialog is text-only in windows-reactor — no room for a text field or
/// the swatch row). Every control in it commits in place, exactly like the settings rows, so
/// the modal needs no draft state and Close is the only way out — there is nothing to cancel.
/// The one deferred repaint is the preset NAME: renaming commits as you type but the pane's
/// scope dropdown refreshes on Close (one revision bump), so the ComboBox is not remounted
/// under the user mid-keystroke.
pub(super) fn edit_preset_modal(
    preset: Option<&StreamPreset>,
    switcher: Option<ComboBox>,
    set_scope: &AsyncSetState<String>,
    set_delete: &AsyncSetState<Option<String>>,
    set_edit: &AsyncSetState<bool>,
    rev: u64,
    set_rev: &AsyncSetState<u64>,
) -> Element {
    let mut rows: Vec<Element> = vec![text_block(if switcher.is_some() {
        "Presets"
    } else {
        "Edit preset"
    })
    .font_size(20.0)
    .bold()
    .into()];
    if let Some(sw) = switcher {
        // Keyed by scope: an in-sheet scope switch re-renders this combo with a different
        // selection, and the in-place diff would leave it blank (the documented
        // items/selected_index hazard) — a remount applies every prop.
        rows.push(
            vstack(vec![Element::from(sw)])
                .with_key(format!(
                    "sheet-scope-{}",
                    preset.map(|p| p.id.as_str()).unwrap_or("")
                ))
                .into(),
        );
    }
    if let Some(preset) = preset {
        let id = preset.id.clone();
        let name_box = {
            let id = id.clone();
            text_box(&preset.name)
                .header("Name")
                .placeholder_text("Preset name")
                .on_text_changed(move |t: String| {
                    let name = t.trim().to_string();
                    if name.is_empty() {
                        return;
                    }
                    let mut catalog = PresetsFile::load();
                    // Names are unique case-insensitively — menus keyed by name are ambiguous
                    // otherwise. A collision simply doesn't commit; the box keeps what was typed.
                    if catalog.name_taken(&name, Some(&id)) {
                        return;
                    }
                    if let Some(p) = catalog.presets.iter_mut().find(|p| p.id == id) {
                        p.name = name;
                        let _ = catalog.save();
                    }
                })
        };
        rows.push(name_box.into());
        rows.push(colour_swatches(preset, rev, set_rev));
    }
    rows.push(
        text_block(
            "A preset overrides only what you change while it is selected; everything \
             else follows Default settings. Renaming applies as you type. Deleting leaves \
             hosts that used it on Default settings.",
        )
        .font_size(12.0)
        .wrap()
        .foreground(ThemeRef::SecondaryText)
        .into(),
    );
    let mut buttons: Vec<Element> = Vec::new();
    if let Some(p) = preset {
        let id = p.id.clone();
        buttons.push(
            {
                let (id, set_scope) = (id.clone(), set_scope.clone());
                button("Duplicate")
                    .icon(lucide::icon("copy"))
                    .on_click(move || {
                        let mut catalog = PresetsFile::load();
                        let Some(new_id) = catalog.duplicate(&id) else {
                            return;
                        };
                        if catalog.save().is_ok() {
                            // The sheet stays open and now edits the copy — scope follows it.
                            set_scope.call(new_id);
                        }
                    })
            }
            .into(),
        );
        buttons.push(
            {
                let set_delete = set_delete.clone();
                button("Delete\u{2026}")
                    .icon(lucide::icon("trash-2"))
                    .on_click(move || set_delete.call(Some(id.clone())))
            }
            .into(),
        );
    }
    // "Save", not "Close": every field in the sheet commits as you type, so this is really
    // "done" — but the review is right that a sheet full of edits wants a verb, and Save
    // is the promise the button already keeps.
    let close_sheet = {
        let (set_edit, set_rev) = (set_edit.clone(), set_rev.clone());
        move || {
            set_edit.call(false);
            // The deferred repaint: the bar dropdown (and any pinned tiles) pick up the
            // rename now, in one pass, instead of remounting per keystroke.
            set_rev.call(rev + 1);
        }
    };
    buttons.push(
        {
            let close_sheet = close_sheet.clone();
            button("Save")
                .accent()
                .icon(lucide::icon("save"))
                .on_click(close_sheet)
        }
        .into(),
    );
    rows.push(
        hstack(buttons)
            .spacing(8.0)
            .horizontal_alignment(HorizontalAlignment::Right)
            .margin(edges(0.0, 6.0, 0.0, 0.0))
            .into(),
    );
    // The content scrolls when the window is shorter than the sheet (same rule as the host
    // editor) — a sheet must never clip its own controls.
    // A tap INSIDE the card bubbles up to the scrim (WinUI bubbles `Tapped`; reactor can't
    // mark it handled), so the card raises this flag first and the scrim's handler swallows
    // exactly that tap — a tap on the scrim itself, and Escape, dismiss the sheet.
    let inside_tap = std::rc::Rc::new(std::cell::Cell::new(false));
    let modal = dialog_surface(scroll_view(vstack(rows).spacing(12.0)))
        .on_tapped({
            let inside_tap = inside_tap.clone();
            move || inside_tap.set(true)
        })
        .max_width(420.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .margin(uniform(24.0));
    let scrim_close = close_sheet.clone();
    let esc_close = close_sheet;
    Element::from(
        border(modal)
            .background(Color {
                a: 140,
                r: 0,
                g: 0,
                b: 0,
            })
            .on_tapped(move || {
                if inside_tap.replace(false) {
                    return;
                }
                scrim_close();
            }),
    )
    .keyboard_accelerator(KeyboardAccelerator::new(
        VirtualKey::Escape,
        VirtualKeyModifiers::None,
        esc_close,
    ))
}

/// The bar above the page: the preset's colour chip and one native DropDownButton that picks
/// the scope, creates a preset or opens the sheet.
pub(super) fn scope_bar(
    active: Option<&StreamPreset>,
    set_scope: &AsyncSetState<String>,
    set_edit: &AsyncSetState<bool>,
) -> Element {
    let catalog = PresetsFile::load();
    let scope_pairs: Vec<(String, String)> = catalog
        .presets
        .iter()
        .map(|p| (p.id.clone(), p.name.clone()))
        .collect();
    const SCOPE_DEFAULT: &str = "Default settings";
    const SCOPE_NEW: &str = "New preset\u{2026}";
    // The Edit entry's prefix — the suffix is the preset's display name.
    const SCOPE_EDIT: &str = "Edit \u{201c}";
    let scope_label = match active {
        Some(p) => p.name.clone(),
        None => SCOPE_DEFAULT.to_string(),
    };
    let switcher = {
        let (set_scope, set_edit) = (set_scope.clone(), set_edit.clone());
        let pairs = scope_pairs.clone();
        let mut items = vec![menu_item(SCOPE_DEFAULT)];
        for (_, name) in &pairs {
            items.push(menu_item(name.clone()));
        }
        items.push(menu_separator());
        items.push(menu_item(SCOPE_NEW));
        if let Some(p) = active {
            items.push(menu_item(format!("{SCOPE_EDIT}{}\u{201d}\u{2026}", p.name)));
        }
        drop_down_button(&scope_label)
            .menu_flyout(items)
            .on_item_clicked(move |item: String| {
                // Fixed entries first — a preset could share their text.
                if item == SCOPE_NEW {
                    // A new preset takes an auto-numbered name and lands straight in
                    // the sheet to be named — creation and naming are one gesture, and
                    // there is no half-created state a Cancel would have to unwind.
                    let mut catalog = PresetsFile::load();
                    let name = (1..)
                        .map(|n| format!("Preset {n}"))
                        .find(|n| !catalog.name_taken(n, None))
                        .unwrap_or_else(|| "Preset".to_string());
                    let preset = StreamPreset::new(name);
                    let new_id = preset.id.clone();
                    catalog.presets.push(preset);
                    if catalog.save().is_ok() {
                        set_scope.call(new_id);
                        set_edit.call(true);
                    }
                    return;
                }
                if item.starts_with(SCOPE_EDIT) {
                    set_edit.call(true);
                    return;
                }
                if item == SCOPE_DEFAULT {
                    set_scope.call(String::new());
                    return;
                }
                if let Some((id, _)) = pairs.iter().find(|(_, n)| n == &item) {
                    set_scope.call(id.clone());
                }
            })
    };
    let mut row: Vec<Element> = vec![text_block("Editing")
        .font_size(13.0)
        .foreground(ThemeRef::SecondaryText)
        .vertical_alignment(VerticalAlignment::Center)
        .into()];
    // The preset's colour, right where the choice is made (menu items are plain
    // strings in this toolkit, so the chip cannot ride inside the menu).
    if let Some(c) = active.and_then(|p| p.accent.as_deref()).and_then(hex_color) {
        row.push(
            border(vstack(Vec::<Element>::new()))
                .width(12.0)
                .height(12.0)
                .background(c)
                .corner_radius(6.0)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    row.push(Element::from(switcher).vertical_alignment(VerticalAlignment::Center));
    hstack(row)
        .spacing(12.0)
        .margin(edges(24.0, 12.0, 28.0, 8.0))
        .into()
}

/// The delete confirmation. Always mounted, with `is_open` doing the arming: a ContentDialog
/// is a phantom child in the reactor backend, and unmounting one destroys its handle before
/// `remove_child` runs, so the backend `RemoveAt()`s a visual child that does not exist
/// (E_BOUNDS, a main-thread panic on every delete). A mounted dialog is never removed.
pub(super) fn delete_dialog(
    delete_pending: &Option<String>,
    set_scope: &AsyncSetState<String>,
    set_delete: &AsyncSetState<Option<String>>,
    set_edit: &AsyncSetState<bool>,
) -> Element {
    let pending = delete_pending
        .as_ref()
        .and_then(|id| PresetsFile::load().find_by_id(id).cloned());
    // The warning counts what actually breaks: hosts that fall back to the defaults,
    // and pinned cards that disappear (design §6).
    let body = pending
        .as_ref()
        .map(|p| {
            let known = KnownHosts::load();
            let bound = known
                .hosts
                .iter()
                .filter(|h| h.preset_id.as_deref() == Some(p.id.as_str()))
                .count();
            let pinned = known
                .hosts
                .iter()
                .filter(|h| h.pinned_presets.iter().any(|x| x == &p.id))
                .count();
            let mut body = format!("\u{201c}{}\u{201d} will be removed.", p.name);
            if bound > 0 {
                body.push_str(&format!(
                    " {bound} host{} will fall back to Default settings.",
                    if bound == 1 { "" } else { "s" }
                ));
            }
            if pinned > 0 {
                body.push_str(&format!(
                    " {pinned} pinned card{} will disappear.",
                    if pinned == 1 { "" } else { "s" }
                ));
            }
            body
        })
        .unwrap_or_default();
    let (id, set_scope, set_delete, set_edit) = (
        pending.as_ref().map(|p| p.id.clone()),
        set_scope.clone(),
        set_delete.clone(),
        set_edit.clone(),
    );
    ContentDialog::new("Delete preset?")
        .content(body)
        .primary_button_text("Delete")
        .close_button_text("Cancel")
        .is_open(pending.is_some())
        .on_closed(move |r: ContentDialogResult| {
            set_delete.call(None);
            if r != ContentDialogResult::Primary {
                return;
            }
            let Some(id) = id.clone() else {
                return;
            };
            let mut catalog = PresetsFile::load();
            catalog.presets.retain(|p| p.id != id);
            // Bindings and pins are left dangling on purpose: they resolve as "no
            // preset" everywhere, and rewriting every host record here would be a
            // second, racier source of truth.
            if catalog.save().is_ok() {
                set_scope.call(String::new());
                // The preset the sheet was showing is gone — without this, the
                // still-armed flag would pop the sheet open on the NEXT preset pick.
                set_edit.call(false);
            }
        })
        .into()
}
