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

use super::lucide;
use super::style::*;
use super::{AppCtx, Screen};
use crate::trust::{KnownHosts, Settings};
use pf_client_core::presets::{PresetsFile, StreamPreset};
// The audio-format table lives in the session crate, not here: the same three stored values also
// have to reach the wire, and they are shared verbatim with the Apple and Android clients so one
// preset round-trips. A second copy of the spellings in this file is exactly the drift the
// shared table exists to prevent — which is why this row has no `const` beside AUDIO_CHANNELS.
use pf_client_core::session::AUDIO_FORMATS;
use pf_client_core::start;
use pf_client_core::trust::{HudCorner, StatsVerbosity};
use punktfunk_core::config::GamepadPref;
use punktfunk_core::hud::{stats_scale, STATS_SCALE_PCTS};
use std::sync::Arc;
use windows_reactor::*;

/// Sizes by family; the Resolution combo lists one family at a time behind the Aspect combo.
/// `(0, 0)` = the native size of the display the window is on, resolved at connect.
use punktfunk_core::resolutions::{aspect_of, nearest, ASPECTS};
/// `0` = the display's native refresh, resolved at connect.
const REFRESH: &[u32] = &[0, 30, 60, 90, 120, 144, 165, 240];
/// Render-scale multipliers. `1.0` = Native; applied at connect and each match-window resize.
use punktfunk_core::render_scale::PRESETS as RENDER_SCALES;

/// A compact label for a render-scale multiplier: "Native" / "1.5×" / "2× (supersample)".
fn render_scale_label(scale: f64) -> String {
    if scale == 1.0 {
        "Native".to_string()
    } else if scale > 1.0 {
        format!("{scale}\u{00D7} (supersample)")
    } else {
        format!("{scale}\u{00D7}")
    }
}
/// Decode backend presets: `(stored value, display label)`.
// A stored legacy value that matches no preset (the D3D11VA-era "hardware", and since M10
// the bare "vulkan"/"d3d11va" that named libavcodec's rungs) shows as Automatic — which is
// how the session's ladder reads "hardware", and near enough for the other two, which
// `pf_client_core::video::migrate_decoder_pref` maps onto the entries below anyway.
const DECODERS: &[(&str, &str)] = &[
    ("auto", "Automatic (GPU, fall back to CPU)"),
    ("native-vulkan", "Hardware (Vulkan Video)"),
    ("native-d3d11va", "Hardware (Direct3D 11 / DXVA)"),
    ("software", "Software (CPU)"),
];
/// Audio channel presets: `(channel count, display label)`. The host clamps to what it can
/// capture; the resolved count drives the decoder + WASAPI render layout.
const AUDIO_CHANNELS: &[(u8, &str)] = &[(2, "Stereo"), (6, "5.1 Surround"), (8, "7.1 Surround")];
/// Preferred-codec presets: `(stored value, display label)`. Soft — the host falls back if it
/// can't encode the chosen codec.
const CODECS: &[(&str, &str)] = &[
    ("auto", "Automatic"),
    ("hevc", "HEVC (H.265)"),
    ("h264", "H.264 (AVC)"),
    ("av1", "AV1"),
    // Preference-only by design: `resolve_codec` never auto-picks PyroWave, and asking for
    // it on a host or device that can't do it simply falls back down the ladder to HEVC.
    ("pyrowave", "PyroWave (wired LAN)"),
];
/// Virtual-pad presets: `(stored value, display label)` — the pad the HOST creates. Same set the
/// GTK client offers; "Automatic" resolves from the physical controller at connect.
const GAMEPADS: &[(&str, &str)] = &[
    ("auto", "Automatic (match the controller)"),
    ("xbox360", "Xbox 360"),
    ("dualsense", "DualSense"),
    ("xboxone", "Xbox One"),
    ("dualshock4", "DualShock 4"),
    // Kept in lockstep with the GTK picker: this row was missing here, so a Windows
    // user could not ask the host for the Deck-shaped pad (trackpads, back grips).
    ("steamdeck", "Steam Deck"),
    ("steamcontroller2", "Steam Controller 2"),
];
/// System-button routing: `(stored value, display label)` — where the guide (Xbox/PS)
/// and quick-access presses land while streaming. The cross-client `system_buttons` key;
/// Automatic forwards on desktop and stays local under Gaming Mode.
const SYSTEM_BUTTONS: &[(&str, &str)] = &[
    ("auto", "Automatic"),
    ("forward", "Send to host"),
    ("local", "This device"),
];
/// The hold-Select guide gesture: `(stored value, display label)` — the cross-client
/// `guide_gesture` key. Automatic arms it only where the raw press can't reach the host.
const GUIDE_GESTURES: &[(&str, &str)] = &[("auto", "Automatic"), ("on", "On"), ("off", "Off")];
/// Stats-overlay tiers: `(stored value, display label)` — the cross-client verbosity ladder
/// (Compact ⊂ Normal ⊂ Detailed); Ctrl+Alt+Shift+S cycles it live in the session window.
const STATS_TIERS: &[(StatsVerbosity, &str)] = &[
    (StatsVerbosity::Off, "Off"),
    (StatsVerbosity::Compact, "Compact"),
    (StatsVerbosity::Normal, "Normal"),
    (StatsVerbosity::Detailed, "Detailed"),
];
/// Touch-input presets: `(stored value, display label)` — how a touchscreen's fingers drive
/// the host. The cross-client set (Android/Apple); only meaningful on a touchscreen device.
const TOUCH_MODES: &[(&str, &str)] = &[
    ("trackpad", "Trackpad"),
    ("pointer", "Direct pointer"),
    ("touch", "Touch passthrough"),
    ("off", "Off"),
];
/// Physical-mouse presets: `(stored value, display label)` — capture (pointer lock,
/// relative, for games) vs desktop (uncaptured absolute pointer, for remote desktop
/// work). Ctrl+Alt+Shift+M flips the model live in-stream.
const MOUSE_MODES: &[(&str, &str)] = &[
    ("capture", "Capture (games)"),
    ("desktop", "Desktop (absolute)"),
];
/// `video_fit`: `(stored value, display label)`. Unknown values show as Fit.
const VIDEO_FITS: &[(&str, &str)] = &[
    ("fit", "Fit"),
    ("crop", "Crop to fill"),
    ("stretch", "Stretch to fill"),
];
/// Presentation intent: `(stored value, display label)` — the `present_priority` key the
/// Apple and Android clients share, so one preset means the same thing everywhere.
const PRESENT_PRIORITIES: &[(&str, &str)] =
    &[("latency", "Lowest latency"), ("smooth", "Smoothness")];
/// Smoothness buffer depth in frames: `(stored value, display label)`. `0` = Automatic,
/// which resolves to 2 (`PresentPriority::resolve`). No millisecond hints — the cost is
/// one refresh per frame, and the refresh isn't known here when the mode is Native.
const SMOOTH_BUFFERS: &[(u8, &str)] = &[
    (0, "Automatic"),
    (1, "1 frame"),
    (2, "2 frames"),
    (3, "3 frames"),
];
/// Host compositor presets: `(stored value, display label)`. Advisory — the host falls back to
/// auto-detect when the choice is unavailable. Only meaningful against a Linux host.
const COMPOSITORS: &[(&str, &str)] = &[
    ("auto", "Automatic"),
    ("kwin", "KWin"),
    ("mutter", "Mutter (GNOME)"),
    ("hyprland", "Hyprland"),
    ("wlroots", "wlroots (Sway/River)"),
    ("gamescope", "gamescope"),
];

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
fn edit_preset_modal(
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

/// Names the host the Start in row resolves to, and says when it resolves to nothing — which
/// is what every value does until one host is paired. The pointer is written from a host's own
/// tile menu, not from this page, so the help line is where the two meet.
fn start_in_help() -> String {
    let known = KnownHosts::load();
    match start::default_host(&Settings::load(), &known) {
        Some(i) => format!(
            "Library opens {}\u{2019}s games; Stream also connects to its desktop. Back leaves \
             either one on the host list.",
            known.hosts[i].name
        ),
        None => "Opens on the host list: there is no default host yet. Pair one, or pick one \
                 from a host\u{2019}s menu when several are paired."
            .into(),
    }
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
    // The override marker is ONE capsule on its own line BETWEEN the control and its
    // caption (the reviewed placement): left-aligned like everything else in the card, so
    // every row's marker sits identically no matter how wide its control is. The capsule
    // holds the state ("Overridden") and the way out ("Reset") as segments of a single
    // tinted pill, the whole of which is the tap target; the caption below stays a plain
    // description in both states.
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

/// General: session, statistics.
fn general_section(cx: &Cx) -> Vec<Element> {
    let Cx {
        ctx,
        scope,
        rev,
        set_rev,
        ref s,
        ref over,
        preset_mode,
        ..
    } = *cx;
    let auto_wake_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.auto_wake, |s, on| {
        s.auto_wake = on
    });
    // Where a bare launch opens. A device preference like auto-wake beside it: which host this
    // machine opens on says nothing about how a stream should look, so it is never presetable.
    let start_in_combo = {
        let want = start::StartIn::parse(&s.start_in);
        let names = start::StartIn::ALL
            .iter()
            .map(|v| v.label().to_string())
            .collect();
        let current = start::StartIn::ALL
            .iter()
            .position(|v| *v == want)
            .unwrap_or(1);
        setting_combo(ctx, scope, (rev, set_rev), names, current, |s, i| {
            s.start_in = start::StartIn::ALL[i].as_str().to_string();
        })
    };
    let fullscreen_toggle = setting_toggle(
        ctx,
        scope,
        (rev, set_rev),
        s.fullscreen_on_stream,
        |s, on| s.fullscreen_on_stream = on,
    );

    let (hud_names, hud_i) = presets(STATS_TIERS, |v| *v == s.stats_verbosity());
    let hud_combo = setting_combo(ctx, scope, (rev, set_rev), hud_names, hud_i, |s, i| {
        s.set_stats_verbosity(STATS_TIERS[i].0);
    });
    let advanced_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.advanced_stats, |s, on| {
        s.advanced_stats = on
    });
    // Explorer hands a URL to the default browser; best-effort, like About's log folder.
    let stats_docs_button = button("What each number means").on_click(|| {
        let _ = std::process::Command::new("explorer.exe")
            .arg("https://docs.punktfunk.unom.io/docs/stats")
            .spawn();
    });

    let mut out = group(
        Some("Session"),
        vec![described_overridable(
            (rev, set_rev),
            scope,
            "fullscreen_on_stream",
            "Start streams fullscreen",
            over.fullscreen_on_stream,
            fullscreen_toggle,
            "Go fullscreen when a session starts; F11 or Alt+Enter switches back \
                 live.",
        )]
        .into_iter()
        // Auto-wake is about this host and this network, not about "Game vs Work" —
        // it stays global in v1 (design §3, tier H/G).
        .chain((!preset_mode).then(|| {
            described_labeled(
                "Auto-wake on connect",
                auto_wake_toggle,
                "Connecting to a saved host that\u{2019}s offline sends Wake-on-LAN and \
                 waits for it to boot. Turn off if hosts behind a VPN look offline when \
                 they aren\u{2019}t.",
            )
        }))
        .chain(
            (!preset_mode).then(|| described_labeled("Start in", start_in_combo, &start_in_help())),
        )
        .collect(),
        None,
    );
    let mut stats_rows = vec![described_overridable(
        (rev, set_rev),
        scope,
        "stats_verbosity",
        "Statistics overlay",
        over.stats_verbosity,
        hud_combo,
        "Live session stats in a corner overlay \u{2014} Compact is a one-line pill, \
         Detailed adds the stage breakdown. Ctrl+Alt+Shift+S cycles the tiers any time.",
    )];
    if !preset_mode {
        stats_rows.push(stats_docs_button.into());
    }
    out.extend(group(Some("Statistics"), stats_rows, None));
    // Device-wide, and shown in both scopes: it changes what this page lists, not a stream.
    let show_toggle = setting_toggle(ctx, "", (rev, set_rev), s.show_advanced, |s, on| {
        s.show_advanced = on
    });
    out.extend(group(
        None,
        vec![described_labeled(
            "Show advanced",
            show_toggle,
            "Adds the settings most players never need to change.",
        )],
        None,
    ));
    // Device-wide rows: a preset never carries them.
    if !preset_mode {
        let corner = s.hud_corner(HudCorner::TopLeft);
        let corner_combo = setting_combo(
            ctx,
            scope,
            (rev, set_rev),
            HudCorner::ALL
                .iter()
                .map(|c| c.label().to_string())
                .collect(),
            HudCorner::ALL
                .iter()
                .position(|c| *c == corner)
                .unwrap_or(0),
            |s, i| s.hud_placement = HudCorner::ALL[i].as_name().into(),
        );
        let pct = (stats_scale(s.stats_scale_pct) * 100.0).round() as u16;
        let size_combo = setting_combo(
            ctx,
            scope,
            (rev, set_rev),
            STATS_SCALE_PCTS.iter().map(|p| format!("{p} %")).collect(),
            STATS_SCALE_PCTS.iter().position(|p| *p == pct).unwrap_or(1),
            |s, i| s.stats_scale_pct = STATS_SCALE_PCTS[i],
        );
        let hint_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.exit_hint, |s, on| {
            s.exit_hint = on
        });
        let changed = usize::from(s.advanced_stats)
            + usize::from(corner != HudCorner::TopLeft)
            + usize::from(pct != 100)
            + usize::from(!s.exit_hint);
        out.extend(advanced_group(
            cx,
            vec![
                described_labeled(
                    "Advanced statistics",
                    advanced_toggle,
                    "Off shows the figures Moonlight's overlay also shows. On shows capture \
                     to glass as p50/p95 and every stage between.",
                ),
                described_labeled(
                    "Statistics position",
                    corner_combo,
                    "The corner the statistics overlay sits in.",
                ),
                described_labeled(
                    "Statistics size",
                    size_combo,
                    "The overlay's size, on top of your display's scaling.",
                ),
                described_labeled(
                    "Exit hint",
                    hint_toggle,
                    "Shows how to leave for a few seconds when a stream starts.",
                ),
            ],
            changed,
            false,
        ));
    }
    out
}

/// Display: resolution, quality, decoding, presentation, host output.
fn display_section(cx: &Cx) -> Vec<Element> {
    let Cx {
        ctx,
        scope,
        rev,
        set_rev,
        ref s,
        ref over,
        preset_mode,
        ..
    } = *cx;
    // The Aspect combo picks a family and lands on its size nearest the current height. The
    // Resolution combo is the D1 tri-state — Native, Match window (a virtual index 1, stored
    // as the `match_window` flag) — then that family's sizes.
    let family = aspect_of(s.width, s.height).unwrap_or(0);
    let aspect_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        ASPECTS.iter().map(|a| a.label.to_string()).collect(),
        family,
        |s, i| {
            s.match_window = false;
            (s.width, s.height) = nearest(i, s.height);
        },
    );
    // Native, Match window, the family's sizes, then Custom…, which shows Width and Height.
    // A size no family lists is Custom whatever the flag says.
    let sizes = ASPECTS[family].sizes;
    let custom_i = sizes.len() + 2;
    let custom =
        !s.match_window && s.width != 0 && (cx.custom_res || !sizes.contains(&(s.width, s.height)));
    let (res_names, res_i) = {
        let names: Vec<String> = ["Native display".to_string(), "Match window".to_string()]
            .into_iter()
            .chain(sizes.iter().map(|&(w, h)| format!("{w} \u{00D7} {h}")))
            .chain(["Custom\u{2026}".to_string()])
            .collect();
        let i = if s.match_window {
            1
        } else if custom {
            custom_i
        } else {
            sizes
                .iter()
                .position(|&(w, h)| w == s.width && h == s.height)
                .map_or(0, |i| i + 2)
        };
        (names, i)
    };
    let res_combo = {
        let set_custom = cx.set_custom_res.clone();
        setting_combo(ctx, scope, (rev, set_rev), res_names, res_i, move |s, i| {
            set_custom.call(i == custom_i);
            s.match_window = i == 1;
            (s.width, s.height) = match i {
                0 | 1 => (0, 0),
                // Custom starts from the size shown, or 1080p from Native.
                i if i == custom_i && s.width == 0 => (1920, 1080),
                i if i == custom_i => (s.width, s.height),
                i => sizes[i - 2],
            };
        })
    };
    // Each box writes its side through the shared rule, keeping the other side as stored.
    let size_box = |value: u32, min: u32, width: bool| {
        let (ctx, scope, set_rev) = (ctx.clone(), scope.to_string(), set_rev.clone());
        NumberBox::new(f64::from(value))
            .range(f64::from(min), 8192.0)
            .on_value_changed(move |v: f64| {
                commit(&ctx, &scope, (rev, &set_rev), |s| {
                    let typed = v.clamp(0.0, 8192.0) as u32;
                    let (w, h) = if width {
                        (typed, s.height)
                    } else {
                        (s.width, typed)
                    };
                    (s.width, s.height) = punktfunk_core::resolutions::custom(w, h, &s.codec);
                    s.match_window = false;
                });
            })
    };
    let res_control: Element = if custom {
        vstack((
            Element::from(res_combo),
            hstack((
                size_box(s.width, punktfunk_core::resolutions::MIN_WIDTH, true)
                    .header("Width")
                    .width(120.0),
                text_block("\u{00D7}").vertical_alignment(VerticalAlignment::Bottom),
                size_box(s.height, punktfunk_core::resolutions::MIN_HEIGHT, false)
                    .header("Height")
                    .width(120.0),
            ))
            .spacing(8.0),
        ))
        .spacing(8.0)
        .into()
    } else {
        res_combo.into()
    };
    let (hz_names, hz_i) = {
        let names: Vec<String> = REFRESH
            .iter()
            .map(|&r| {
                if r == 0 {
                    "Native".into()
                } else {
                    format!("{r} Hz")
                }
            })
            .collect();
        let i = REFRESH.iter().position(|&r| r == s.refresh_hz).unwrap_or(0);
        (names, i)
    };
    let hz_combo = setting_combo(ctx, scope, (rev, set_rev), hz_names, hz_i, |s, i| {
        s.refresh_hz = REFRESH[i];
    });
    let (scale_names, scale_i) = {
        let names: Vec<String> = RENDER_SCALES
            .iter()
            .map(|&x| render_scale_label(x))
            .collect();
        let i = RENDER_SCALES
            .iter()
            .position(|&x| (x - s.render_scale).abs() < 1e-6)
            .unwrap_or_else(|| RENDER_SCALES.iter().position(|&x| x == 1.0).unwrap());
        (names, i)
    };
    let scale_combo = setting_combo(ctx, scope, (rev, set_rev), scale_names, scale_i, |s, i| {
        s.render_scale = RENDER_SCALES[i];
    });
    let (comp_names, comp_i) = presets(COMPOSITORS, |v| *v == s.compositor);
    let comp_combo = setting_combo(ctx, scope, (rev, set_rev), comp_names, comp_i, |s, i| {
        s.compositor = COMPOSITORS[i].0.to_string();
    });
    // Migrated for the LOOKUP only (the store is left alone): a pre-M10 settings file
    // holds `vulkan`/`d3d11va`, which match no preset — the combo would show Automatic and
    // a save would silently rewrite the user's hardware preference to `auto`.
    let stored_decoder = pf_client_core::video::migrate_decoder_pref(&s.decoder);
    let (dec_names, dec_i) = presets(DECODERS, |v| *v == stored_decoder);
    let decoder_combo = setting_combo(ctx, scope, (rev, set_rev), dec_names, dec_i, |s, i| {
        s.decoder = DECODERS[i].0.to_string();
    });
    // GPU picker, only on a multi-GPU box (hybrid laptop, eGPU): which adapter decodes + presents.
    // Stored as the adapter description; empty = automatic (the window's monitor's adapter).
    let gpus = ctx.probes.lock().unwrap().gpus.clone();
    let gpu_combo = (gpus.len() > 1).then(|| {
        let mut names = vec!["Automatic (the display's GPU)".to_string()];
        names.extend(gpus.iter().cloned());
        let current = gpus
            .iter()
            .position(|n| *n == s.adapter)
            .map_or(0, |i| i + 1);
        let gpus = gpus.clone();
        setting_combo(ctx, scope, (rev, set_rev), names, current, move |s, i| {
            s.adapter = if i == 0 {
                String::new()
            } else {
                gpus[i - 1].clone()
            };
        })
    });
    let (codec_names, codec_i) = presets(CODECS, |v| *v == s.codec);
    let codec_combo = setting_combo(ctx, scope, (rev, set_rev), codec_names, codec_i, |s, i| {
        s.codec = CODECS[i].0.to_string();
    });
    // Free-form Mb/s (0 = host default) instead of presets, so a speed-test recommendation
    // round-trips exactly. Through `commit` like every other row: writing `ctx.settings`
    // directly here would edit the GLOBAL defaults from inside a preset scope (and record
    // no override, so the row could never say "Overridden here").
    let bitrate_box = {
        let (ctx, scope, set_rev) = (ctx.clone(), scope.to_string(), set_rev.clone());
        NumberBox::new(f64::from(s.bitrate_kbps) / 1000.0)
            .range(0.0, 3000.0)
            // PyroWave sets its own rate; the stored one stays for the other codecs.
            .enabled(s.codec != "pyrowave")
            .on_value_changed(move |v: f64| {
                commit(&ctx, &scope, (rev, &set_rev), |s| {
                    s.bitrate_kbps = (v.clamp(0.0, 3000.0) * 1000.0) as u32;
                });
            })
    };
    let hdr_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.hdr_enabled, |s, on| {
        s.hdr_enabled = on
    });
    let ten_bit_sdr_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.ten_bit_sdr, |s, on| {
        s.ten_bit_sdr = on
    });
    let chroma_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.enable_444, |s, on| {
        s.enable_444 = on
    });
    // Presentation intent (design/desktop-presentation-rebuild.md). The buffer row is
    // rendered only under Smoothness — `commit` bumps the revision, so flipping the
    // intent re-renders the section and the row appears/disappears with it.
    let (fit_names, fit_i) = presets(VIDEO_FITS, |v| *v == s.video_fit);
    let fit_combo = setting_combo(ctx, scope, (rev, set_rev), fit_names, fit_i, |s, i| {
        s.video_fit = VIDEO_FITS[i].0.to_string();
    });
    let (present_names, present_i) = presets(PRESENT_PRIORITIES, |v| *v == s.present_priority);
    let present_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        present_names,
        present_i,
        |s, i| s.present_priority = PRESENT_PRIORITIES[i].0.to_string(),
    );
    let smoothing = s.present_priority == "smooth";
    let (buffer_names, buffer_i) = presets(SMOOTH_BUFFERS, |v| *v == s.smooth_buffer);
    let buffer_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        buffer_names,
        buffer_i,
        |s, i| s.smooth_buffer = SMOOTH_BUFFERS[i].0,
    );
    let vsync_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.vsync, |s, on| s.vsync = on);
    let vrr_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.allow_vrr, |s, on| {
        s.allow_vrr = on
    });

    let mut out = group(
        Some("Resolution"),
        vec![
            described_labeled(
                "Aspect ratio",
                aspect_combo,
                "Which shapes the Resolution list offers. Picking one moves to its \
                 size nearest the current height.",
            ),
            described_overridable(
                (rev, set_rev),
                scope,
                "resolution",
                "Resolution",
                over.resolution,
                res_control,
                "The host drives a real virtual output at exactly this size \u{2014} true \
                 pixels, no scaling. \u{201C}Native display\u{201D} follows the monitor this \
                 window is on; \u{201C}Match window\u{201D} keeps the picture pixel-exact \
                 (1:1) through every resize.",
            ),
            described_overridable(
                (rev, set_rev),
                scope,
                "refresh_hz",
                "Refresh rate",
                over.refresh_hz,
                hz_combo,
                "\u{201C}Native\u{201D} resolves to this display\u{2019}s refresh rate at \
                 connect.",
            ),
        ],
        None,
    );
    out.extend(group(
        Some("Picture"),
        vec![
            described_overridable(
                (rev, set_rev),
                scope,
                "bitrate_kbps",
                "Bitrate (Mb/s, 0 = automatic)",
                over.bitrate_kbps,
                bitrate_box,
                if s.codec == "pyrowave" {
                    "PyroWave sets its own rate from the stream mode."
                } else {
                    "0 lets the host decide (its default, clamped to what it supports). A \
                     host card\u{2019}s context menu has a network speed test."
                },
            ),
            described_overridable(
                (rev, set_rev),
                scope,
                "video_fit",
                "Picture fit",
                over.video_fit,
                fit_combo,
                "When the stream's shape differs from the window. Fit shows the whole \
                 picture with black bars, Crop to fill cuts the edges off, Stretch to \
                 fill distorts it.",
            ),
            described_overridable(
                (rev, set_rev),
                scope,
                "hdr_enabled",
                "10-bit HDR",
                over.hdr_enabled,
                hdr_toggle,
                "HDR10, when the host has HDR content and this display supports it. \
                 With H.264 the stream stays SDR.",
            ),
            described_overridable(
                (rev, set_rev),
                scope,
                "present_priority",
                "Prioritize",
                over.present_priority,
                present_combo,
                "Lowest latency shows each frame the moment the display can take \
                 it \u{2014} a network hiccup becomes an occasional repeated or \
                 skipped frame. Smoothness buffers a little to even those out.",
            ),
        ],
        // The one form-level note, exactly as on Apple.
        Some("Display changes apply from the next session."),
    ));

    let d = Settings::default();
    let mut advanced = Vec::new();
    if smoothing {
        advanced.push(described_overridable(
            (rev, set_rev),
            scope,
            "smooth_buffer",
            "Smoothness buffer",
            over.smooth_buffer,
            buffer_combo,
            "Frames held back before showing. Each one absorbs about a refresh of \
             network hiccup and adds a refresh of delay. Automatic holds two.",
        ));
    }
    advanced.extend([
        described_overridable(
            (rev, set_rev),
            scope,
            "render_scale",
            "Render scale",
            over.render_scale,
            scale_combo,
            "Above native supersamples for sharpness; below renders lighter on the \
             host and the link. This device resamples the result to the window.",
        ),
        described_overridable(
            (rev, set_rev),
            scope,
            "codec",
            "Video codec",
            over.codec,
            codec_combo,
            "A preference \u{2014} the host falls back if it can\u{2019}t encode it. \
             PyroWave is the low-latency wavelet codec for a WIRED link: it trades \
             bitrate (hundreds of Mb/s) for near-zero decode time, so it wants \
             gigabit Ethernet.",
        ),
        // First sentence shared with the GTK client (its chroma_row); the constraint
        // sentence names the real gate (host: PyroWave || NVENC).
        described_overridable(
            (rev, set_rev),
            scope,
            "enable_444",
            "Full chroma (4:4:4)",
            over.enable_444,
            chroma_toggle,
            "Full-colour video: crisp small text and thin lines, at more bandwidth. \
             Requires an NVIDIA host (NVENC) or the PyroWave codec \u{2014} other \
             encoders stream 4:2:0.",
        ),
        described_overridable(
            (rev, set_rev),
            scope,
            "ten_bit_sdr",
            "10-bit SDR",
            over.ten_bit_sdr,
            ten_bit_sdr_toggle,
            "Smoother gradients without HDR \u{2014} the picture is encoded at 10-bit \
             precision. Needs an NVIDIA host; HDR takes over when it engages.",
        ),
        described_overridable(
            (rev, set_rev),
            scope,
            "vsync",
            "V-Sync",
            over.vsync,
            vsync_toggle,
            "Tear-free. Turning it off removes the wait for the screen\u{2019}s refresh \
             \u{2014} the lowest possible delay, at the cost of visible tearing. Not \
             every driver offers it; the stats overlay names the mode actually in use.",
        ),
        described_overridable(
            (rev, set_rev),
            scope,
            "allow_vrr",
            "Follow variable refresh",
            over.allow_vrr,
            vrr_toggle,
            "On a VRR/FreeSync/G-Sync screen, let the panel refresh in step with the \
             stream instead of on a fixed cadence. Applies to fullscreen sessions; \
             harmless on a fixed-refresh screen.",
        ),
        described_overridable(
            (rev, set_rev),
            scope,
            "compositor",
            "Host compositor",
            over.compositor,
            comp_combo,
            "The backend the host uses for its virtual output (Linux hosts only). A \
             specific choice falls back to auto-detection when that backend \
             isn\u{2019}t available.",
        ),
    ]);
    // Decoder and GPU are facts about THIS device's hardware — never per preset.
    if !preset_mode {
        advanced.push(described_labeled(
            "Video decoder",
            decoder_combo,
            "Automatic picks the hardware path this GPU does best \u{2014} Direct3D 11 on \
             Intel, Vulkan Video on NVIDIA and AMD \u{2014} and falls back to the CPU. \
             Change it only when debugging.",
        ));
        if let Some(c) = gpu_combo {
            advanced.push(described_labeled(
                "GPU",
                c,
                "Which adapter decodes and presents the stream. Automatic uses the GPU \
                 driving this window\u{2019}s display.",
            ));
        }
    }
    let changed = [
        smoothing && s.smooth_buffer != d.smooth_buffer,
        s.render_scale != d.render_scale,
        s.codec != d.codec,
        s.enable_444 != d.enable_444,
        s.ten_bit_sdr != d.ten_bit_sdr,
        s.vsync != d.vsync,
        s.allow_vrr != d.allow_vrr,
        s.compositor != d.compositor,
        stored_decoder != d.decoder,
        !s.adapter.is_empty(),
    ];
    let overridden = over.smooth_buffer
        || over.render_scale
        || over.codec
        || over.enable_444
        || over.ten_bit_sdr
        || over.vsync
        || over.allow_vrr
        || over.compositor;
    out.extend(advanced_group(
        cx,
        advanced,
        changed.into_iter().filter(|c| *c).count(),
        overridden,
    ));
    out
}

/// Input: touch, keyboard and mouse.
fn input_section(cx: &Cx) -> Vec<Element> {
    let Cx {
        ctx,
        scope,
        rev,
        set_rev,
        ref s,
        ref over,
        ..
    } = *cx;
    let (touch_names, touch_i) = presets(TOUCH_MODES, |v| *v == s.touch_mode);
    let touch_combo = setting_combo(ctx, scope, (rev, set_rev), touch_names, touch_i, |s, i| {
        s.touch_mode = TOUCH_MODES[i].0.to_string();
    });
    let (mouse_names, mouse_i) = presets(MOUSE_MODES, |v| *v == s.mouse_mode);
    let mouse_combo = setting_combo(ctx, scope, (rev, set_rev), mouse_names, mouse_i, |s, i| {
        s.mouse_mode = MOUSE_MODES[i].0.to_string();
    });
    let invert_scroll_toggle =
        setting_toggle(ctx, scope, (rev, set_rev), s.invert_scroll, |s, on| {
            s.invert_scroll = on
        });
    let shortcuts_toggle =
        setting_toggle(ctx, scope, (rev, set_rev), s.inhibit_shortcuts, |s, on| {
            s.inhibit_shortcuts = on
        });

    let mut out = group(
        Some("Touch & pointer"),
        vec![described_overridable(
            (rev, set_rev),
            scope,
            "touch_mode",
            "Touch input",
            over.touch_mode,
            touch_combo,
            "How a touchscreen drives the host: Trackpad moves the host cursor like a \
             laptop trackpad (tap to click), Direct pointer jumps the cursor to wherever \
             you touch, Touch passthrough sends real multi-touch through.",
        )],
        None,
    );
    out.extend(group(
        Some("Keyboard & mouse"),
        vec![
            described_overridable(
                (rev, set_rev),
                scope,
                "mouse_mode",
                "Mouse input",
                over.mouse_mode,
                mouse_combo,
                "Capture locks the pointer to the stream and sends relative motion — \
                 best for games. Desktop leaves the pointer free to enter and leave \
                 the stream and sends absolute positions — best for remote desktop \
                 work. Ctrl+Alt+Shift+M switches live.",
            ),
            described_overridable(
                (rev, set_rev),
                scope,
                "inhibit_shortcuts",
                "Capture system shortcuts",
                over.inhibit_shortcuts,
                shortcuts_toggle,
                "Alt+Tab, the Windows key and friends reach the host while the stream \
                 has input captured. Off, they act on this machine instead.",
            ),
            described_overridable(
                (rev, set_rev),
                scope,
                "invert_scroll",
                "Invert scroll direction",
                over.invert_scroll,
                invert_scroll_toggle,
                "Reverses the wheel and trackpad scroll direction sent to the host.",
            ),
        ],
        None,
    ));
    out
}

/// The quick-action ring, edited on the ring itself (design touch-client-overlay.md
/// §3.3). The editor commits every edit itself; the override marker is the page's.
fn quick_actions_section(cx: &Cx) -> Vec<Element> {
    let Cx {
        ctx,
        scope,
        rev,
        set_rev,
        ref over,
        ..
    } = *cx;
    vec![described_overridable(
        (rev, set_rev),
        scope,
        "overlay_actions",
        "Quick actions",
        over.overlay_actions,
        component(
            super::quick_actions::quick_actions_section,
            super::quick_actions::Props {
                ctx: ctx.clone(),
                scope: scope.to_string(),
                rev,
                set_rev: set_rev.clone(),
            },
        ),
        "The dial Ctrl+Alt+Shift+O, a two-finger twist or Select+A opens in a stream: what \
         its six buttons hold, and the shortcut chords they can send. A preset that \
         changes it owns the whole dial.",
    )]
}

/// Controllers: the pad inventory, forwarding and the emulated pad.
fn controllers_section(cx: &Cx) -> Vec<Element> {
    let Cx {
        ctx,
        scope,
        rev,
        set_rev,
        ref s,
        ref over,
        preset_mode,
        ..
    } = *cx;
    // Controller forwarding: Automatic forwards EVERY real controller, each as its own pad;
    // pinning one restricts the session to that single controller (single-player). Persisted
    // by stable key (`Settings::forward_pad`, GTK parity) so the pin survives restarts AND
    // reaches the spawned session binary, whose service applies the same key.
    let pads = ctx.gamepad.pads();
    let (fwd_names, fwd_i) = {
        let mut names = vec!["Automatic (all controllers)".to_string()];
        names.extend(pads.iter().map(|p| {
            let kind = p.kind_label();
            if kind.is_empty() {
                p.name.clone()
            } else {
                format!("{} \u{00B7} {kind}", p.name)
            }
        }));
        let i = (!s.forward_pad.is_empty())
            .then(|| pads.iter().position(|p| p.key == s.forward_pad))
            .flatten()
            .map_or(0, |i| i + 1);
        (names, i)
    };
    let forward_combo = {
        let svc = ctx.gamepad.clone();
        let ctx2 = ctx.clone();
        let keys: Vec<String> = pads.iter().map(|p| p.key.clone()).collect();
        ComboBox::new(fwd_names)
            .selected_index(fwd_i as i32)
            .on_selection_changed(move |i: i32| {
                // -1 is "nothing selected" (a pad list rebuilt in place): not a pick.
                let Ok(sel) = usize::try_from(i) else {
                    return;
                };
                let key = if sel == 0 {
                    None
                } else {
                    keys.get(sel - 1).cloned()
                };
                // Apply live to the gamepad service and persist — the spawned session
                // reads `forward_pad` at connect. Rebase on the file first (the same
                // discipline as `commit()`): this handler bypasses commit and a stale
                // whole-struct save would revert other writers.
                svc.set_pinned(key.clone());
                let mut s = ctx2.settings.lock().unwrap();
                *s = Settings::load();
                s.forward_pad = key.unwrap_or_default();
                s.save();
            })
            // Dimmed with the master switch above it, like echo cancellation under the mic:
            // this and the three below have nothing to act on while no controller is
            // forwarded. Every commit bumps `rev` and re-renders, so they follow it live.
            .enabled(s.gamepad_forwarding)
    };
    let pad_forward_toggle =
        setting_toggle(ctx, scope, (rev, set_rev), s.gamepad_forwarding, |s, on| {
            s.gamepad_forwarding = on
        });
    // The two DualSense pad-audio rows, GTK parity. The session binary this shell spawns has
    // honoured both all along; only the rows were missing here. Global scope only, like GTK's:
    // no override marker exists for either, so a preset-scope toggle would be discarded.
    let pad_haptics_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.pad_haptics, |s, on| {
        s.pad_haptics = on
    });
    let pad_rumble_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.pad_rumble, |s, on| {
        s.pad_rumble = on
    })
    .enabled(s.gamepad_forwarding);
    let pad_speaker_toggle = setting_toggle(
        ctx,
        scope,
        (rev, set_rev),
        pf_client_core::pad_audio::speaker_active(&s.pad_speaker),
        |s, on| s.pad_speaker = if on { "pad".into() } else { "off".into() },
    );
    let (pad_names, pad_i) = presets(GAMEPADS, |v| {
        GamepadPref::from_name(v) == GamepadPref::from_name(&s.gamepad)
    });
    let pad_combo = setting_combo(ctx, scope, (rev, set_rev), pad_names, pad_i, |s, i| {
        s.gamepad = GAMEPADS[i].0.to_string();
    })
    .enabled(s.gamepad_forwarding);
    let (sysbtn_names, sysbtn_i) = presets(SYSTEM_BUTTONS, |v| *v == s.system_buttons);
    let sysbtn_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        sysbtn_names,
        sysbtn_i,
        |s, i| {
            s.system_buttons = SYSTEM_BUTTONS[i].0.to_string();
        },
    )
    .enabled(s.gamepad_forwarding);
    let (gesture_names, gesture_i) = presets(GUIDE_GESTURES, |v| *v == s.guide_gesture);
    let gesture_combo = setting_combo(
        ctx,
        scope,
        (rev, set_rev),
        gesture_names,
        gesture_i,
        |s, i| {
            s.guide_gesture = GUIDE_GESTURES[i].0.to_string();
        },
    )
    .enabled(s.gamepad_forwarding);

    let mut out = group(
        None,
        [
            // The read-only pad inventory (GTK parity): what THIS device sees right
            // now — the fastest answer to "is my controller even detected?". A
            // device fact, so defaults scope only, like the forward picker below.
            (!preset_mode).then(|| {
                let inventory: Element = if pads.is_empty() {
                    text_block("No controllers detected")
                        .font_size(12.0)
                        .foreground(ThemeRef::SecondaryText)
                        .into()
                } else {
                    vstack(
                        pads.iter()
                            .map(|p| {
                                let sub = if p.steam_virtual {
                                    "Steam Input's virtual pad \u{2014} Automatic skips \
                                     it while a real pad is connected"
                                        .to_string()
                                } else {
                                    p.kind_label().to_string()
                                };
                                vstack((
                                    text_block(p.name.clone()).semibold(),
                                    text_block(sub)
                                        .font_size(11.0)
                                        .foreground(ThemeRef::SecondaryText),
                                ))
                                .spacing(1.0)
                                .into()
                            })
                            .collect::<Vec<Element>>(),
                    )
                    .spacing(8.0)
                    .into()
                };
                described_labeled(
                    "Detected controllers",
                    inventory,
                    "Plug in or pair a controller and it appears here.",
                )
            }),
            Some(described_overridable(
                (rev, set_rev),
                scope,
                "gamepad",
                "Controller type",
                over.gamepad,
                pad_combo,
                "The virtual pad created on the host. Automatic matches your controller \
                 \u{2014} a DualSense keeps adaptive triggers, lightbar, touchpad and \
                 motion.",
            )),
            // This device's motors, so defaults scope only, like Controller haptics.
            (!preset_mode).then(|| {
                described_labeled(
                    "Controller rumble",
                    pad_rumble_toggle,
                    "Off, controllers don't vibrate from the stream or in the menus, whatever the game sends.",
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect(),
        Some("Applies from the next session."),
    );

    let d = Settings::default();
    let mut advanced = vec![
        // Whether ANY controller is forwarded — presetable, so it renders in both scopes
        // (a "Work" preset can decline what "Game" forwards).
        described_overridable(
            (rev, set_rev),
            scope,
            "gamepad_forwarding",
            "Forward controllers",
            over.gamepad_forwarding,
            pad_forward_toggle,
            "Sends controllers connected to this PC to the host. Turn it off when your \
             controller already reaches the host another way \u{2014} USB passthrough \
             such as VirtualHere, or a pad plugged into the host itself \u{2014} so games \
             don't see two of them. Off, this PC never opens the controller at all, which \
             is what leaves it free for a passthrough tool to claim.",
        ),
    ];
    // Which physical pad this device forwards is a device fact (tier G): defaults scope
    // only. Apple forwards ONE pad as player 1; this client forwards each as its own player.
    if !preset_mode {
        advanced.push(described_labeled(
            "Use controller",
            forward_combo,
            "Every connected controller is forwarded, each as its own player. Pick one to \
             force single-player \u{2014} only it reaches the host.",
        ));
    }
    advanced.extend([
        described_overridable(
            (rev, set_rev),
            scope,
            "system_buttons",
            "Guide button",
            over.system_buttons,
            sysbtn_combo,
            "Where the guide (Xbox/PS) and quick-access presses go while streaming. \
             Automatic sends them to the host \u{2014} except on devices whose own overlay \
             reacts to the same press (Gaming Mode), where they stay local and the gesture \
             below reaches the host.",
        ),
        described_overridable(
            (rev, set_rev),
            scope,
            "guide_gesture",
            "Hold Select for guide",
            over.guide_gesture,
            gesture_combo,
            "Hold Select on its own to press the host's guide button \u{2014} keep holding \
             for a Gaming-Mode host's quick-access menu. A Select tap still goes through, \
             slightly delayed. Automatic arms it only where the real button can't reach \
             the host.",
        ),
    ]);
    if !preset_mode {
        advanced.push(described_labeled(
            "Controller haptics",
            pad_haptics_toggle,
            "Play a DualSense's voice-coil haptics on the pad itself. Wired pads only, and \
             only while controllers are forwarded.",
        ));
        advanced.push(described_labeled(
            "Controller speaker",
            pad_speaker_toggle,
            "Play the audio a game sends to the pad's own speaker on the pad, not through \
             this PC.",
        ));
    }
    let changed = [
        s.gamepad_forwarding != d.gamepad_forwarding,
        !s.forward_pad.is_empty(),
        s.system_buttons != d.system_buttons,
        s.guide_gesture != d.guide_gesture,
        s.pad_haptics != d.pad_haptics,
        s.pad_speaker != d.pad_speaker,
    ];
    out.extend(advanced_group(
        cx,
        advanced,
        changed.into_iter().filter(|c| *c).count(),
        over.gamepad_forwarding || over.system_buttons || over.guide_gesture,
    ));
    out
}

/// Audio: channels, format, host audio, microphone and this device's endpoints.
fn audio_section(cx: &Cx) -> Vec<Element> {
    let Cx {
        ctx,
        scope,
        rev,
        set_rev,
        ref s,
        ref over,
        preset_mode,
        ..
    } = *cx;
    let (ac_names, ac_i) = presets(AUDIO_CHANNELS, |v| *v == s.audio_channels);
    let channels_combo = setting_combo(ctx, scope, (rev, set_rev), ac_names, ac_i, |s, i| {
        s.audio_channels = AUDIO_CHANNELS[i].0;
    });
    // The lossless-audio opt-in. An unknown stored value (a newer client's row, arriving through a
    // shared preset) shows as Opus — which is what the session resolves it to as well, so the row
    // and the wire agree rather than the combo silently rewriting the user's choice on save.
    let (af_names, af_i) = presets(AUDIO_FORMATS, |v| *v == s.audio_format);
    let format_combo = setting_combo(ctx, scope, (rev, set_rev), af_names, af_i, |s, i| {
        s.audio_format = AUDIO_FORMATS[i].0.to_string();
    });
    let keep_host_audio_toggle =
        setting_toggle(ctx, scope, (rev, set_rev), s.keep_host_audio, |s, on| {
            s.keep_host_audio = on
        });
    let mic_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.mic_enabled, |s, on| {
        s.mic_enabled = on
    });
    // Endpoint pickers (the WASAPI probe — the GTK client's PipeWire twins): visible
    // labels are friendly names, the stored value is the endpoint id. Hidden when the
    // probe found at most the default; a saved device that's gone keeps a revertable
    // "(not detected)" entry, like the GPU row. Device facts — defaults scope only, probed
    // once per visit (`refresh_snapshot`).
    let (speakers, mics) = {
        let p = ctx.probes.lock().unwrap();
        (p.speakers.clone(), p.mics.clone())
    };
    let dev_combo = |saved: &str,
                     devs: &[pf_client_core::audio::AudioDevice],
                     apply: fn(&mut Settings, String)| {
        let mut names = vec!["System default".to_string()];
        let mut keys = vec![String::new()];
        for d in devs {
            names.push(d.description.clone());
            keys.push(d.name.clone());
        }
        if !saved.is_empty() && !keys.iter().any(|k| k == saved) {
            names.push(format!("{saved} (not detected)"));
            keys.push(saved.to_string());
        }
        (keys.len() > 1).then(|| {
            let current = keys.iter().position(|k| k == saved).unwrap_or(0);
            setting_combo(ctx, scope, (rev, set_rev), names, current, move |s, i| {
                apply(s, keys[i.min(keys.len() - 1)].clone());
            })
        })
    };
    let speaker_combo = dev_combo(&s.speaker_device, &speakers, |s, v| s.speaker_device = v);
    let mic_dev_combo = dev_combo(&s.mic_device, &mics, |s, v| s.mic_device = v);
    // Echo cancellation is meaningless without an uplink, so it greys out with the mic above
    // it. Every commit bumps `rev` and re-renders this screen, so the two stay in step live.
    let echo_toggle = setting_toggle(ctx, scope, (rev, set_rev), s.echo_cancel, |s, on| {
        s.echo_cancel = on
    })
    .enabled(s.mic_enabled);

    let mut out = group(
        None,
        [
            Some(described_overridable(
                (rev, set_rev),
                scope,
                "audio_channels",
                "Audio channels",
                over.audio_channels,
                channels_combo,
                "The speaker layout requested from the host. It downmixes if its own \
                 output has fewer channels.",
            )),
            // The endpoint picks are facts about THIS device's hardware — never
            // per preset, like Decoder/GPU.
            (!preset_mode)
                .then(|| {
                    speaker_combo.map(|c| {
                        described_labeled(
                            "Speaker",
                            c,
                            "Host audio plays here \u{2014} System default follows \
                             the Windows output device.",
                        )
                    })
                })
                .flatten(),
            Some(described_overridable(
                (rev, set_rev),
                scope,
                "mic_enabled",
                "Stream microphone",
                over.mic_enabled,
                mic_toggle,
                "This device\u{2019}s microphone feeds the host\u{2019}s virtual mic. \
                 Ctrl+Alt+Shift+V mutes and unmutes it during a stream.",
            )),
            (!preset_mode)
                .then(|| {
                    mic_dev_combo.map(|c| {
                        described_labeled(
                            "Microphone",
                            c,
                            "The input that feeds the host\u{2019}s virtual mic.",
                        )
                    })
                })
                .flatten(),
        ]
        .into_iter()
        .flatten()
        .collect(),
        Some("Applies from the next session."),
    );

    let d = Settings::default();
    let stereo = s.audio_channels == 2;
    let mut advanced = Vec::new();
    // Stereo-only, so HIDDEN under 5.1/7.1: a lossless surround frame does not fit one
    // QUIC datagram at the default MTU (design/hi-res-audio.md §4.2).
    if stereo {
        advanced.push(described_overridable(
            (rev, set_rev),
            scope,
            "audio_format",
            "Audio quality",
            over.audio_format,
            format_combo,
            "Lossless sends uncompressed PCM instead of Opus \u{2014} bit-exact, at \
             2.3\u{2013}4.6 Mb/s taken off the top of the link and outside the \
             automatic-bitrate loop. The host has its own switch, off by default, and \
             quietly stays on Opus if it can\u{2019}t deliver the rate; the stats overlay \
             names what the session actually got.",
        ));
    }
    advanced.extend([
        described_overridable(
            (rev, set_rev),
            scope,
            "keep_host_audio",
            "Keep host audio playing",
            over.keep_host_audio,
            keep_host_audio_toggle,
            "The host\u{2019}s own speakers or headphones keep playing while you stream \
             \u{2014} both ends hear the same audio. Needs a host on 0.32 or newer.",
        ),
        described_overridable(
            (rev, set_rev),
            scope,
            "echo_cancel",
            "Echo cancellation",
            over.echo_cancel,
            echo_toggle,
            "Keeps the host\u{2019}s audio, playing from this machine\u{2019}s speakers, \
             from being picked up and sent straight back. Turn it off if your microphone \
             already does its own processing.",
        ),
    ]);
    let changed = [
        stereo && s.audio_format != d.audio_format,
        s.keep_host_audio != d.keep_host_audio,
        s.echo_cancel != d.echo_cancel,
    ];
    out.extend(advanced_group(
        cx,
        advanced,
        changed.into_iter().filter(|c| *c).count(),
        over.audio_format || over.keep_host_audio || over.echo_cancel,
    ));
    out
}

/// About: identity and version, the log folder, licenses.
fn about_section(set_screen: &AsyncSetState<Screen>) -> Vec<Element> {
    let licenses_button = {
        let ss = set_screen.clone();
        button("Third-party licenses").on_click(move || ss.call(Screen::Licenses))
    };
    // The client log's folder, so the rotated `.old` generation is in reach too. `real_dir`,
    // not the literal %LOCALAPPDATA% path: Explorer lives outside the MSIX container and opens
    // Documents for a redirected path. The `is_dir` guard keeps that fallback unreachable; a
    // failed spawn stays silent.
    let logs_button = button("Open log folder").on_click(|| {
        if let Some(dir) = crate::logfile::real_dir().filter(|d| d.is_dir()) {
            let _ = std::process::Command::new("explorer.exe").arg(&dir).spawn();
        }
    });
    // App identity + version at the top of the About card (the WinUI Settings convention).
    // CARGO_PKG_VERSION is the workspace version, baked in at compile time.
    let about_identity = vstack((
        text_block("Punktfunk").font_size(20.0).semibold(),
        text_block(concat!("Version ", env!("CARGO_PKG_VERSION")))
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
    ))
    .spacing(2.0);

    group(
        None,
        vec![
            about_identity.into(),
            described_labeled(
                "Diagnostics",
                logs_button,
                "The client log (client.log, plus the session\u{2019}s whole \
                 receive/decode/present trail) \u{2014} attach it to a bug report.",
            ),
            licenses_button.into(),
        ],
        None,
    )
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
        "display" => ("Display", display_section(&cx)),
        "input" => ("Input", input_section(&cx)),
        "quick_actions" => ("Quick actions", quick_actions_section(&cx)),
        "controllers" => ("Controllers", controllers_section(&cx)),
        "audio" => ("Audio", audio_section(&cx)),
        "about" => ("About", about_section(set_screen)),
        // "general" and anything unrecognized.
        _ => ("General", general_section(&cx)),
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
    // The card is keyed by section, so a pane switch remounts it (a reused ComboBox loses its
    // selection). The content column carries the section entrance and the category title, so
    // the title shares the cards' left edge; no max-width, as the pane already takes a third.
    // The scope switcher is one native DropDownButton in a bar above the NavigationView.
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
    let scope_bar: Element = {
        let scope_label = match &active {
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
            if let Some(p) = &active {
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
        if let Some(c) = active
            .as_ref()
            .and_then(|p| p.accent.as_deref())
            .and_then(hex_color)
        {
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
    };

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
    // The keyed column MUST sit inside a panel's child list, not directly under the
    // scroll_view: `ScrollView::children()` is `Children::PositionalSingle`, which
    // reconciles its one child POSITIONALLY and ignores keys outright. Keyed straight onto
    // the scroll_view's child, the section switch silently diffs one section's controls into
    // another's — which re-sets each reused ComboBox's items (clearing WinUI's selection)
    // but skips `selected_index` whenever the two sections' values compare equal, so the
    // combos render blank until touched. A panel (vstack) takes the keyed path, so the key
    // remounts the whole column and every prop is applied fresh.
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
    // The delete confirmation. Declarative like every dialog in this shell — but ALWAYS
    // MOUNTED, with `is_open` doing the arming: a ContentDialog is a "phantom" child in the
    // reactor backend (tracked logically, never attached to the panel), and unmounting one
    // destroys its handle before `remove_child` runs, so the backend stops recognising it
    // as phantom and RemoveAt()s a visual child that does not exist — E_BOUNDS, main-thread
    // panic ("Daten außerhalb des gültigen Bereichs"), reliably on every delete. A mounted
    // dialog is never removed, so the bug has nothing to bite. (Upstream report material —
    // the third windows-reactor bug this client documents.)
    let confirm: Element = {
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
    };
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
    // Overlay layers fill the NAV's cell (grids stretch children; a vstack would hand the
    // NavigationView its desired height — clipped short, floating tall). The layer list is
    // STABLE — always [nav, sheet slot, dialog] — so no pass ever removes a grid child:
    // removals are where the reconciler's phantom-dialog bookkeeping breaks (see `confirm`
    // above), and a closed sheet leaves a same-kind, background-less Border in its slot
    // (invisible, and per style.rs a null background is not hit-testable, so it swallows
    // no clicks).
    let sheet_slot: Element = if edit_open && preset_mode {
        // The preset sheet — "Edit preset…" in the bar. The bar owns the scope choice,
        // so the sheet carries only the preset being edited.
        edit_preset_modal(
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
