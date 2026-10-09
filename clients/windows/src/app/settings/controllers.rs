//! Controllers: the pad inventory, forwarding and the emulated pad.

use super::{
    advanced_group, described_labeled, described_overridable, group, presets, setting_combo,
    setting_toggle, Cx,
};
use crate::trust::Settings;
use punktfunk_core::config::GamepadPref;
use windows_reactor::*;

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

/// Controllers: the pad inventory, forwarding and the emulated pad.
pub(super) fn controllers_section(cx: &Cx) -> Vec<Element> {
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
    .enabled(s.gamepad_forwarding);
    let pad_speaker_toggle = setting_toggle(
        cx,
        scope,
        "pad_speaker",
        pf_client_core::pad_audio::speaker_active(&s.pad_speaker),
        |s, on| s.pad_speaker = if on { "pad".into() } else { "off".into() },
    );
    let (pad_names, pad_i) = presets(GAMEPADS, |v| {
        GamepadPref::from_name(v) == GamepadPref::from_name(&s.gamepad)
    });
    let pad_combo = setting_combo(cx, scope, "gamepad", pad_names, pad_i, |s, i| {
        s.gamepad = GAMEPADS[i].0.to_string();
    })
    .enabled(s.gamepad_forwarding);
    let (sysbtn_names, sysbtn_i) = presets(SYSTEM_BUTTONS, |v| *v == s.system_buttons);
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
    .enabled(s.gamepad_forwarding);
    let (gesture_names, gesture_i) = presets(GUIDE_GESTURES, |v| *v == s.guide_gesture);
    let gesture_combo = setting_combo(
        cx,
        scope,
        "guide_gesture",
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
                cx,
                "gamepad",
                "Controller type",
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
            cx,
            "gamepad_forwarding",
            "Forward controllers",
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
            cx,
            "system_buttons",
            "Guide button",
            sysbtn_combo,
            "Where the guide (Xbox/PS) and quick-access presses go while streaming. \
             Automatic sends them to the host \u{2014} except on devices whose own overlay \
             reacts to the same press (Gaming Mode), where they stay local and the gesture \
             below reaches the host.",
        ),
        described_overridable(
            cx,
            "guide_gesture",
            "Hold Select for guide",
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
            "Play a DualSense's voice-coil haptics on the pad itself, while controllers are \
             forwarded.",
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
        ["gamepad_forwarding", "system_buttons", "guide_gesture"]
            .into_iter()
            .any(|f| cx.overrides(f)),
    ));
    out
}
