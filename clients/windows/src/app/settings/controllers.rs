//! Controllers: the pad inventory, forwarding and the emulated pad.

use super::{
    advanced_group, described_labeled, described_overridable, group, labeled, presets, rows,
    setting_combo, setting_toggle, Cx,
};
use crate::app::style::*;
use crate::trust::Settings;
use pf_client_core::settings::Label;
use pf_fluent::Token;
use pf_l10n::{client, desktop};
use punktfunk_core::config::GamepadPref;
use windows_reactor::*;

/// Virtual-pad presets: `(stored value, display label)` — the pad the HOST creates. Same set the
/// GTK client offers; "Automatic" resolves from the physical controller at connect.
const GAMEPADS: &[(&str, Label)] = &[
    ("auto", client::player_gamepad_auto),
    ("xbox360", || "Xbox 360"),
    ("dualsense", || "DualSense"),
    ("xboxone", || "Xbox One"),
    ("dualshock4", || "DualShock 4"),
    // Kept in lockstep with the GTK picker: this row was missing here, so a Windows
    // user could not ask the host for the Deck-shaped pad (trackpads, back grips).
    ("steamdeck", || "Steam Deck"),
    ("steamcontroller2", || "Steam Controller 2"),
];
/// System-button routing: `(stored value, display label)` — where the guide (Xbox/PS)
/// and quick-access presses land while streaming. The cross-client `system_buttons` key;
/// Automatic forwards on desktop and stays local under Gaming Mode.
const SYSTEM_BUTTONS: &[(&str, Label)] = &[
    ("auto", client::player_system_buttons_auto),
    ("forward", client::player_system_buttons_forward),
    ("local", client::player_system_buttons_local),
];

/// Controllers: the pad inventory, forwarding and the emulated pad.
pub(super) fn controllers_section(cx: &Cx) -> Vec<View> {
    let Cx {
        ctx,
        scope,
        ref s,
        preset_mode,
        ..
    } = *cx;
    // Controller forwarding: Automatic forwards EVERY real controller, each as its own pad;
    // pinning one restricts the session to that single controller (single-player). Persisted
    // by stable key (`Settings::forward_pad`, GTK parity) so the pin survives restarts AND
    // reaches the spawned session binary, whose service applies the same key.
    let pads = ctx.gamepad.pads();
    let (fwd_names, fwd_i) = {
        let mut names = vec![client::player_forward_pad_auto().to_string()];
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
        ComboBox::new()
            .items_source(fwd_names)
            .selected_index(fwd_i)
            .on_selection_changed(move |i: Option<usize>| {
                // None is "nothing selected" (a pad list rebuilt in place): not a pick.
                let Some(sel) = i else {
                    return;
                };
                let key = if sel == 0 {
                    None
                } else {
                    keys.get(sel - 1).cloned()
                };
                // Apply live to the gamepad service and persist: the spawned session reads
                // `forward_pad` at connect. This handler bypasses `commit()`, so it edits the
                // file under its lock the same way.
                svc.set_pinned(key.clone());
                let mut s = ctx2.settings.lock().unwrap();
                *s = Settings::update(|file| {
                    file.forward_pad = key.unwrap_or_default();
                    true
                });
            })
            // Dimmed with the master switch above it, like echo cancellation under the mic:
            // this and the three below have nothing to act on while no controller is
            // forwarded. Every commit bumps `rev` and re-renders, so they follow it live.
            .is_enabled(s.gamepad_forwarding)
    };
    let pad_forward_toggle = setting_toggle(
        cx,
        scope,
        "gamepad_forwarding",
        s.gamepad_forwarding,
        |s, on| s.gamepad_forwarding = on,
    );
    // The two DualSense pad-audio rows, GTK parity. The session binary this shell spawns has
    // honoured both all along; only the rows were missing here. Global scope only, like GTK's:
    // no override marker exists for either, so a preset-scope toggle would be discarded.
    let pad_haptics_toggle = setting_toggle(cx, scope, "pad_haptics", s.pad_haptics, |s, on| {
        s.pad_haptics = on
    });
    let pad_rumble_toggle = setting_toggle(cx, scope, "pad_rumble", s.pad_rumble, |s, on| {
        s.pad_rumble = on
    })
    .is_enabled(s.gamepad_forwarding);
    let pad_speaker_toggle = setting_toggle(
        cx,
        scope,
        "pad_speaker",
        pf_client_core::pad_audio::speaker_active(&s.pad_speaker),
        |s, on| s.pad_speaker = if on { "pad".into() } else { "off".into() },
    );
    let (pad_names, pad_i) = presets(&labeled(GAMEPADS), |v| {
        GamepadPref::from_name(v) == GamepadPref::from_name(&s.gamepad)
    });
    let pad_combo = setting_combo(cx, scope, "gamepad", pad_names, pad_i, |s, i| {
        s.gamepad = GAMEPADS[i].0.to_string();
    })
    .is_enabled(s.gamepad_forwarding);
    let (sysbtn_names, sysbtn_i) = presets(&labeled(SYSTEM_BUTTONS), |v| *v == s.system_buttons);
    let sysbtn_combo = setting_combo(
        cx,
        scope,
        "system_buttons",
        sysbtn_names,
        sysbtn_i,
        |s, i| {
            s.system_buttons = SYSTEM_BUTTONS[i].0.to_string();
        },
    )
    .is_enabled(s.gamepad_forwarding);

    let mut out = group(
        cx.pal,
        None,
        [
            // The read-only pad inventory (GTK parity): what THIS device sees right
            // now — the fastest answer to "is my controller even detected?". A
            // device fact, so defaults scope only, like the forward picker below.
            (!preset_mode).then(|| {
                let inventory: View = if pads.is_empty() {
                    text(client::pad_none_detected())
                        .font_size(12.0)
                        .foreground(cx.pal.get(Token::SecondaryText))
                        .into()
                } else {
                    vstack(
                        pads.iter()
                            .map(|p| {
                                let sub = if p.steam_virtual {
                                    desktop::desktop_pad_steam_virtual().to_string()
                                } else {
                                    p.kind_label().to_string()
                                };
                                vstack((
                                    text(p.name.clone()).font_weight(FontWeight::SEMI_BOLD),
                                    text(sub)
                                        .font_size(11.0)
                                        .foreground(cx.pal.get(Token::SecondaryText)),
                                ))
                                .spacing(1.0)
                                .into()
                            })
                            .collect::<Vec<View>>(),
                    )
                    .spacing(8.0)
                    .into()
                };
                described_labeled(
                    cx.pal,
                    desktop::desktop_pad_detected(),
                    inventory,
                    desktop::desktop_pad_detected_hint(),
                )
            }),
            Some(described_overridable(
                cx,
                &rows::GAMEPAD,
                pad_combo,
                client::player_gamepad_hint(),
            )),
            // This device's motors, so defaults scope only, like Controller haptics.
            (!preset_mode).then(|| {
                described_labeled(
                    cx.pal,
                    rows::PAD_RUMBLE.label(),
                    pad_rumble_toggle,
                    client::player_pad_rumble_hint(),
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect(),
        Some(client::prefs_applies_next_session()),
    );

    let d = Settings::default();
    let mut advanced = vec![
        // Whether ANY controller is forwarded — presetable, so it renders in both scopes
        // (a "Work" preset can decline what "Game" forwards).
        described_overridable(
            cx,
            &rows::FORWARDING,
            pad_forward_toggle,
            client::player_gamepad_forwarding_hint(),
        ),
    ];
    // Which physical pad this device forwards is a device fact (tier G): defaults scope
    // only. Apple forwards ONE pad as player 1; this client forwards each as its own player.
    if !preset_mode {
        advanced.push(described_labeled(
            cx.pal,
            rows::FORWARD_PAD.label(),
            forward_combo,
            client::player_forward_pad_hint(),
        ));
    }
    advanced.push(described_overridable(
        cx,
        &rows::SYSTEM_BUTTONS,
        sysbtn_combo,
        client::player_system_buttons_hint(),
    ));
    if !preset_mode {
        advanced.push(described_labeled(
            cx.pal,
            rows::PAD_HAPTICS.label(),
            pad_haptics_toggle,
            client::player_pad_haptics_hint(),
        ));
        advanced.push(described_labeled(
            cx.pal,
            rows::PAD_SPEAKER.label(),
            pad_speaker_toggle,
            client::player_pad_speaker_hint(),
        ));
    }
    let changed = [
        s.gamepad_forwarding != d.gamepad_forwarding,
        !s.forward_pad.is_empty(),
        s.system_buttons != d.system_buttons,
        s.pad_haptics != d.pad_haptics,
        s.pad_speaker != d.pad_speaker,
    ];
    out.extend(advanced_group(
        cx,
        advanced,
        changed.into_iter().filter(|c| *c).count(),
        ["gamepad_forwarding", "system_buttons"]
            .into_iter()
            .any(|f| cx.overrides(f)),
    ));
    out
}
