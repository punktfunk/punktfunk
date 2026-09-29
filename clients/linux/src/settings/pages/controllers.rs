//! Controllers: the pads on this device, the virtual pad on the host, and what gets forwarded.

use super::{Build, PageB};
use crate::settings::choice::set_row_subtitle;
use crate::settings::field::Field;
use crate::settings::spec::{self, Page};
use crate::settings::tables::*;
use adw::prelude::*;
use std::rc::Rc;

pub fn controllers(b: &mut Build) {
    let mut p = b.page(Page::Controllers, "input-gaming-symbolic");
    let g = p.group("", "");
    // The pads plugged into this device: the device's, so preset scope leaves them out.
    let pads = b.gamepads.pads();
    if !b.preset_scope {
        inventory(&p, &g, &pads);
    }
    let pad_type = b.choice(
        &spec::GAMEPAD,
        &[
            "Automatic",
            "Xbox 360",
            "DualSense",
            "Xbox One",
            "DualShock 4",
            "Steam Deck",
            "Steam Controller 2",
        ],
    );
    let mut follows: Vec<gtk::Widget> = vec![pad_type.widget().clone().upcast()];
    b.put(
        &mut p,
        Some(&g),
        Field::choice(&spec::GAMEPAD, &pad_type, index::gamepad, |s, i| {
            s.gamepad = at(GAMEPADS, i).to_string()
        }),
    );

    // Off sends nothing and never opens a pad, which frees it for USB passthrough, so the rows
    // about a forwarded pad follow this switch.
    let (field, forwarding) = Field::switch(
        &spec::FORWARDING,
        |s| s.gamepad_forwarding,
        |s, v| s.gamepad_forwarding = v,
    );
    b.put(&mut p, None, field);
    let pin = pin_field(b, &pads);
    follows.push(pin.row.clone().upcast());
    b.put(&mut p, None, pin);
    // Guide and quick-access presses. Desktop rarely needs them off Automatic; they are here
    // because presets are authored on the desktop and applied everywhere.
    let row = b.choice(&spec::SYSTEM_BUTTONS, SYSTEM_BUTTON_LABELS);
    follows.push(row.widget().clone().upcast());
    b.put(
        &mut p,
        None,
        Field::choice(
            &spec::SYSTEM_BUTTONS,
            &row,
            index::system_buttons,
            |s, i| s.system_buttons = at(SYSTEM_BUTTONS, i).to_string(),
        ),
    );
    let row = b.choice(&spec::GUIDE_GESTURE, GUIDE_GESTURE_LABELS);
    follows.push(row.widget().clone().upcast());
    b.put(
        &mut p,
        None,
        Field::choice(&spec::GUIDE_GESTURE, &row, index::guide_gesture, |s, i| {
            s.guide_gesture = at(GUIDE_GESTURES, i).to_string()
        }),
    );
    // A wired DualSense's voice coils and speaker, streamed from the host.
    let (field, haptics) = Field::switch(
        &spec::PAD_HAPTICS,
        |s| s.pad_haptics,
        |s, v| s.pad_haptics = v,
    );
    follows.push(haptics.upcast());
    b.put(&mut p, None, field);
    // `"mix"` is a value this switch cannot show; only moving the switch writes over it.
    let (field, speaker) = Field::switch(
        &spec::PAD_SPEAKER,
        |s| pf_client_core::pad_audio::speaker_active(&s.pad_speaker),
        |s, v| s.pad_speaker = if v { "pad" } else { "off" }.to_string(),
    );
    follows.push(speaker.upcast());
    b.put(&mut p, None, field);

    for w in &follows {
        w.set_sensitive(false);
    }
    forwarding.connect_active_notify(move |r| {
        for w in &follows {
            w.set_sensitive(r.is_active());
        }
    });
    b.finish(p);
}

fn inventory(p: &PageB, g: &adw::PreferencesGroup, pads: &[crate::gamepad::PadInfo]) {
    if pads.is_empty() {
        let none = adw::ActionRow::builder()
            .title("No controllers detected")
            .css_classes(["dim-label"])
            .build();
        p.row(g, &none);
    }
    for pad in pads {
        let row = adw::ActionRow::builder()
            .title(&pad.name)
            .use_markup(false)
            .build();
        row.set_subtitle(if pad.steam_virtual {
            "Steam Input's virtual pad \u{2014} Automatic skips it while a real pad is connected"
        } else {
            pad.kind_label()
        });
        row.add_prefix(&crate::widgets::lucide::row_icon("gamepad-2"));
        p.row(g, &row);
    }
}

/// Automatic forwards every real controller as its own pad; pinning one forces single-player.
/// The pin is kept by stable key, so an offline pinned pad keeps its entry. The service takes
/// the pin at once.
fn pin_field(b: &Build, pads: &[crate::gamepad::PadInfo]) -> Field {
    let saved = b.seed.forward_pad.clone();
    let mut names = vec!["Automatic (all controllers)".to_string()];
    let mut keys = vec![String::new()];
    for pad in pads {
        let kind = pad.kind_label();
        names.push(if kind.is_empty() {
            pad.name.clone()
        } else {
            format!("{} \u{b7} {kind}", pad.name)
        });
        keys.push(pad.key.clone());
    }
    if !saved.is_empty() && !keys.contains(&saved) {
        let name = saved.splitn(3, ':').nth(2).unwrap_or("Saved controller");
        names.push(format!("{name} (not connected)"));
        keys.push(saved.clone());
    }
    let row = b.choice(
        &spec::FORWARD_PAD,
        &names.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    if pads.is_empty() {
        set_row_subtitle(row.widget(), "No controllers detected");
    }
    let keys = Rc::new(keys);
    let (k, svc) = (keys.clone(), b.gamepads.clone());
    Field::choice(
        &spec::FORWARD_PAD,
        &row,
        move |s| k.iter().position(|k| *k == s.forward_pad).unwrap_or(0) as u32,
        move |s, i| {
            let key = at(&keys, i).clone();
            svc.set_pinned((!key.is_empty()).then(|| key.clone()));
            s.forward_pad = key;
        },
    )
}
