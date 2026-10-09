//! Input: touch, keyboard and mouse, and the quick-action ring.

use super::{described_overridable, group, presets, setting_combo, setting_toggle, Cx};
use windows_reactor::*;

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

/// Input: touch, keyboard and mouse.
pub(super) fn input_section(cx: &Cx) -> Vec<Element> {
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
pub(super) fn quick_actions_section(cx: &Cx) -> Vec<Element> {
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
            crate::app::quick_actions::quick_actions_section,
            crate::app::quick_actions::Props {
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
