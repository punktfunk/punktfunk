//! Input: touch, keyboard and mouse, and the quick-action ring.

use super::{described_overridable, group, presets, rows, setting_combo, setting_toggle, Cx};
use pf_client_core::settings::{MouseMode, TouchMode};
use pf_l10n::{client, desktop};
use windows_reactor::*;

/// Input: touch, keyboard and mouse.
pub(super) fn input_section(cx: &Cx) -> Vec<View> {
    let Cx { scope, ref s, .. } = *cx;
    // How a touchscreen's fingers drive the host; only meaningful on a touchscreen device.
    let touch_modes: Vec<(&str, &str)> = TouchMode::ALL
        .iter()
        .map(|m| (m.as_name(), m.label()))
        .collect();
    let (touch_names, touch_i) = presets(&touch_modes, |v| *v == s.touch_mode);
    let touch_combo = setting_combo(cx, scope, "touch_mode", touch_names, touch_i, |s, i| {
        s.touch_mode = TouchMode::ALL[i].as_name().to_string();
    });
    // Capture (pointer lock, relative, for games) vs desktop (uncaptured absolute pointer, for
    // remote desktop work); Ctrl+Alt+Shift+M flips the model live in-stream.
    let mouse_modes: Vec<(&str, &str)> = MouseMode::ALL
        .iter()
        .map(|m| (m.as_name(), m.label()))
        .collect();
    let (mouse_names, mouse_i) = presets(&mouse_modes, |v| *v == s.mouse_mode);
    let mouse_combo = setting_combo(cx, scope, "mouse_mode", mouse_names, mouse_i, |s, i| {
        s.mouse_mode = MouseMode::ALL[i].as_name().to_string();
    });
    let invert_scroll_toggle =
        setting_toggle(cx, scope, "invert_scroll", s.invert_scroll, |s, on| {
            s.invert_scroll = on
        });
    let shortcuts_toggle = setting_toggle(
        cx,
        scope,
        "inhibit_shortcuts",
        s.inhibit_shortcuts,
        |s, on| s.inhibit_shortcuts = on,
    );

    let mut out = group(
        cx.pal,
        Some(client::prefs_group_touch_pointer()),
        vec![described_overridable(
            cx,
            &rows::TOUCH,
            touch_combo,
            client::player_touch_mode_hint(),
        )],
        None,
    );
    out.extend(group(
        cx.pal,
        Some(client::prefs_group_keyboard_mouse()),
        vec![
            described_overridable(
                cx,
                &rows::MOUSE,
                mouse_combo,
                &format!(
                    "{} {}",
                    client::player_mouse_mode_hint(),
                    client::player_mouse_mode_keys_hint("Ctrl+Alt+Shift+M"),
                ),
            ),
            described_overridable(
                cx,
                &rows::SHORTCUTS,
                shortcuts_toggle,
                &client::player_inhibit_shortcuts_hint("Alt+Tab, Win, \u{2026}"),
            ),
            described_overridable(
                cx,
                &rows::INVERT_SCROLL,
                invert_scroll_toggle,
                client::player_invert_scroll_hint(),
            ),
        ],
        None,
    ));
    out
}

/// The quick-action ring, edited on the ring itself (design touch-client-overlay.md
/// §3.3). The editor commits every edit itself; the override marker is the page's.
pub(super) fn quick_actions_section(cx: &Cx) -> Vec<View> {
    let Cx {
        ctx,
        scope,
        rev,
        set_rev,
        set_status,
        ..
    } = *cx;
    vec![described_overridable(
        cx,
        &rows::QUICK_ACTIONS,
        component::<crate::app::quick_actions::QuickActions>(
            "quick-actions",
            crate::app::quick_actions::Props {
                ctx: ctx.clone(),
                pal: *cx.pal,
                scope: scope.to_string(),
                rev,
                set_rev: set_rev.clone(),
                set_status: set_status.clone(),
            },
        ),
        &format!(
            "{} {}",
            desktop::desktop_player_overlay_actions_hint("Ctrl+Alt+Shift+O", "Select+A"),
            desktop::desktop_player_overlay_actions_preset_hint(),
        ),
    )]
}
