//! Input: touch, keyboard and mouse, the quick-action ring, and the in-stream shortcuts.

use super::{described_overridable, group, presets, rows, setting_combo, setting_toggle, Cx};
use crate::app::quick_actions::{Props, QuickActions};
use crate::app::shortcuts::{resolved, ring_keys, ShortcutsPage};
use pf_client_core::overlay_actions::SlotId;
use pf_client_core::settings::{MouseMode, TouchMode};
use pf_client_core::shortcuts::{Action, Device};
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
    // remote desktop work); the Mouse mode shortcut flips the model live in-stream.
    let mouse_modes: Vec<(&str, &str)> = MouseMode::ALL
        .iter()
        .map(|m| (m.as_name(), m.label()))
        .collect();
    let (mouse_names, mouse_i) = presets(&mouse_modes, |v| *v == s.mouse_mode);
    let mouse_combo = setting_combo(cx, scope, "mouse_mode", mouse_names, mouse_i, |s, i| {
        s.mouse_mode = MouseMode::ALL[i].as_name().to_string();
    });
    let mouse_keys = resolved(s)
        .hint(&Action::Slot(SlotId::MouseMode), Device::Keyboard)
        .map(|keys| client::player_mouse_mode_keys_hint(&keys));
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
                &[client::player_mouse_mode_hint().to_string()]
                    .into_iter()
                    .chain(mouse_keys)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            described_overridable(
                cx,
                &rows::INHIBIT_SHORTCUTS,
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

/// What the ring and shortcut editors take from the page: the scope and its commit plumbing.
fn editor_props(cx: &Cx) -> Props {
    Props {
        ctx: cx.ctx.clone(),
        pal: *cx.pal,
        scope: cx.scope.to_string(),
        rev: cx.rev,
        set_rev: cx.set_rev.clone(),
        set_status: cx.set_status.clone(),
    }
}

/// The quick-action ring, edited on the ring itself (design touch-client-overlay.md
/// §3.3). The editor commits every edit itself; the override marker is the page's. The
/// caption names the scope's Quick actions shortcuts while it has both.
pub(super) fn quick_actions_section(cx: &Cx) -> Vec<View> {
    let opens = ring_keys(&resolved(&cx.s))
        .map(|(keys, pad)| desktop::desktop_player_overlay_actions_hint(&keys, &pad));
    vec![described_overridable(
        cx,
        &rows::QUICK_ACTIONS,
        component::<QuickActions>("quick-actions", editor_props(cx)),
        &opens
            .into_iter()
            .chain([desktop::desktop_player_overlay_actions_preset_hint().to_string()])
            .collect::<Vec<_>>()
            .join(" "),
    )]
}

/// The in-stream shortcuts (design client-shortcuts.md §5). The editor commits every edit
/// itself; the override marker is the page's.
pub(super) fn shortcuts_section(cx: &Cx) -> Vec<View> {
    vec![described_overridable(
        cx,
        &rows::SHORTCUTS,
        component::<ShortcutsPage>("shortcuts", editor_props(cx)),
        client::player_shortcuts_hint(),
    )]
}
