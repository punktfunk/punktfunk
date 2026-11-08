//! A shortcut is a trigger bound to an action; [`Shortcuts`] is the `shortcuts` blob.

use crate::overlay_actions::SlotId;
use punktfunk_core::input::{gamepad as wire, key_vk};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Pad buttons by their stored name. The console's input test names (`PadTestState.held`)
/// lowercased, `select` for Back, and the wire's own names for the rest.
pub const BUTTONS: [(&str, u32); 22] = [
    ("a", wire::BTN_A),
    ("b", wire::BTN_B),
    ("x", wire::BTN_X),
    ("y", wire::BTN_Y),
    ("lb", wire::BTN_LB),
    ("rb", wire::BTN_RB),
    ("ls", wire::BTN_LS_CLICK),
    ("rs", wire::BTN_RS_CLICK),
    ("start", wire::BTN_START),
    ("select", wire::BTN_BACK),
    ("guide", wire::BTN_GUIDE),
    ("up", wire::BTN_DPAD_UP),
    ("down", wire::BTN_DPAD_DOWN),
    ("left", wire::BTN_DPAD_LEFT),
    ("right", wire::BTN_DPAD_RIGHT),
    ("touchpad", wire::BTN_TOUCHPAD),
    ("misc1", wire::BTN_MISC1),
    ("misc2", wire::BTN_MISC2),
    ("paddle1", wire::BTN_PADDLE1),
    ("paddle2", wire::BTN_PADDLE2),
    ("paddle3", wire::BTN_PADDLE3),
    ("paddle4", wire::BTN_PADDLE4),
];

/// The wire bit of a stored button name, any case; `back` is `select`.
pub fn button_bit(name: &str) -> Option<u32> {
    let n = name.trim().to_ascii_lowercase();
    let n = if n == "back" { "select" } else { n.as_str() };
    BUTTONS.iter().find(|(k, _)| *k == n).map(|&(_, bit)| bit)
}

/// The stored name of a wire bit.
pub fn button_name(bit: u32) -> Option<&'static str> {
    BUTTONS.iter().find(|(_, b)| *b == bit).map(|&(k, _)| k)
}

/// The keyboard modifiers a shortcut may hold, in display order. ⌘ and Win are the host's.
pub const MODIFIERS: [&str; 3] = ["ctrl", "alt", "shift"];

fn modifier(name: &str) -> Option<&'static str> {
    match name.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some("ctrl"),
        "alt" | "option" => Some("alt"),
        "shift" => Some("shift"),
        _ => None,
    }
}

/// Where a trigger is pressed; one [`Engine`](super::Engine) runs per pad and one for the keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Device {
    Pad,
    Keyboard,
}

/// What the player presses. Pad buttons are wire bits in stored order; keys are
/// [`key_vk`] names, modifiers in [`MODIFIERS`] order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trigger {
    Pad { buttons: Vec<u32>, hold: bool },
    Key { modifiers: Vec<String>, key: String },
}

impl Trigger {
    pub fn device(&self) -> Device {
        match self {
            Trigger::Pad { .. } => Device::Pad,
            Trigger::Key { .. } => Device::Keyboard,
        }
    }

    /// A keyboard trigger is always a press.
    pub fn hold(&self) -> bool {
        matches!(self, Trigger::Pad { hold: true, .. })
    }

    /// What the engine matches, sorted: wire bits, or the VKs of the modifiers and the key.
    pub fn ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = match self {
            Trigger::Pad { buttons, .. } => buttons.clone(),
            Trigger::Key { modifiers, key } => modifiers
                .iter()
                .chain([key])
                .filter_map(|k| key_vk(k).map(u32::from))
                .collect(),
        };
        ids.sort_unstable();
        ids.dedup();
        ids
    }
}

/// How an action meets a hold: once, or down and up on the host like a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Once,
    /// Guide, Quick access, a key combination: a hold keeps it down on the host.
    KeyLike,
}

/// A quick action, or opening the dial itself. `Unknown` is an id from a newer client: the
/// row stays, dimmed, and the engine never sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Slot(SlotId),
    Ring,
    Unknown(String),
}

impl Action {
    /// Inverse of [`Action::parse`].
    pub fn id(&self) -> String {
        match self {
            Action::Slot(slot) => slot.id(),
            Action::Ring => "ring".into(),
            Action::Unknown(id) => id.clone(),
        }
    }

    pub fn parse(id: &str) -> Action {
        match id {
            "ring" => Action::Ring,
            _ => SlotId::parse(id).map_or_else(|| Action::Unknown(id.into()), Action::Slot),
        }
    }

    pub fn kind(&self) -> ActionKind {
        match self {
            Action::Slot(SlotId::Guide | SlotId::Qam | SlotId::Shortcut(_)) => ActionKind::KeyLike,
            _ => ActionKind::Once,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub trigger: Trigger,
    pub action: Action,
}

/// Why the editor refuses a new binding. Indices point into [`Shortcuts::bindings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// The same trigger already exists: offer to replace it.
    Identical(usize),
    /// One of the two is a press whose buttons the other's contain: it fires first.
    PressSubset(usize),
    /// One button is a hold; a press needs two.
    SinglePress,
    NoModifier,
    NoKey,
}

/// One binding as the blob stores it.
#[derive(Serialize, Deserialize)]
struct Stored {
    on: String,
    keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hold: Option<bool>,
    #[serde(rename = "do")]
    action: String,
}

impl Stored {
    fn binding(self) -> Option<Binding> {
        let trigger = match self.on.as_str() {
            "pad" => {
                let mut buttons: Vec<u32> = Vec::new();
                for name in &self.keys {
                    let bit = button_bit(name)?;
                    if !buttons.contains(&bit) {
                        buttons.push(bit);
                    }
                }
                Trigger::Pad {
                    buttons,
                    hold: self.hold.unwrap_or(false),
                }
            }
            "key" => {
                let (key, mods) = self.keys.split_last()?;
                let key = key.trim().to_ascii_lowercase();
                key_vk(&key)?;
                let mods = mods
                    .iter()
                    .map(|m| modifier(m))
                    .collect::<Option<Vec<_>>>()?;
                Trigger::Key {
                    modifiers: MODIFIERS
                        .iter()
                        .filter(|m| mods.contains(m))
                        .map(|m| m.to_string())
                        .collect(),
                    key,
                }
            }
            _ => return None,
        };
        let empty = matches!(&trigger, Trigger::Pad { buttons, .. } if buttons.is_empty());
        (!empty).then(|| Binding {
            trigger,
            action: Action::parse(&self.action),
        })
    }

    fn from(b: &Binding) -> Stored {
        let (on, keys, hold) = match &b.trigger {
            Trigger::Pad { buttons, hold } => (
                "pad",
                buttons
                    .iter()
                    .filter_map(|&bit| button_name(bit).map(String::from))
                    .collect(),
                Some(*hold),
            ),
            Trigger::Key { modifiers, key } => (
                "key",
                modifiers.iter().chain([key]).cloned().collect(),
                None,
            ),
        };
        Stored {
            on: on.into(),
            keys,
            hold,
            action: b.action.id(),
        }
    }
}

/// Every shortcut of one device and preset. Order is display order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shortcuts {
    pub bindings: Vec<Binding>,
}

impl Shortcuts {
    pub const SCHEMA_VERSION: u32 = 1;

    /// Never fails: an unparseable blob is empty, and a binding with a name this build does not
    /// know is dropped. An unknown action keeps its row as [`Action::Unknown`].
    pub fn parse(json: &str) -> Shortcuts {
        Shortcuts::read(json).unwrap_or_default()
    }

    /// `None` when the blob is not a shortcuts blob at all; the caller takes the defaults.
    pub(super) fn read(json: &str) -> Option<Shortcuts> {
        let blob: Value = serde_json::from_str(json).ok()?;
        let bindings = blob.get("bindings")?.as_array()?;
        Some(Shortcuts {
            bindings: bindings
                .iter()
                .filter_map(|b| serde_json::from_value::<Stored>(b.clone()).ok()?.binding())
                .collect(),
        })
    }

    pub fn to_json(&self) -> String {
        #[derive(Serialize)]
        struct Blob {
            v: u32,
            bindings: Vec<Stored>,
        }
        serde_json::to_string(&Blob {
            v: Self::SCHEMA_VERSION,
            bindings: self.bindings.iter().map(Stored::from).collect(),
        })
        .unwrap_or_default()
    }

    /// Whether `candidate` may join these bindings. An edit is checked against the list
    /// without the binding it changes.
    pub fn validate(&self, candidate: &Binding) -> Result<(), Conflict> {
        match &candidate.trigger {
            Trigger::Key { modifiers, key } => {
                if key.is_empty() || modifier(key).is_some() {
                    return Err(Conflict::NoKey);
                }
                if modifiers.is_empty() {
                    return Err(Conflict::NoModifier);
                }
            }
            Trigger::Pad { buttons, hold } => {
                if buttons.len() < 2 && !(*hold && buttons.len() == 1) {
                    return Err(Conflict::SinglePress);
                }
            }
        }
        let ids = candidate.trigger.ids();
        let device = candidate.trigger.device();
        let same_device = || {
            self.bindings
                .iter()
                .enumerate()
                .filter(move |(_, b)| b.trigger.device() == device)
        };
        if let Some((i, _)) = same_device()
            .find(|(_, b)| b.trigger.ids() == ids && b.trigger.hold() == candidate.trigger.hold())
        {
            return Err(Conflict::Identical(i));
        }
        // Modifiers are down before the key, so only pad presses can cut a chord short.
        if device == Device::Pad {
            let strict = |small: &[u32], big: &[u32]| {
                small.len() < big.len() && small.iter().all(|b| big.contains(b))
            };
            if let Some((i, _)) = same_device().find(|(_, b)| {
                let other = b.trigger.ids();
                (!candidate.trigger.hold() && strict(&ids, &other))
                    || (!b.trigger.hold() && strict(&other, &ids))
            }) {
                return Err(Conflict::PressSubset(i));
            }
        }
        Ok(())
    }
}
