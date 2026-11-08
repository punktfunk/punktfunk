//! Each client's shortcuts when the player has changed none: the chords it always had.

use super::model::{button_bit, Action, Binding, Shortcuts, Trigger};
use super::Client;
use crate::overlay_actions::SlotId;

const EXIT: [&str; 4] = ["lb", "rb", "start", "select"];
const PS5_EXIT: [&str; 4] = ["lb", "rb", "start", "touchpad"];

/// This client's defaults. `legacy_guide_gesture` is the old *Hold Select for guide* value:
/// `on` and `off` decide the Hold Select row, anything else is the client's automatic.
pub fn defaults(client: Client, game_mode: bool, legacy_guide_gesture: &str) -> Shortcuts {
    use SlotId::*;
    let guide = match legacy_guide_gesture {
        "on" => true,
        "off" => false,
        _ => match client {
            Client::Desktop => game_mode,
            Client::Apple => true,
            _ => false,
        },
    };
    let ring = || Action::Ring;
    let slot = Action::Slot;
    // The clients with their own pad path; the others' dial is their only pad chord.
    let native = matches!(
        client,
        Client::Desktop | Client::Android | Client::Apple | Client::Mac
    );
    let (open, exit) = match client {
        Client::Ps5 => ("touchpad", &PS5_EXIT),
        _ => ("select", &EXIT),
    };
    let mut b = Vec::new();
    // Pad rows first, then keys, each in the order the reference screen lists them.
    b.push(pad(&[open, "a"], false, ring()));
    if native {
        b.push(pad(&["select", "x"], false, slot(Stats)));
    }
    if client == Client::Android {
        b.push(pad(&["select", "y"], false, slot(Mic)));
    }
    if native && guide {
        b.push(pad(&["select"], true, slot(Guide)));
    }
    if matches!(client, Client::Desktop | Client::Web | Client::Tizen) {
        b.push(pad(exit, false, slot(ReleaseInput)));
    }
    b.push(pad(exit, true, slot(EndStream)));
    let chord = |k: &str, action: Action| key(&["ctrl", "alt", "shift"], k, action);
    match client {
        Client::Desktop | Client::Web | Client::Tizen => {
            b.push(chord("q", slot(ReleaseInput)));
            b.push(chord("d", slot(EndStream)));
            b.push(chord("s", slot(Stats)));
            b.push(chord("o", ring()));
            b.push(chord("v", slot(Mic)));
            b.push(chord("m", slot(MouseMode)));
            b.push(key(&[], "f11", slot(Fullscreen)));
            b.push(key(&["alt"], "enter", slot(Fullscreen)));
        }
        Client::Android => {
            b.push(chord("q", slot(ReleaseInput)));
            b.push(chord("o", ring()));
        }
        Client::Apple | Client::Mac => {
            b.push(chord("q", slot(ReleaseInput)));
            b.push(chord("d", slot(EndStream)));
            b.push(chord("s", slot(Stats)));
            b.push(chord("o", ring()));
            if client == Client::Mac {
                b.push(chord("m", slot(MouseMode)));
            }
            b.push(chord("c", slot(Clipboard)));
            b.push(chord("a", slot(Mic)));
        }
        Client::WebOS | Client::Ps5 => {}
    }
    Shortcuts { bindings: b }
}

/// The stored blob, or this client's defaults when it is empty or unreadable.
pub fn resolve(
    blob: &str,
    client: Client,
    game_mode: bool,
    legacy_guide_gesture: &str,
) -> Shortcuts {
    Shortcuts::read(blob).unwrap_or_else(|| defaults(client, game_mode, legacy_guide_gesture))
}

fn pad(buttons: &[&str], hold: bool, action: Action) -> Binding {
    Binding {
        trigger: Trigger::Pad {
            buttons: buttons.iter().filter_map(|n| button_bit(n)).collect(),
            hold,
        },
        action,
    }
}

fn key(modifiers: &[&str], key: &str, action: Action) -> Binding {
    Binding {
        trigger: Trigger::Key {
            modifiers: modifiers.iter().map(|m| m.to_string()).collect(),
            key: key.into(),
        },
        action,
    }
}
