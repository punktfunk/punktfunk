//! What the Shortcuts screen draws: the bindings as rows, plus the fixed touch, remote and
//! system rows. Key and button names are English data; the words around them are messages.

use super::model::{Action, Binding, Device, Shortcuts, Trigger};
use super::Client;
use crate::overlay_actions::{catalogue, key_legend, OverlayConfig, RingPlatform};
use pf_l10n::client as c;
use punktfunk_core::input::gamepad as wire;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Mutex;
use Press::{Either, Hold, Keys};

/// What to press. Key and button names are English data joined by `+`; only the words
/// around them and a gesture's name come from messages, and nothing reads those back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// One key, or keys pressed together: `"Ctrl+Alt+Shift+Q"`, `"Select + A"`.
    Keys(&'static str),
    /// Keys held down: [`pf_l10n::client::controls_hold`].
    Hold(&'static str),
    /// Either of two chords: [`pf_l10n::client::controls_either`].
    Either(&'static str, &'static str),
    Gesture(Gesture),
}

/// A pointer or touch gesture, named in the reader's language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    Click,
    ThreeFingerTap,
    TwoFingerTwist,
    ThreeFingerSwipeUp,
    ThreeFingerSwipeDown,
}

impl Gesture {
    pub fn label(self) -> &'static str {
        match self {
            Gesture::Click => c::controls_gesture_click(),
            Gesture::ThreeFingerTap => c::controls_gesture_three_finger_tap(),
            Gesture::TwoFingerTwist => c::controls_gesture_two_finger_twist(),
            Gesture::ThreeFingerSwipeUp => c::controls_gesture_three_finger_swipe_up(),
            Gesture::ThreeFingerSwipeDown => c::controls_gesture_three_finger_swipe_down(),
        }
    }
}

/// One line, for a client that writes a chord as text: "Hold Yellow", "F11 or Alt+Enter".
impl fmt::Display for Press {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Keys(keys) => f.write_str(keys),
            Hold(keys) => f.write_str(&c::controls_hold(keys)),
            Either(a, b) => f.write_str(&c::controls_either(a, b)),
            Press::Gesture(g) => f.write_str(g.label()),
        }
    }
}

/// What to press, then what it does ([`Item::text`]).
#[derive(Clone, Debug)]
pub struct Item {
    pub keys: Press,
    text: Text,
    /// The binding this row edits; `None` for a fixed row. An [`Press::Either`] row's second
    /// chord is the next binding with the same action on the device.
    pub binding: Option<usize>,
}

#[derive(Clone, Debug)]
enum Text {
    Message(fn() -> &'static str),
    /// A message with a chord filled in.
    Filled(fn() -> String),
    /// An action's catalogue label, or its id where this build has none.
    Label(String),
}

impl Item {
    pub fn text(&self) -> Cow<'static, str> {
        match &self.text {
            Text::Message(text) => text().into(),
            Text::Filled(text) => text().into(),
            Text::Label(label) => label.clone().into(),
        }
    }
}

/// Which device a group's rows are pressed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Remote,
    Keyboard,
    Touch,
    Pad,
}

#[derive(Clone, Debug)]
pub struct Group {
    pub kind: Kind,
    pub items: Vec<Item>,
    /// The PS5's pad group is the DualSense, by name, with no Select to explain.
    dualsense: bool,
}

/// The names one button carries across controllers, in the pad group's note.
const SELECT_NAMES: [&str; 3] = ["Select", "Back", "View"];

impl Group {
    /// The group's title; [`Group::note`] goes beside it.
    pub fn title(&self) -> &'static str {
        match self.kind {
            Kind::Remote => c::controls_group_remote(),
            Kind::Keyboard => c::controls_group_keyboard(),
            Kind::Touch => c::controls_group_touch(),
            Kind::Pad if self.dualsense => "DualSense",
            Kind::Pad => c::controls_group_pad(),
        }
    }

    /// A quieter line beside the title: when the rows apply, or what a button is called.
    pub fn note(&self) -> Option<Cow<'static, str>> {
        let [select, back, view] = SELECT_NAMES;
        match self.kind {
            Kind::Touch => Some(c::controls_group_touch_note().into()),
            Kind::Pad if !self.dualsense => {
                Some(c::controls_group_pad_note(select, back, view).into())
            }
            _ => None,
        }
    }

    /// Title and note as one line, for a client that draws them together.
    pub fn heading(&self) -> Cow<'static, str> {
        let [select, back, view] = SELECT_NAMES;
        match self.kind {
            Kind::Touch => c::controls_group_touch_full().into(),
            Kind::Pad if !self.dualsense => c::controls_group_pad_full(select, back, view).into(),
            _ => self.title().into(),
        }
    }
}

const fn item(keys: Press, text: fn() -> &'static str) -> Item {
    Item {
        keys,
        text: Text::Message(text),
        binding: None,
    }
}

const fn gesture(gesture: Gesture, text: fn() -> &'static str) -> Item {
    item(Press::Gesture(gesture), text)
}

const WEBOS_REMOTE: &[Item] = &[
    item(Keys("Green"), c::controls_cycle_stats),
    item(Keys("Yellow"), c::controls_cycle_log),
    item(Hold("Yellow"), c::controls_open_dial),
    item(Keys("Blue"), c::controls_show_osk),
    item(Keys("Red"), c::controls_right_mouse_button),
    item(Keys("Back"), c::controls_escape_on_host),
    item(Hold("Back"), c::controls_open_disconnect_dialog),
    item(Keys("Home"), c::controls_tv_home),
];

const TIZEN_REMOTE: &[Item] = &[item(Keys("Back"), c::controls_open_menu)];

const WEB_KEYS: &[Item] = &[item(Keys("Esc"), c::controls_release_mouse_page)];

// The console's font has no ⌃ ⌥ ⌘ ⎋, so the Apple rows spell them out.
const MAC_KEYS: &[Item] = &[
    gesture(Gesture::Click, c::controls_capture_mouse_keyboard),
    item(Keys("Command+Esc"), c::controls_release_mouse_keyboard),
    Item {
        keys: Keys("Control+Command+F"),
        text: Text::Filled(|| c::controls_toggle_fullscreen_after("Command+Esc")),
        binding: None,
    },
];

const APPLE_KEYS: &[Item] = &[item(
    Keys("Command+Esc"),
    c::controls_release_pointer_device,
)];

const DESKTOP_TOUCH: &[Item] = &[
    gesture(Gesture::ThreeFingerTap, c::controls_cycle_stats),
    gesture(Gesture::TwoFingerTwist, c::controls_open_dial),
];

const ANDROID_TOUCH: &[Item] = &[
    item(Keys("Back"), c::controls_open_dial),
    gesture(Gesture::ThreeFingerTap, c::controls_cycle_stats),
    gesture(Gesture::ThreeFingerSwipeUp, c::controls_show_keyboard),
    gesture(Gesture::ThreeFingerSwipeDown, c::controls_hide_keyboard),
    gesture(Gesture::TwoFingerTwist, c::controls_open_dial),
];

impl Shortcuts {
    /// The groups a client shows, remote and keyboard first. `touchscreen` is false on a TV,
    /// which has no screen to touch. An Apple TV binds no keys either.
    pub fn rows(&self, client: Client, touchscreen: bool) -> Vec<Group> {
        let (remote, fixed_keys, touch): (&[Item], &[Item], &[Item]) = match client {
            Client::Desktop => (&[], &[], DESKTOP_TOUCH),
            Client::Android => (&[], &[], ANDROID_TOUCH),
            Client::Apple => (&[], APPLE_KEYS, &[]),
            Client::Mac => (&[], MAC_KEYS, &[]),
            Client::WebOS => (WEBOS_REMOTE, &[], &[]),
            Client::Web => (&[], WEB_KEYS, &[]),
            Client::Tizen => (TIZEN_REMOTE, &[], &[]),
            Client::Ps5 => (&[], &[], &[]),
        };
        let keyboard = match client {
            Client::Apple => touchscreen,
            Client::WebOS | Client::Ps5 => false,
            _ => true,
        };
        let group = |kind, items| Group {
            kind,
            items,
            dualsense: client == Client::Ps5,
        };
        let labels = labels();
        let mut out = Vec::new();
        if !remote.is_empty() {
            out.push(group(Kind::Remote, remote.to_vec()));
        }
        if keyboard {
            let mut keys = self.items(Device::Keyboard, client, &labels);
            keys.extend_from_slice(fixed_keys);
            if !keys.is_empty() {
                out.push(group(Kind::Keyboard, keys));
            }
        }
        if touchscreen && !touch.is_empty() {
            out.push(group(Kind::Touch, touch.to_vec()));
        }
        out.push(group(Kind::Pad, self.items(Device::Pad, client, &labels)));
        out
    }

    /// The first trigger on `device` bound to `action`, as one line: "Hold L1 + R1 + Start +
    /// Select", "Ctrl+Alt+Shift+D". For the exit hint, the start banner, the capture hint.
    pub fn hint(&self, action: &Action, device: Device) -> Option<String> {
        self.bindings
            .iter()
            .find(|b| b.action == *action && b.trigger.device() == device)
            .map(|b| b.trigger.label(Client::Desktop))
    }

    /// One row per binding on `device`; two presses of one action share an `Either` row.
    fn items(&self, device: Device, client: Client, labels: &[(String, String)]) -> Vec<Item> {
        let mut shown = vec![false; self.bindings.len()];
        let mut items = Vec::new();
        for (i, b) in self.bindings.iter().enumerate() {
            if shown[i] || b.trigger.device() != device {
                continue;
            }
            let first = intern(b.trigger.chord(client));
            let also_pressed = |o: &Binding| {
                o.action == b.action && o.trigger.device() == device && !o.trigger.hold()
            };
            let keys = if b.trigger.hold() {
                Hold(first)
            } else if let Some(j) =
                (i + 1..self.bindings.len()).find(|&j| also_pressed(&self.bindings[j]))
            {
                shown[j] = true;
                Either(first, intern(self.bindings[j].trigger.chord(client)))
            } else {
                Keys(first)
            };
            let id = b.action.id();
            let label = match &b.action {
                Action::Ring => c::controls_open_dial().to_string(),
                _ => labels
                    .iter()
                    .find(|(k, _)| *k == id)
                    .map_or(id, |(_, label)| label.clone()),
            };
            items.push(Item {
                keys,
                text: Text::Label(label),
                binding: Some(i),
            });
        }
        items
    }
}

/// The old entry point: `client`'s default rows.
pub fn groups(client: Client, touchscreen: bool) -> Vec<Group> {
    super::defaults(client, false, "auto").rows(client, touchscreen)
}

impl Trigger {
    /// The keys alone: "Select + A", "Ctrl+Alt+Shift+Q", "Touchpad + Cross".
    pub fn chord(&self, client: Client) -> String {
        match self {
            Trigger::Pad { buttons, .. } => buttons
                .iter()
                .map(|&b| button_label(b, client == Client::Ps5))
                .collect::<Vec<_>>()
                .join(" + "),
            Trigger::Key { modifiers, key } => {
                let apple = matches!(client, Client::Apple | Client::Mac);
                modifiers
                    .iter()
                    .map(|m| match m.as_str() {
                        "ctrl" if apple => "Control".to_string(),
                        "alt" if apple => "Option".to_string(),
                        m => key_legend(m),
                    })
                    .chain([key_legend(key)])
                    .collect::<Vec<_>>()
                    .join("+")
            }
        }
    }

    /// The whole line: the chord, or "Hold …" for a hold.
    pub fn label(&self, client: Client) -> String {
        let chord = self.chord(client);
        if self.hold() {
            c::controls_hold(&chord)
        } else {
            chord
        }
    }
}

/// A button as the reference screens name it: Xbox positions with Select and Start and
/// PlayStation shoulders, or the DualSense's own names.
fn button_label(bit: u32, dualsense: bool) -> &'static str {
    match bit {
        wire::BTN_A if dualsense => "Cross",
        wire::BTN_B if dualsense => "Circle",
        wire::BTN_X if dualsense => "Square",
        wire::BTN_Y if dualsense => "Triangle",
        wire::BTN_START if dualsense => "Options",
        wire::BTN_BACK if dualsense => "Create",
        wire::BTN_GUIDE if dualsense => "PS",
        wire::BTN_A => "A",
        wire::BTN_B => "B",
        wire::BTN_X => "X",
        wire::BTN_Y => "Y",
        wire::BTN_LB => "L1",
        wire::BTN_RB => "R1",
        wire::BTN_LS_CLICK => "L3",
        wire::BTN_RS_CLICK => "R3",
        wire::BTN_START => "Start",
        wire::BTN_BACK => "Select",
        wire::BTN_GUIDE => "Guide",
        wire::BTN_DPAD_UP => "Up",
        wire::BTN_DPAD_DOWN => "Down",
        wire::BTN_DPAD_LEFT => "Left",
        wire::BTN_DPAD_RIGHT => "Right",
        wire::BTN_TOUCHPAD => "Touchpad",
        wire::BTN_MISC1 => "Share",
        wire::BTN_MISC2 => "C",
        wire::BTN_PADDLE1 => "R4",
        wire::BTN_PADDLE2 => "L4",
        wire::BTN_PADDLE3 => "R5",
        wire::BTN_PADDLE4 => "L5",
        _ => "?",
    }
}

/// Every action id the dial's catalogue names, with its label in the reader's language.
fn labels() -> Vec<(String, String)> {
    let cfg = OverlayConfig::platform_default(RingPlatform::Desktop);
    [RingPlatform::Desktop, RingPlatform::Touch]
        .into_iter()
        .flat_map(|p| catalogue(&cfg, p))
        .flat_map(|g| g.entries)
        .map(|e| (e.id, e.label))
        .collect()
}

/// `Press` is `Copy` over `&'static str` for the screens that draw it, so a chord's text is
/// kept for the process: one string per distinct chord, a handful.
fn intern(s: String) -> &'static str {
    static SEEN: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&kept) = seen.get(s.as_str()) {
        return kept;
    }
    let kept: &'static str = Box::leak(s.into_boxed_str());
    seen.insert(kept);
    kept
}
