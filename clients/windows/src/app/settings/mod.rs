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

pub(crate) use preset_sheet::hex_color;

use super::lucide;
use super::style::*;
use super::{AppCtx, Screen};
use crate::trust::Settings;
use pf_client_core::presets::{PresetsFile, StreamPreset};
use std::sync::Arc;
use windows_reactor::*;

/// Persist one control's edit into the layer being edited.
///
/// This shell commits PER CONTROL (unlike the GTK one, which writes when its dialog closes),
/// so it can't hand the preset a list of touched fields. It hands over the effective settings
/// before and after instead, and [`SettingsOverlay::absorb`] records the field that moved —
/// the comparison is against what the control was SHOWING, so picking a value that happens to
/// equal the global still records an override (the pin the design asks for).
///
/// Every commit ends by bumping the revision: a preset-scope edit changes what the page
/// should SHOW (the row's Overridden marker, the catalog behind the controls) without
/// changing any state the page reads, so without the bump no render pass runs and the
/// marker only appears after some unrelated re-render — the exact bug the Linux client
/// fixed in "the override marker appears on touch". Bumping on global-scope edits too is
/// deliberate: it is one code path, a same-value repaint is cheap, and it also refreshes
/// rows whose displayed effective value derives from the field just written.
pub(super) fn commit(
    ctx: &Arc<AppCtx>,
    scope: &str,
    rev: (u64, &AsyncSetState<u64>),
    edit: impl FnOnce(&mut Settings),
) {
    if scope.is_empty() {
        // Rebase on the file before the whole-struct save: a spawned session and the console
        // write it too (match-window size, their own settings), and saving the stale snapshot
        // in `ctx.settings` would revert them. presets.rs says why there is no merge. The
        // snapshot follows the fresh load, so every row renders what is on disk.
        let mut s = ctx.settings.lock().unwrap();
        *s = Settings::load();
        edit(&mut s);
        s.save();
        rev.1.call(rev.0 + 1);
        return;
    }
    let mut catalog = PresetsFile::load();
    // The same rebase as the global arm above: `base` is what `absorb`'s before/after
    // effective settings derive from, and the snapshot is not the file — another process
    // (session resize, console UI, Decky) may have moved a global under us. The historical
    // rebase fix ("settings saves stop reverting each other") covered the whole-file
    // writers but missed this arm.
    let base = {
        let mut s = ctx.settings.lock().unwrap();
        *s = Settings::load();
        s.clone()
    };
    let Some(p) = catalog.presets.iter_mut().find(|p| p.id == scope) else {
        return; // deleted from under us; the next render falls back to the defaults scope
    };
    let before = p.overrides.apply(&base);
    let mut after = before.clone();
    edit(&mut after);
    p.overrides.absorb(&before, &after);
    if let Err(e) = catalog.save() {
        tracing::warn!(error = %format!("{e:#}"), "saving the preset catalog");
    }
    rev.1.call(rev.0 + 1);
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
    };
}

/// This device's pickable hardware: DXGI adapters and WASAPI endpoints. Probed once per
/// visit by [`refresh_snapshot`], never on the render each settings commit triggers.
#[derive(Default)]
pub(crate) struct DeviceProbes {
    gpus: Vec<String>,
    speakers: Vec<pf_client_core::audio::AudioDevice>,
    mics: Vec<pf_client_core::audio::AudioDevice>,
}

/// Which tier-P rows the preset in scope overrides. Plain bools rather than a lookup so the
/// call sites read as `over.codec` — the row and its flag stay visibly paired.
#[derive(Default)]
struct OverrideFlags {
    resolution: bool,
    refresh_hz: bool,
    render_scale: bool,
    bitrate_kbps: bool,
    codec: bool,
    hdr_enabled: bool,
    enable_444: bool,
    ten_bit_sdr: bool,
    compositor: bool,
    audio_channels: bool,
    audio_format: bool,
    keep_host_audio: bool,
    mic_enabled: bool,
    echo_cancel: bool,
    touch_mode: bool,
    mouse_mode: bool,
    invert_scroll: bool,
    inhibit_shortcuts: bool,
    gamepad: bool,
    gamepad_forwarding: bool,
    system_buttons: bool,
    guide_gesture: bool,
    stats_verbosity: bool,
    fullscreen_on_stream: bool,
    video_fit: bool,
    present_priority: bool,
    smooth_buffer: bool,
    vsync: bool,
    allow_vrr: bool,
    /// The whole ring: a preset that touches it owns all of it (D10).
    overlay_actions: bool,
}

impl OverrideFlags {
    fn of(preset: Option<&StreamPreset>) -> OverrideFlags {
        let Some(o) = preset.map(|p| &p.overrides) else {
            return OverrideFlags::default();
        };
        OverrideFlags {
            // One control drives the width/height/match-window tri-state, so any of the three
            // marks the row.
            resolution: o.width.is_some() || o.height.is_some() || o.match_window.is_some(),
            refresh_hz: o.refresh_hz.is_some(),
            render_scale: o.render_scale.is_some(),
            bitrate_kbps: o.bitrate_kbps.is_some(),
            codec: o.codec.is_some(),
            hdr_enabled: o.hdr_enabled.is_some(),
            enable_444: o.enable_444.is_some(),
            ten_bit_sdr: o.ten_bit_sdr.is_some(),
            compositor: o.compositor.is_some(),
            audio_channels: o.audio_channels.is_some(),
            audio_format: o.audio_format.is_some(),
            keep_host_audio: o.keep_host_audio.is_some(),
            mic_enabled: o.mic_enabled.is_some(),
            echo_cancel: o.echo_cancel.is_some(),
            touch_mode: o.touch_mode.is_some(),
            mouse_mode: o.mouse_mode.is_some(),
            invert_scroll: o.invert_scroll.is_some(),
            inhibit_shortcuts: o.inhibit_shortcuts.is_some(),
            gamepad: o.gamepad.is_some(),
            gamepad_forwarding: o.gamepad_forwarding.is_some(),
            system_buttons: o.system_buttons.is_some(),
            guide_gesture: o.guide_gesture.is_some(),
            stats_verbosity: o.stats_verbosity.is_some(),
            fullscreen_on_stream: o.fullscreen_on_stream.is_some(),
            video_fit: o.video_fit.is_some(),
            present_priority: o.present_priority.is_some(),
            smooth_buffer: o.smooth_buffer.is_some(),
            vsync: o.vsync.is_some(),
            allow_vrr: o.allow_vrr.is_some(),
            overlay_actions: o.overlay_actions.is_some(),
        }
    }
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
    ctx: &Arc<AppCtx>,
    scope: &str,
    rev: (u64, &AsyncSetState<u64>),
    names: Vec<String>,
    current: usize,
    apply: impl Fn(&mut Settings, usize) + 'static,
) -> ComboBox {
    let (ctx, scope) = (ctx.clone(), scope.to_string());
    let (rev, set_rev) = (rev.0, rev.1.clone());
    let max = names.len().saturating_sub(1);
    ComboBox::new(names)
        .selected_index(current as i32)
        .on_selection_changed(move |i: i32| {
            // -1 is "nothing selected", which an in-place items rebuild raises: not a pick.
            let Ok(i) = usize::try_from(i) else {
                return;
            };
            commit(&ctx, &scope, (rev, &set_rev), |s| {
                apply(s, i.min(max));
            });
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
    ctx: &Arc<AppCtx>,
    scope: &str,
    rev: (u64, &AsyncSetState<u64>),
    on: bool,
    apply: impl Fn(&mut Settings, bool) + 'static,
) -> ToggleSwitch {
    let (ctx, scope) = (ctx.clone(), scope.to_string());
    let (rev, set_rev) = (rev.0, rev.1.clone());
    ToggleSwitch::new(on)
        .on_content("On")
        .off_content("Off")
        .on_toggled(move |v: bool| {
            commit(&ctx, &scope, (rev, &set_rev), |s| apply(s, v));
        })
}

/// One field: the control with its explanation directly underneath (Apple's `described`).
///
/// The caption goes BELOW the control on purpose. An earlier revision put guidance only in
/// hover tooltips because a paragraph *above* a control reads as that control's label — true,
/// but a caption under it reads as a caption, which is how every Windows Settings page and
/// the Apple client both do it. Width-capped for the same reason Apple caps at 360pt: a
/// full-width caption runs into the control column and the whole cell reads as one block.
/// [`described_labeled`], plus the override marker and reset a preset-scope row carries: the caption
/// says the preset changes this one, and the button is the only way back to inheriting.
/// An override is recorded when a control's committed value differs from what it was
/// SHOWING (`SettingsOverlay::absorb` diffs against the effective snapshot — see `commit`);
/// WinUI change events don't fire on a no-op re-selection, so every reachable edit marks
/// its row, and "not overridden" needs an explicit Reset. (Linux marks a literal no-op
/// touch too — unobservable here, the one intentional divergence.)
fn described_overridable(
    rev: (u64, &AsyncSetState<u64>),
    scope: &str,
    field: &'static str,
    label: &str,
    overridden: bool,
    control: impl Into<Element>,
    caption: &str,
) -> Element {
    if scope.is_empty() || !overridden {
        return described_labeled(label, control, caption);
    }
    // The marker is one left-aligned capsule on its own line between label and control, so
    // every row's marker sits alike whatever the control's width: "Overridden" and "Reset"
    // as segments of one tinted pill, all of it the tap target.
    let (rev, set_rev) = (rev.0, rev.1.clone());
    let scope = scope.to_string();
    let reset_pill = border(
        hstack((
            text_block("Overridden")
                .font_size(11.0)
                .semibold()
                .foreground(ThemeRef::SystemAttention)
                .vertical_alignment(VerticalAlignment::Center),
            // The seam between the state and the action.
            border(vstack(Vec::<Element>::new()).width(1.0).height(12.0))
                .background(ThemeRef::CardStroke)
                .vertical_alignment(VerticalAlignment::Center),
            text_block("Reset")
                .font_size(11.0)
                .semibold()
                .foreground(ThemeRef::AccentText)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .spacing(7.0),
    )
    .background(ThemeRef::SystemAttentionBackground)
    .border_brush(ThemeRef::CardStroke)
    .border_thickness(uniform(1.0))
    .corner_radius(10.0)
    .padding(edges(10.0, 3.0, 10.0, 3.0))
    .tooltip("Overridden by this preset \u{2014} Reset returns it to Default settings")
    .on_tapped(move || {
        let mut catalog = PresetsFile::load();
        if let Some(p) = catalog.presets.iter_mut().find(|p| p.id == scope) {
            p.overrides.clear(field);
            if let Err(e) = catalog.save() {
                tracing::warn!(error = %format!("{e:#}"), "clearing an override");
            }
        }
        // The catalog changed behind the controls, and nothing the page reads as state
        // did — bump the revision so the row re-renders showing the inherited value.
        set_rev.call(rev + 1);
    });
    vstack((
        row_label(label),
        Element::from(reset_pill).horizontal_alignment(HorizontalAlignment::Left),
        control.into(),
        row_caption(caption),
    ))
    .spacing(6.0)
    .into()
}

/// The row's label line — what the widgets' `.header` used to render, moved out so the
/// Overridden pill can sit between label and input with ONE consistent gap everywhere.
fn row_label(label: &str) -> Element {
    text_block(label)
        .horizontal_alignment(HorizontalAlignment::Left)
        .into()
}

/// The row's caption line (shared styling for every variant).
fn row_caption(caption: &str) -> Element {
    text_block(caption)
        .font_size(12.0)
        .foreground(ThemeRef::SecondaryText)
        .wrap()
        .max_width(420.0)
        .horizontal_alignment(HorizontalAlignment::Left)
        .into()
}

/// The plain row with the row-owned label line: label, input, caption — the same skeleton
/// as an overridable row minus the pill, so both kinds space out identically.
fn described_labeled(label: &str, control: impl Into<Element>, caption: &str) -> Element {
    vstack((row_label(label), control.into(), row_caption(caption)))
        .spacing(6.0)
        .into()
}

/// A settings sub-section heading. Deliberately NOT the shared [`section`] helper: that one
/// carries a 2px left inset (fine over the hosts/licenses lists it was written for), which
/// here left every heading hanging one nudge right of the card edge below it. Flush left, so
/// heading and card share one line.
fn group_heading(label: &str) -> Element {
    text_block(label)
        .font_size(12.0)
        .semibold()
        .foreground(ThemeRef::SecondaryText)
        .horizontal_alignment(HorizontalAlignment::Left)
        .margin(edges(0.0, 14.0, 0.0, 2.0))
        .into()
}

/// One settings group: an optional sub-section label, a card of fields, and an optional
/// form-level note under it (Apple's Section header/footer). Groups stack down the page.
/// A group with NO fields renders NOTHING — several groups pass an empty list in preset
/// scope (Decoding, Library: device facts, never per preset), and a heading over an empty
/// card read as a bug.
fn group(header: Option<&str>, fields: Vec<Element>, footer: Option<&str>) -> Vec<Element> {
    if fields.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(3);
    if let Some(h) = header {
        out.push(group_heading(h));
    }
    out.push(card(vstack(fields).spacing(14.0)).into());
    if let Some(f) = footer {
        out.push(
            text_block(f)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .wrap()
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
fn advanced_group(cx: &Cx, fields: Vec<Element>, changed: usize, overridden: bool) -> Vec<Element> {
    if cx.s.show_advanced || overridden {
        return group(Some("Advanced"), fields, None);
    }
    if changed == 0 || cx.preset_mode {
        return Vec::new();
    }
    let label = match changed {
        1 => "1 advanced setting changed".to_string(),
        n => format!("{n} advanced settings changed"),
    };
    let (ctx, rev, set_rev) = (cx.ctx.clone(), cx.rev, cx.set_rev.clone());
    let show = button(label).on_click(move || {
        // Device-wide, so the global layer whatever the scope.
        commit(&ctx, "", (rev, &set_rev), |s| s.show_advanced = true);
    });
    group(None, vec![show.into()], None)
}

/// What every section's rows read: the layer in scope, its effective values, which of them
/// the preset overrides, and the revision a commit bumps.
struct Cx<'a> {
    ctx: &'a Arc<AppCtx>,
    scope: &'a str,
    rev: u64,
    set_rev: &'a AsyncSetState<u64>,
    s: Settings,
    over: OverrideFlags,
    preset_mode: bool,
    /// Resolution sits on Custom… though the stored size is a listed one.
    custom_res: bool,
    set_custom_res: &'a AsyncSetState<bool>,
}

/// The settings screen: a stock WinUI `NavigationView` (the Windows-Settings sidebar pattern) —
/// one pane item per section, the section's card as the content, the built-in back arrow
/// returning to the host list. `section`/`set_section` are the selected pane tag, held in ROOT
/// state (this page stays hook-free): `on_selection_changed` is wired in the reactor backend, so
/// only a root `AsyncSetState` reliably re-renders the new section in. `progress` is the
/// section-switch entrance tween (0 → 1), mapped onto the content column's opacity + offset.
#[allow(clippy::too_many_arguments)]
pub(crate) fn settings_page(
    ctx: &Arc<AppCtx>,
    set_screen: &AsyncSetState<Screen>,
    section: &str,
    set_section: &AsyncSetState<String>,
    scope_id: &str,
    set_scope: &AsyncSetState<String>,
    delete_pending: &Option<String>,
    set_delete: &AsyncSetState<Option<String>>,
    edit_open: bool,
    set_edit: &AsyncSetState<bool>,
    custom_res: bool,
    set_custom_res: &AsyncSetState<bool>,
    rev: u64,
    set_rev: &AsyncSetState<u64>,
    progress: f64,
) -> Element {
    // The layer being edited. A scope pointing at a deleted preset degrades to the defaults,
    // the same rule a dangling host binding follows.
    let active = active_preset(scope_id);
    let scope: &str = match &active {
        Some(p) => &p.id,
        None => "",
    };
    let preset_mode = active.is_some();
    // Which rows this preset overrides — the marker + reset each of them carries. In the
    // defaults scope nothing is marked, and `described_overridable` degrades to `described_labeled`.
    let over = OverrideFlags::of(active.as_ref());
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
        scope,
        rev,
        set_rev,
        s,
        over,
        preset_mode,
        custom_res,
        set_custom_res,
    };
    // The selected section's content, grouped exactly like the Apple client's categories
    // (SettingsCategory + SettingsView+Sections.swift). Each section builds only its own rows.
    let (title, groups): (&str, Vec<Element>) = match section {
        "display" => ("Display", display::display_section(&cx)),
        "input" => ("Input", input::input_section(&cx)),
        "quick_actions" => ("Quick actions", input::quick_actions_section(&cx)),
        "controllers" => ("Controllers", controllers::controllers_section(&cx)),
        "audio" => ("Audio", audio::audio_section(&cx)),
        "about" => ("About", about::about_section(set_screen)),
        // "general" and anything unrecognized.
        _ => ("General", general::general_section(&cx)),
    };

    // The stock WinUI sidebar (Windows-Settings pattern): pane on the left, the section's card
    // as content, the NavigationView's own back arrow returning to the host list. Auto display
    // mode collapses the pane on a narrow window, exactly like Windows Settings.
    // Category order mirrors the Apple client's sidebar exactly.
    let items = vec![
        NavViewItem::new("General")
            .tag("general")
            .icon(lucide::icon("settings")),
        NavViewItem::new("Display")
            .tag("display")
            .icon(lucide::icon("maximize")),
        NavViewItem::new("Input")
            .tag("input")
            .icon(lucide::icon("keyboard")),
        NavViewItem::new("Quick actions")
            .tag("quick_actions")
            .icon(lucide::icon("rotate-cw")),
        NavViewItem::new("Audio")
            .tag("audio")
            .icon(lucide::icon("volume-2")),
        NavViewItem::new("Controllers")
            .tag("controllers")
            .icon(lucide::icon("gamepad-2")),
        NavViewItem::new("About")
            .tag("about")
            .icon(lucide::icon("circle-help")),
    ];
    let scope_bar = preset_sheet::scope_bar(active.as_ref(), set_scope, set_edit);

    // The card is keyed by section, so a pane switch remounts it (a reused ComboBox loses its
    // selection). The content column carries the section entrance and the category title, so
    // the title shares the cards' left edge; no max-width, as the pane already takes a third.
    let titled: Vec<Element> = std::iter::once(
        text_block(title)
            .font_size(28.0)
            .semibold()
            .horizontal_alignment(HorizontalAlignment::Left)
            .margin(edges(0.0, 0.0, 0.0, 6.0))
            .into(),
    )
    .chain(groups)
    .collect();
    // The keyed column sits inside a panel's child list: `ScrollView::children()` reconciles
    // its one child positionally and ignores keys, so a section switch would diff one
    // section's combos into another's and leave them blank. A vstack takes the keyed path.
    let scrolled = scroll_view(
        // ⚠️ Keyed on (scope, section), not section alone: switching SCOPE re-renders the same
        // section's controls with different values, and an in-place diff re-sets each reused
        // ComboBox's items (clearing WinUI's selection) while skipping `selected_index`
        // wherever the two scopes' values compare equal — the combo then renders blank. A
        // fresh mount applies every prop. Same reason the section key exists.
        vstack(vec![vstack(titled)
            .spacing(10.0)
            .with_key(format!("{scope}/{section}"))
            .into()])
        .margin(edges(24.0, 20.0, 28.0, 40.0)),
    )
    .opacity(progress)
    .margin(edges(0.0, (1.0 - progress) * 22.0, 0.0, 0.0));
    let content: Element = scrolled.into();
    let confirm = preset_sheet::delete_dialog(delete_pending, set_scope, set_delete, set_edit);
    let nav = NavigationView::new(items, content)
        .pane_title("Settings")
        .selected_tag(section)
        .on_selection_changed({
            let ss = set_section.clone();
            move |tag: String| ss.call(tag)
        })
        .settings_visible(false)
        .back_enabled(true)
        .on_back_requested({
            let ss = set_screen.clone();
            move || ss.call(Screen::Hosts)
        });
    // Overlay layers fill the nav's cell (a vstack would hand it its desired height). The list
    // is stable, always [nav, sheet slot, dialog]: removing a grid child breaks the phantom-
    // dialog bookkeeping (`delete_dialog`), so a closed sheet leaves an empty Border in its
    // slot, which a null background keeps from taking clicks.
    let sheet_slot: Element = if edit_open && preset_mode {
        // The preset sheet — "Edit preset…" in the bar. The bar owns the scope choice,
        // so the sheet carries only the preset being edited.
        preset_sheet::edit_preset_modal(
            active.as_ref(),
            None,
            set_scope,
            set_delete,
            set_edit,
            rev,
            set_rev,
        )
    } else {
        border(vstack(Vec::<Element>::new())).into()
    };
    // Saves are fire-and-forget, so a config store that refuses writes looks normal until a
    // restart loses everything. When it refuses, say so and name the path. Always mounted, like
    // `sheet_slot`: a Border in both states (an empty one is not hit-testable), since adding or
    // removing a child is where this reconciler's phantom bookkeeping breaks.
    let store_slot: Element = match pf_client_core::trust::store_health::last_error() {
        Some(err) => border(
            InfoBar::new("Your changes aren\u{2019}t being saved")
                .message(format!(
                    "Punktfunk can\u{2019}t write to its settings folder, so nothing on this \
                     page will survive a restart. {err}"
                ))
                .error()
                .is_closable(false),
        )
        .margin(edges(24.0, 12.0, 28.0, 0.0))
        .into(),
        None => border(vstack(Vec::<Element>::new())).into(),
    };
    // The bar rides an Auto row above the nav's Star row, so the nav (and the sheet's scrim
    // over it) still fills the rest of the window.
    grid(vec![
        Element::from(vstack(vec![store_slot, scope_bar])).grid_row(0),
        Element::from(grid(vec![nav.into(), sheet_slot, confirm])).grid_row(1),
    ])
    .rows([GridLength::Auto, GridLength::STAR])
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::presets::SettingsOverlay;

    /// Every overlay field maps to its row flag — including the tri-state resolution
    /// (any of width/height/match_window marks the one Resolution row) and the 4:4:4
    /// switch added for GTK parity. A field that records without marking its row is the
    /// original Overridden-row bug wearing a new face.
    #[test]
    fn override_flags_mirror_the_overlay() {
        let none = OverrideFlags::of(None);
        assert!(!none.resolution && !none.enable_444 && !none.codec);

        let mut p = StreamPreset::new("t".to_string());
        p.overrides = SettingsOverlay {
            match_window: Some(true),
            enable_444: Some(true),
            codec: Some("hevc".into()),
            bitrate_kbps: Some(20000),
            ..Default::default()
        };
        let f = OverrideFlags::of(Some(&p));
        assert!(f.resolution, "match_window alone marks the Resolution row");
        assert!(f.enable_444);
        assert!(f.codec);
        assert!(f.bitrate_kbps);
        assert!(!f.hdr_enabled && !f.compositor && !f.render_scale);

        let mut p2 = StreamPreset::new("t2".to_string());
        p2.overrides = SettingsOverlay {
            width: Some(3840),
            height: Some(2160),
            ..Default::default()
        };
        assert!(OverrideFlags::of(Some(&p2)).resolution);

        // The audio pair: the mic and its echo canceller are separate overrides, so a preset
        // can pin one without claiming the other.
        let mut p3 = StreamPreset::new("t3".to_string());
        p3.overrides = SettingsOverlay {
            echo_cancel: Some(false),
            ..Default::default()
        };
        let f3 = OverrideFlags::of(Some(&p3));
        assert!(f3.echo_cancel);
        assert!(!f3.mic_enabled);

        // Channels and format are likewise independent — a "lossless on this host" preset that
        // leaves the layout following the global is valid, and the two are separate keys in the
        // catalog every client shares.
        let mut p3b = StreamPreset::new("t3b".to_string());
        p3b.overrides = SettingsOverlay {
            audio_format: Some(pf_client_core::session::AUDIO_FORMAT_LOSSLESS_96.into()),
            ..Default::default()
        };
        let f3b = OverrideFlags::of(Some(&p3b));
        assert!(f3b.audio_format);
        assert!(!f3b.audio_channels);

        // The presentation pair, likewise independent: pinning the intent doesn't claim
        // the buffer (a "Smoothness, whatever the global buffer is" preset is valid).
        let mut p4 = StreamPreset::new("t4".to_string());
        p4.overrides = SettingsOverlay {
            present_priority: Some("smooth".into()),
            ..Default::default()
        };
        let f4 = OverrideFlags::of(Some(&p4));
        assert!(f4.present_priority);
        assert!(!f4.smooth_buffer);

        // V-Sync and VRR are independent of each other and of the intent pair.
        let mut p5 = StreamPreset::new("t5".to_string());
        p5.overrides = SettingsOverlay {
            vsync: Some(false),
            ..Default::default()
        };
        let f5 = OverrideFlags::of(Some(&p5));
        assert!(f5.vsync);
        assert!(!f5.allow_vrr && !f5.present_priority);
    }
}
