//! The settings screen. Every control writes straight back to the persisted [`Settings`]
//! (there is no Apply step), via the small [`setting_combo`]/[`setting_toggle`] builders.
//!
//! **Structure mirrors the Apple client's 2026-07 settings revamp** (its
//! `SettingsCategory` + `SettingsView+Sections.swift`), so the two desktop clients read the
//! same way: General = session/app behavior, Display = everything about the picture,
//! Input = touch/keyboard/mouse, Audio, Controllers, About. Each field carries its
//! explanation DIRECTLY under it ([`described`]) rather than only on hover — the same move
//! Apple made, for the same reason (guidance nobody hovers for is guidance nobody reads).
//! Wording is shared verbatim wherever the setting means the same thing on both platforms;
//! where the BEHAVIOR differs the text is deliberately Windows-specific (the forwarded-
//! controller picker especially: Apple forwards one pad, this client forwards them all).

mod about;
mod audio;
mod controllers;
mod display;
mod general;
mod input;
mod preset_sheet;
mod rows;

pub(crate) use preset_sheet::hex_color;

use super::lucide;
use super::saved;
use super::style::*;
use super::{AppCtx, AsyncSetState, Msg, Shell};
use crate::trust::Settings;
use pf_client_core::presets::{PresetsFile, SettingsOverlay, StreamPreset};
use pf_fluent::{Palette, Token};
use std::sync::Arc;
use windows_reactor::*;

/// Persist one control's edit into the layer being edited: the defaults, or the preset's
/// overrides. In preset scope the edit pins the control's `field` at its new value, also when
/// that equals the global. An event that moved nothing (a combo mounting, a list rebuilt in
/// place) pins nothing: this shell has no quiet flag around its own updates.
///
/// Every commit ends by bumping the revision: a preset-scope edit changes what the page
/// should SHOW (the row's Overridden marker, the catalog behind the controls) without
/// changing any state the page reads, so without the bump no render pass runs. Global edits
/// bump too, which also refreshes rows whose shown value derives from the field just written.
pub(super) fn commit(
    ctx: &Arc<AppCtx>,
    scope: &str,
    field: &str,
    rev: (u64, &AsyncSetState<u64>),
    set_status: &AsyncSetState<String>,
    edit: impl FnOnce(&mut Settings),
) {
    if scope.is_empty() {
        // Edit the file under its lock, never the snapshot: a spawned session and the console
        // write it too (match-window size, their own settings). The snapshot follows the
        // result, so every row renders what is on disk.
        let mut s = ctx.settings.lock().unwrap();
        *s = Settings::update(|file| {
            edit(file);
            true
        });
        rev.1.call(rev.0 + 1);
        return;
    }
    // The file, not the snapshot, here too: the preset's effective values derive from `base`,
    // and another process (session resize, console UI, Decky) may have moved a global.
    let base = {
        let mut s = ctx.settings.lock().unwrap();
        *s = Settings::load();
        s.clone()
    };
    let r = update_preset(scope, |p| {
        let before = p.overrides.apply(&base);
        let mut after = before.clone();
        edit(&mut after);
        if after != before {
            p.overrides.pin(field, &after);
        }
    });
    saved(r, set_status, Some(rev));
}

/// [`PresetsFile::update`] on the preset `id`. A preset deleted from under us is left alone:
/// the next render falls back to the defaults scope.
pub(crate) fn update_preset(
    id: &str,
    f: impl FnOnce(&mut StreamPreset),
) -> std::result::Result<(), impl std::fmt::Display> {
    PresetsFile::update(|catalog| {
        if let Some(p) = catalog.presets.iter_mut().find(|p| p.id == id) {
            f(p);
        }
    })
}

/// Re-base the process-lifetime settings snapshot on the file, and re-probe this device's
/// hardware — called from the navigation handlers that (re)enter this page, NOT per render
/// pass. `ctx.settings` is loaded once at process start and this process is not the file's
/// only writer (a spawned session persists its match-window size, the console UI and Decky
/// save too — presets.rs documents the family), so without this the page opens showing values
/// another process already replaced, which then visibly "jump" the moment a row is touched and
/// `commit`'s rebase pulls the file in.
pub(crate) fn refresh_snapshot(ctx: &Arc<AppCtx>) {
    *ctx.settings.lock().unwrap() = Settings::load();
    let (speakers, mics) = pf_client_core::audio::devices().unwrap_or_default();
    *ctx.probes.lock().unwrap() = DeviceProbes {
        gpus: crate::gpu::adapter_names(),
        speakers,
        mics,
        native: primary_mode(),
        link: punktfunk_core::transport::ifinfo::local_link_facts(None),
    };
}

/// The primary display's current mode in real pixels: what a Native stream asks for here.
/// `None` when Windows names none.
fn primary_mode() -> Option<punktfunk_core::Mode> {
    use windows::Win32::wingdi::DEVMODEW;
    use windows::Win32::winuser::{EnumDisplaySettingsW, ENUM_CURRENT_SETTINGS};
    // SAFETY: DEVMODEW is integers, arrays and unions of integers; all-zero is a valid value.
    let mut dm: DEVMODEW = unsafe { std::mem::zeroed() };
    dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    // SAFETY: `dm` is a local with `dmSize` set, the one field the call reads before filling it;
    // a null device name is the current display device.
    let ok = unsafe {
        EnumDisplaySettingsW(
            windows::core::PCWSTR::null(),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
        )
    };
    (ok.as_bool() && dm.dmPelsWidth > 0).then_some(punktfunk_core::Mode {
        width: dm.dmPelsWidth,
        height: dm.dmPelsHeight,
        // 0 and 1 are "the hardware's default".
        refresh_hz: if dm.dmDisplayFrequency > 1 {
            dm.dmDisplayFrequency
        } else {
            60
        },
    })
}

/// This device's pickable hardware: DXGI adapters and WASAPI endpoints, and the display and
/// link PyroWave quality prices against. Probed once per visit by [`refresh_snapshot`], never
/// on the render each settings commit triggers.
#[derive(Default)]
pub(crate) struct DeviceProbes {
    gpus: Vec<String>,
    speakers: Vec<pf_client_core::audio::AudioDevice>,
    mics: Vec<pf_client_core::audio::AudioDevice>,
    native: Option<punktfunk_core::Mode>,
    link: punktfunk_core::transport::LinkFacts,
}

/// The layer the settings screen is editing, resolved for display: `None` = the defaults.
pub(super) fn active_preset(scope: &str) -> Option<StreamPreset> {
    (!scope.is_empty())
        .then(|| PresetsFile::load().find_by_id(scope).cloned())
        .flatten()
}

// NOTE: the row builders no longer set the widget's own `.header` — the row label is
// rendered by [`described_overridable`]/[`described_labeled`], because the Overridden pill
// must sit BETWEEN the label and the input, and a widget-embedded header allows nothing
// between itself and its box.
fn setting_combo(
    cx: &Cx,
    scope: &str,
    field: &'static str,
    names: Vec<String>,
    current: usize,
    apply: impl Fn(&mut Settings, usize) + 'static,
) -> ComboBox {
    let max = names.len().saturating_sub(1);
    let write = committer(cx, scope, field, move |s, i: usize| apply(s, i.min(max)));
    ComboBox::new()
        .items_source(names)
        .selected_index(current)
        .on_selection_changed(move |i: Option<usize>| {
            // None is "nothing selected", which an in-place items rebuild raises: not a pick.
            if let Some(i) = i {
                write(i);
            }
        })
}

/// The labels of a `(value, label)` preset table, plus the index of `is_current`'s match.
fn presets<V>(table: &[(V, &str)], is_current: impl Fn(&V) -> bool) -> (Vec<String>, usize) {
    let names = table.iter().map(|(_, l)| l.to_string()).collect();
    let current = table.iter().position(|(v, _)| is_current(v)).unwrap_or(0);
    (names, current)
}

/// A `ToggleSwitch` bound to one boolean settings field (label rendered by the row — see
/// [`setting_combo`]'s note).
fn setting_toggle(
    cx: &Cx,
    scope: &str,
    field: &'static str,
    on: bool,
    apply: impl Fn(&mut Settings, bool) + 'static,
) -> ToggleSwitch {
    ToggleSwitch::new()
        .is_on(on)
        .on_content("On")
        .off_content("Off")
        .on_toggled(committer(cx, scope, field, apply))
}

/// What a row's change event runs: [`commit`] of `apply` into `scope`, pinning `field`.
fn committer<T: 'static>(
    cx: &Cx,
    scope: &str,
    field: &'static str,
    apply: impl Fn(&mut Settings, T) + 'static,
) -> impl Fn(T) + 'static {
    let (ctx, scope) = (cx.ctx.clone(), scope.to_string());
    let (rev, set_rev, set_status) = (cx.rev, cx.set_rev.clone(), cx.set_status.clone());
    move |v| {
        commit(&ctx, &scope, field, (rev, &set_rev), &set_status, |s| {
            apply(s, v)
        })
    }
}

/// One field a preset can override: [`described_labeled`] plus, while the preset in scope
/// overrides the row's key, the Overridden marker whose Reset is the only way back to
/// inheriting. Captions sit below their control, as on every Windows Settings page and the
/// Apple client. A control's edit pins the key ([`commit`]); WinUI raises no change event on
/// a no-op re-selection, so "not overridden" needs the Reset.
fn described_overridable(
    cx: &Cx,
    row: &rows::Row,
    control: impl Into<View>,
    caption: &str,
) -> View {
    let (field, label, pal) = (row.key, row.label, cx.pal);
    if !cx.overrides(field) {
        return described_labeled(pal, label, control, caption);
    }
    // The marker is one left-aligned capsule on its own line between label and control, so
    // every row's marker sits alike whatever the control's width: "Overridden" and "Reset"
    // as segments of one tinted pill, all of it the tap target.
    let (rev, set_rev, set_status) = (cx.rev, cx.set_rev.clone(), cx.set_status.clone());
    let scope = cx.scope.to_string();
    let pill = Border::new()
        .content(
            hstack((
                text("Overridden")
                    .font_size(11.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(pal.get(Token::SystemAttention))
                    .vertical_alignment(VerticalAlignment::Center),
                // The seam between the state and the action.
                blank(1.0, 12.0)
                    .background(ThemeBrush::CardStroke)
                    .vertical_alignment(VerticalAlignment::Center),
                text("Reset")
                    .font_size(11.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .foreground(ThemeBrush::AccentText)
                    .vertical_alignment(VerticalAlignment::Center),
            ))
            .spacing(7.0),
        )
        .background(pal.get(Token::SystemAttentionBackground))
        .border_brush(ThemeBrush::CardStroke)
        .border_thickness(uniform(1.0))
        .corner_radius(10.0)
        .padding(edges(10.0, 3.0, 10.0, 3.0))
        .horizontal_alignment(HorizontalAlignment::Left);
    let reset_pill = on_tap(pill, move || {
        let r = update_preset(&scope, |p| {
            p.overrides.clear(field);
        });
        // The catalog changed behind the controls, and nothing the page reads as state
        // did: the bump re-renders the row showing the inherited value.
        saved(r, &set_status, Some((rev, &set_rev)));
    })
    .tooltip("Overridden by this preset \u{2014} Reset returns it to Default settings");
    vstack((
        row_label(label),
        reset_pill,
        control.into(),
        row_caption(pal, caption),
    ))
    .spacing(6.0)
    .into()
}

/// The row's label line — what the widgets' `.header` used to render, moved out so the
/// Overridden pill can sit between label and input with ONE consistent gap everywhere.
fn row_label(label: &str) -> View {
    text(label)
        .horizontal_alignment(HorizontalAlignment::Left)
        .into()
}

/// The row's caption line (shared styling for every variant).
fn row_caption(pal: &Palette, caption: &str) -> View {
    text(caption)
        .font_size(12.0)
        .foreground(pal.get(Token::SecondaryText))
        .text_wrapping(TextWrapping::Wrap)
        .max_width(420.0)
        .horizontal_alignment(HorizontalAlignment::Left)
        .into()
}

/// The plain row with the row-owned label line: label, input, caption — the same skeleton
/// as an overridable row minus the pill, so both kinds space out identically.
fn described_labeled(pal: &Palette, label: &str, control: impl Into<View>, caption: &str) -> View {
    vstack((row_label(label), control.into(), row_caption(pal, caption)))
        .spacing(6.0)
        .into()
}

/// A settings sub-section heading. Deliberately NOT the shared [`section`] helper: that one
/// carries a 2px left inset (fine over the hosts/licenses lists it was written for), which
/// here left every heading hanging one nudge right of the card edge below it. Flush left, so
/// heading and card share one line.
fn group_heading(pal: &Palette, label: &str) -> View {
    text(label)
        .font_size(12.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .foreground(pal.get(Token::SecondaryText))
        .horizontal_alignment(HorizontalAlignment::Left)
        .margin(edges(0.0, 14.0, 0.0, 2.0))
        .into()
}

/// One settings group: an optional sub-section label, a card of fields, and an optional
/// form-level note under it (Apple's Section header/footer). Groups stack down the page.
/// A group with NO fields renders NOTHING — several groups pass an empty list in preset
/// scope (Decoding, Library: device facts, never per preset), and a heading over an empty
/// card read as a bug.
fn group(
    pal: &Palette,
    header: Option<&str>,
    fields: Vec<View>,
    footer: Option<&str>,
) -> Vec<View> {
    if fields.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(3);
    if let Some(h) = header {
        out.push(group_heading(pal, h));
    }
    out.push(card(vstack(fields).spacing(14.0)).into());
    if let Some(f) = footer {
        out.push(
            text(f)
                .font_size(12.0)
                .foreground(pal.get(Token::SecondaryText))
                .text_wrapping(TextWrapping::Wrap)
                .horizontal_alignment(HorizontalAlignment::Left)
                .margin(edges(0.0, 6.0, 0.0, 0.0))
                .into(),
        );
    }
    out
}

/// A section's advanced rows: under an Advanced heading while Show advanced is on or this
/// preset overrides one of them; otherwise one button naming how many hold a changed value,
/// which shows them. Nothing when none changed.
fn advanced_group(cx: &Cx, fields: Vec<View>, changed: usize, overridden: bool) -> Vec<View> {
    if cx.s.show_advanced || overridden {
        return group(cx.pal, Some("Advanced"), fields, None);
    }
    if changed == 0 || cx.preset_mode {
        return Vec::new();
    }
    let label = match changed {
        1 => "1 advanced setting changed".to_string(),
        n => format!("{n} advanced settings changed"),
    };
    let (ctx, rev, set_rev, set_status) = (
        cx.ctx.clone(),
        cx.rev,
        cx.set_rev.clone(),
        cx.set_status.clone(),
    );
    let show = button(label).on_click(move || {
        // Device-wide, so the global layer whatever the scope.
        commit(
            &ctx,
            "",
            "show_advanced",
            (rev, &set_rev),
            &set_status,
            |s| s.show_advanced = true,
        );
    });
    group(cx.pal, None, vec![show.into()], None)
}

/// What every section's rows read: the layer in scope, its effective values, the preset's
/// overrides, the revision a commit bumps and the status line a failed save lands on.
struct Cx<'a> {
    ctx: &'a Arc<AppCtx>,
    pal: &'a Palette,
    scope: &'a str,
    rev: u64,
    set_rev: &'a AsyncSetState<u64>,
    set_status: &'a AsyncSetState<String>,
    s: Settings,
    /// The preset's overrides; `None` in the defaults scope.
    overlay: Option<&'a SettingsOverlay>,
    preset_mode: bool,
    /// Resolution sits on Custom… though the stored size is a listed one.
    custom_res: bool,
    set_custom_res: &'a AsyncSetState<bool>,
    /// Render scale sits on Custom… though the stored scale is a listed stop.
    custom_scale: bool,
    set_custom_scale: &'a AsyncSetState<bool>,
}

impl Cx<'_> {
    /// Whether the preset in scope overrides `field`, by [`SettingsOverlay::clear`]'s names.
    fn overrides(&self, field: &str) -> bool {
        self.overlay.is_some_and(|o| o.overrides(field))
    }
}

/// The sidebar's sections in the Apple client's order: tag, title, Lucide mark.
const SECTIONS: [(&str, &str, &str); 7] = [
    ("general", "General", "settings"),
    ("display", "Display", "maximize"),
    ("input", "Input", "keyboard"),
    ("quick_actions", "Quick actions", "rotate-cw"),
    ("audio", "Audio", "volume-2"),
    ("controllers", "Controllers", "gamepad-2"),
    ("about", "About", "circle-help"),
];

/// The settings screen: a stock WinUI `NavigationView` (the Windows-Settings sidebar pattern),
/// one pane item per section and the section's cards as the content, under the scope bar. The
/// way back is the shell's title bar. The `section_enter` tween (0 → 1) is the section-switch
/// entrance, mapped onto the content column's opacity and offset.
pub(crate) fn settings_page(shell: &Shell) -> View {
    let (ctx, pal, section) = (&shell.ctx, &shell.pal, shell.settings_nav.as_str());
    let (set_screen, set_status) = (shell.set(Msg::Screen), shell.set(Msg::Status));
    let (set_scope, set_delete) = (
        shell.set(Msg::SettingsScope),
        shell.set(Msg::SettingsDelete),
    );
    let (set_edit, set_rev) = (shell.set(Msg::SettingsEdit), shell.set(Msg::SettingsRev));
    let (set_custom_res, set_custom_scale) =
        (shell.set(Msg::CustomRes), shell.set(Msg::CustomScale));
    // The layer being edited. A scope pointing at a deleted preset degrades to the defaults,
    // the same rule a dangling host binding follows.
    let active = active_preset(&shell.settings_scope);
    let scope: &str = match &active {
        Some(p) => &p.id,
        None => "",
    };
    let preset_mode = active.is_some();
    // Every control shows the EFFECTIVE value: the global underneath with this preset's
    // overrides on top, so a row the preset doesn't override reads as the live global.
    let s = {
        let base = ctx.settings.lock().unwrap().clone();
        match &active {
            Some(p) => p.overrides.apply(&base),
            None => base,
        }
    };

    let cx = Cx {
        ctx,
        pal,
        scope,
        rev: shell.settings_rev,
        set_rev: &set_rev,
        set_status: &set_status,
        s,
        overlay: active.as_ref().map(|p| &p.overrides),
        preset_mode,
        custom_res: shell.custom_res,
        set_custom_res: &set_custom_res,
        custom_scale: shell.custom_scale,
        set_custom_scale: &set_custom_scale,
    };
    // The selected section's content, grouped exactly like the Apple client's categories
    // (SettingsCategory + SettingsView+Sections.swift). Each section builds only its own rows.
    let groups: Vec<View> = match section {
        "display" => display::display_section(&cx),
        "input" => input::input_section(&cx),
        "quick_actions" => input::quick_actions_section(&cx),
        "controllers" => controllers::controllers_section(&cx),
        "audio" => audio::audio_section(&cx),
        "about" => about::about_section(pal, &set_screen),
        // "general" and anything unrecognized.
        _ => general::general_section(&cx),
    };
    let title = SECTIONS
        .iter()
        .find(|(tag, ..)| *tag == section)
        .map_or("General", |(_, title, _)| title);

    let items = SECTIONS.iter().map(|(tag, title, mark)| {
        KeyedView::new(
            *tag,
            NavigationViewItem::new()
                .tag(tag)
                .content(*title)
                .icon(lucide::icon(mark, pal))
                .is_selected(*tag == section),
        )
    });
    let scope_bar = preset_sheet::scope_bar(&cx, active.as_ref(), &set_scope, &set_edit);

    // The content column carries the section entrance and the category title, so the title
    // shares the cards' left edge; no max-width, as the pane already takes a third.
    let titled: Vec<View> = std::iter::once(
        text(title)
            .font_size(28.0)
            .font_weight(FontWeight::SEMI_BOLD)
            .horizontal_alignment(HorizontalAlignment::Left)
            .margin(edges(0.0, 0.0, 0.0, 6.0))
            .into(),
    )
    .chain(groups)
    .collect();
    // Keyed on (scope, section): a switch of either mounts fresh controls, so a reused
    // ComboBox never shows one layer's selection over another's values.
    let progress = shell.section_enter.eased();
    let column = Border::new()
        .content(vstack(()).keyed_children([KeyedView::new(
            format!("{scope}/{section}"),
            vstack(titled).spacing(10.0),
        )]))
        .padding(edges(24.0, 20.0, 28.0, 40.0))
        .opacity(progress)
        .translation(0.0, (1.0 - progress) * 22.0);
    let nav = NavigationView::new()
        .keyed_menu_items(items)
        // ScrollView scrolls through its direct child's Translation, and a Translation write
        // stops that scroll until the child resizes. The tweened column stays one Border in.
        .content(ScrollView::new().content(Border::new().content(column)))
        .pane_title("Settings")
        .on_selected_tag_changed({
            let ss = shell.set(Msg::SettingsNav);
            move |tag: Option<std::rc::Rc<str>>| {
                if let Some(tag) = tag {
                    ss.call(tag.to_string())
                }
            }
        })
        .is_settings_visible(false)
        .is_back_button_visible(NavigationViewBackButtonVisible::Collapsed);
    let sheet_slot: View = if shell.settings_edit && preset_mode {
        // The preset sheet — "Edit preset…" in the bar. The bar owns the scope choice,
        // so the sheet carries only the preset being edited.
        preset_sheet::edit_preset_modal(&cx, active.as_ref(), &set_scope, &set_delete, &set_edit)
    } else {
        Border::new().into()
    };
    // Saves are fire-and-forget, so a config store that refuses writes looks normal until a
    // restart loses everything. When it refuses, say so and name the path.
    let mut bars: Vec<View> = Vec::new();
    if let Some(err) = pf_client_core::trust::store_health::last_error() {
        bars.push(
            Border::new()
                .content(
                    InfoBar::new()
                        .title("Your changes aren\u{2019}t being saved")
                        .message(format!(
                            "Punktfunk can\u{2019}t write to its settings folder, so nothing \
                             on this page will survive a restart. {err}"
                        ))
                        .severity(InfoBarSeverity::Error)
                        .is_open(true)
                        .is_closable(false),
                )
                .margin(edges(24.0, 12.0, 28.0, 0.0))
                .into(),
        );
    }
    bars.push(scope_bar);
    // The bar rides an Auto row above the nav's Star row, so the nav (and the sheet's scrim
    // over it) still fills the rest of the window.
    Grid::new()
        .children((
            vstack(bars).grid_row(0),
            Grid::new().children((nav, sheet_slot)).grid_row(1),
        ))
        .rows([GridLength::Auto, GridLength::STAR])
        .content_dialog(preset_sheet::delete_dialog(
            &cx,
            &shell.settings_delete,
            &set_scope,
            &set_delete,
            &set_edit,
        ))
}
