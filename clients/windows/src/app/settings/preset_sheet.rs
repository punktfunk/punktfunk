//! The preset UI: the scope bar, the Edit-preset sheet and the delete confirmation.

use super::{update_preset, Cx};
use crate::app::style::*;
use crate::app::{saved, AsyncSetState};
use crate::trust::KnownHosts;
use pf_client_core::presets::{PresetsFile, StreamPreset};
use pf_client_core::settings::Label;
use pf_l10n::{client, common, desktop};
use windows_reactor::*;

/// The chip palette a preset can carry (`StreamPreset.accent`), same set as the GTK client so
/// a preset looks the same on both. Eight legible colours rather than a free picker: the job is
/// telling presets apart at a glance on a host tile, and the schema still accepts any
/// `#RRGGBB` a hand-edit writes.
const SWATCHES: &[(&str, Label)] = &[
    ("", client::preset_color_none),
    ("#e01b24", client::preset_color_red),
    ("#ff7800", client::preset_color_orange),
    ("#f6d32d", desktop::desktop_preset_color_yellow),
    ("#33d17a", client::preset_color_green),
    ("#3584e4", client::preset_color_blue),
    ("#9141ac", desktop::desktop_preset_color_purple),
    ("#d16d9e", client::preset_color_pink),
    ("#77767b", desktop::desktop_preset_color_slate),
];

/// `#RRGGBB` to a brush colour. Anything else is refused rather than guessed at — the value is
/// user data and reaches the renderer.
pub(crate) fn hex_color(hex: &str) -> Option<Color> {
    let h = hex.strip_prefix('#')?;
    if h.len() != 6 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(Color::rgb(
        u8::from_str_radix(&h[0..2], 16).ok()?,
        u8::from_str_radix(&h[2..4], 16).ok()?,
        u8::from_str_radix(&h[4..6], 16).ok()?,
    ))
}

/// The colour row: one tappable swatch per palette entry, the current one ringed.
fn colour_swatches(cx: &Cx, preset: &StreamPreset) -> View {
    let current = preset.accent.clone().unwrap_or_default();
    let mut row: Vec<View> = vec![muted(cx.pal, client::preset_color())
        .font_size(12.0)
        .vertical_alignment(VerticalAlignment::Center)
        .margin(edges(0.0, 0.0, 6.0, 0.0))
        .into()];
    for (hex, name) in SWATCHES {
        let name = name();
        let selected = current == *hex;
        // "None" (and anything unparsable) draws as a faint neutral disc, so the row still
        // reads as a palette with a clear "no colour" end.
        let fill = hex_color(hex).unwrap_or(Color::argb(40, 128, 128, 128));
        let (id, hex_owned) = (preset.id.clone(), hex.to_string());
        let (rev, set_rev, set_status) = (cx.rev, cx.set_rev.clone(), cx.set_status.clone());
        let disc = blank(20.0, 20.0)
            .background(fill)
            .corner_radius(10.0)
            .border_brush(if selected {
                ThemeBrush::Accent
            } else {
                ThemeBrush::CardStroke
            })
            .border_thickness(uniform(if selected { 2.0 } else { 1.0 }));
        row.push(
            on_tap(disc, move || {
                let r = update_preset(&id, |p| {
                    p.accent = (!hex_owned.is_empty()).then(|| hex_owned.clone())
                });
                saved(r, &set_status, Some((rev, &set_rev)));
            })
            .tooltip(name),
        );
    }
    hstack(row).spacing(8.0).into()
}

/// Rename preset `id`. Names are unique case-insensitively, since menus keyed by name would be
/// ambiguous otherwise: a collision does not commit, and the box keeps what was typed.
fn rename(id: &str, name: String) -> std::result::Result<(), impl std::fmt::Display> {
    PresetsFile::update(|catalog| {
        if !catalog.name_taken(&name, Some(id))
            && let Some(p) = catalog.presets.iter_mut().find(|p| p.id == id)
        {
            p.name = name;
        }
    })
}

/// The Edit-preset [`sheet`]. Every control in it commits in place, exactly like the settings
/// rows, so the sheet needs no draft state and Save only closes it. The one deferred repaint
/// is the preset NAME: renaming commits as you type, but the bar's scope menu refreshes on
/// close (one revision bump), so it is not rebuilt under the user mid-keystroke.
pub(super) fn edit_preset_modal(
    cx: &Cx,
    preset: Option<&StreamPreset>,
    set_scope: &AsyncSetState<String>,
    set_delete: &AsyncSetState<Option<String>>,
    set_edit: &AsyncSetState<bool>,
) -> View {
    let (rev, set_rev, pal) = (cx.rev, cx.set_rev, cx.pal);
    let mut rows: Vec<View> = vec![text(client::preset_edit_title())
        .font_size(20.0)
        .font_weight(FontWeight::BOLD)
        .into()];
    if let Some(preset) = preset {
        let (id, set_status) = (preset.id.clone(), cx.set_status.clone());
        rows.push(
            TextBox::new()
                .text(&preset.name)
                .header(client::preset_name())
                .placeholder_text(client::preset_name_placeholder())
                .on_text_changed(move |t: std::rc::Rc<str>| {
                    let name = t.trim().to_string();
                    if name.is_empty() {
                        return;
                    }
                    // No revision bump: the bar picks the name up when the sheet closes.
                    saved(rename(&id, name), &set_status, None);
                })
                .into(),
        );
        rows.push(colour_swatches(cx, preset));
    }
    rows.push(
        muted(pal, desktop::desktop_preset_sheet_hint())
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap)
            .into(),
    );
    let mut buttons: Vec<View> = Vec::new();
    if let Some(p) = preset {
        let id = p.id.clone();
        let (dup_id, set_scope) = (id.clone(), set_scope.clone());
        let (set_rev, set_status) = (set_rev.clone(), cx.set_status.clone());
        buttons.push(
            icon_button("copy", common::common_duplicate())
                .on_click(move || {
                    let r = PresetsFile::update(|catalog| catalog.duplicate(&dup_id));
                    // The sheet stays open and now edits the copy — scope follows it.
                    if let Some(new_id) = saved(r, &set_status, Some((rev, &set_rev))).flatten() {
                        set_scope.call(new_id);
                    }
                })
                .into(),
        );
        let set_delete = set_delete.clone();
        buttons.push(
            icon_button("trash-2", client::preset_menu_delete())
                .on_click(move || set_delete.call(Some(id.clone())))
                .into(),
        );
    }
    // "Save", not "Close": every field commits as you type, and Save is the promise the
    // button already keeps.
    let close_sheet = {
        let (set_edit, set_rev) = (set_edit.clone(), set_rev.clone());
        move || {
            set_edit.call(false);
            // The deferred repaint: the bar menu (and any pinned tiles) pick up the rename
            // now, in one pass.
            set_rev.call(rev + 1);
        }
    };
    let save = close_sheet.clone();
    buttons.push(
        icon_button("save", common::common_save())
            .style(ButtonStyle::Accent)
            .on_click(save)
            .into(),
    );
    rows.push(
        hstack(buttons)
            .spacing(8.0)
            .horizontal_alignment(HorizontalAlignment::Right)
            .margin(edges(0.0, 6.0, 0.0, 0.0))
            .into(),
    );
    sheet(pal, vstack(rows).spacing(12.0), 420.0, 1.0, close_sheet)
}

/// What a row of the scope menu does.
#[derive(Clone)]
enum ScopePick {
    Default,
    Preset(String),
    /// An empty preset, named in the sheet.
    New,
    /// Core's Second screen starter.
    SecondScreen,
    Edit,
}

/// The bar above the page: the preset's colour chip and one native DropDownButton that picks
/// the scope, creates a preset from a starter (Empty, Second screen) or opens the sheet.
pub(super) fn scope_bar(
    cx: &Cx,
    active: Option<&StreamPreset>,
    set_scope: &AsyncSetState<String>,
    set_edit: &AsyncSetState<bool>,
) -> View {
    let catalog = PresetsFile::load();
    let mut picks = vec![ScopePick::Default];
    let mut items = vec![MenuItem::item("0", client::prefs_scope_defaults())];
    for p in &catalog.presets {
        items.push(MenuItem::item(picks.len().to_string(), p.name.as_str()));
        picks.push(ScopePick::Preset(p.id.clone()));
    }
    items.push(MenuItem::separator("sep"));
    let empty = MenuItem::item(picks.len().to_string(), client::preset_starter_empty());
    picks.push(ScopePick::New);
    let second = MenuItem::item(picks.len().to_string(), client::screen_role_second());
    picks.push(ScopePick::SecondScreen);
    items.push(MenuItem::submenu(
        "sub:new",
        client::preset_new_title(),
        [empty, second],
    ));
    if let Some(p) = active {
        let label = client::preset_menu_edit_named(&p.name);
        items.push(MenuItem::item(picks.len().to_string(), label));
        picks.push(ScopePick::Edit);
    }
    let scope_label = active.map_or(client::prefs_scope_defaults(), |p| p.name.as_str());
    let (set_scope, set_edit) = (set_scope.clone(), set_edit.clone());
    let (rev, set_rev, set_status) = (cx.rev, cx.set_rev.clone(), cx.set_status.clone());
    let menu = Menu::new(items, move |key: Key| {
        let pick = key
            .as_str()
            .and_then(|k| k.parse::<usize>().ok())
            .and_then(|i| picks.get(i));
        match pick {
            Some(ScopePick::Default) => set_scope.call(String::new()),
            Some(ScopePick::Preset(id)) => set_scope.call(id.clone()),
            Some(ScopePick::New) => {
                // A new preset takes an auto-numbered name and lands straight in the sheet to
                // be named — creation and naming are one gesture, and there is no
                // half-created state a Cancel would have to unwind.
                let r = PresetsFile::update(|catalog| {
                    let name = (1..)
                        .map(desktop::desktop_preset_auto_name)
                        .find(|n| !catalog.name_taken(n, None))
                        .unwrap_or_else(|| desktop::desktop_preset_auto_name_bare().to_string());
                    let preset = StreamPreset::new(name);
                    let new_id = preset.id.clone();
                    catalog.presets.push(preset);
                    new_id
                });
                if let Some(new_id) = saved(r, &set_status, Some((rev, &set_rev))) {
                    set_scope.call(new_id);
                    set_edit.call(true);
                }
            }
            // Named after its role, so it opens on its settings rather than the sheet.
            Some(ScopePick::SecondScreen) => {
                let r = PresetsFile::update(|catalog| {
                    let preset = StreamPreset::second_screen(|n| catalog.name_taken(n, None));
                    let new_id = preset.id.clone();
                    catalog.presets.push(preset);
                    new_id
                });
                if let Some(new_id) = saved(r, &set_status, Some((rev, &set_rev))) {
                    set_scope.call(new_id);
                }
            }
            Some(ScopePick::Edit) => set_edit.call(true),
            None => {}
        }
    });
    let switcher = DropDownButton::new()
        .content(scope_label)
        .vertical_alignment(VerticalAlignment::Center)
        .menu(menu);
    let mut row: Vec<View> = vec![muted(cx.pal, client::prefs_scope_editing())
        .font_size(13.0)
        .vertical_alignment(VerticalAlignment::Center)
        .into()];
    // The preset's colour, right where the choice is made (menu items are plain strings in
    // this toolkit, so the chip cannot ride inside the menu).
    if let Some(c) = active.and_then(|p| p.accent.as_deref()).and_then(hex_color) {
        row.push(
            blank(12.0, 12.0)
                .background(c)
                .corner_radius(6.0)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    row.push(switcher);
    hstack(row)
        .spacing(12.0)
        .margin(edges(24.0, 12.0, 28.0, 8.0))
        .into()
}

/// The delete confirmation, armed by `is_open`.
pub(super) fn delete_dialog(
    cx: &Cx,
    delete_pending: &Option<String>,
    set_scope: &AsyncSetState<String>,
    set_delete: &AsyncSetState<Option<String>>,
    set_edit: &AsyncSetState<bool>,
) -> ContentDialog {
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
            let mut body = desktop::desktop_preset_delete_body(&p.name);
            if bound > 0 {
                body.push(' ');
                body.push_str(&client::preset_delete_hosts(bound as u64));
            }
            if pinned > 0 {
                body.push(' ');
                body.push_str(&client::preset_delete_cards(pinned as u64));
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
    let (rev, set_rev, set_status) = (cx.rev, cx.set_rev.clone(), cx.set_status.clone());
    ContentDialog::new()
        .title(desktop::desktop_preset_delete_title())
        .content(body)
        .primary_button_text(common::common_delete())
        .close_button_text(common::common_cancel())
        .is_open(pending.is_some())
        .on_closed(move |r: ContentDialogResult| {
            set_delete.call(None);
            if r != ContentDialogResult::Primary {
                return;
            }
            let Some(id) = id.clone() else {
                return;
            };
            // Bindings and pins are left dangling on purpose: they resolve as "no
            // preset" everywhere, and rewriting every host record here would be a
            // second, racier source of truth.
            let r = PresetsFile::update(|catalog| catalog.presets.retain(|p| p.id != id));
            if saved(r, &set_status, Some((rev, &set_rev))).is_some() {
                set_scope.call(String::new());
                // The preset the sheet was showing is gone — without this, the
                // still-armed flag would pop the sheet open on the NEXT preset pick.
                set_edit.call(false);
            }
        })
}
