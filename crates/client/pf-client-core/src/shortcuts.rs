//! The in-stream controls for each client's reference screen: chords as data, words as
//! messages.
//!
//! Data only: the Linux and Windows clients and the console draw it in their own UI. Each
//! list is what that client binds, so a row here moves with the binding it names
//! (`pf-presenter`'s `chord_of`, [`crate::gamepad`]'s Select chords and escape chord, the
//! Android client's key and touch handlers, the Apple client's `macChords` and `padChords`,
//! the webOS client's remote and dial, the browser's `input.ts`).

use pf_l10n::client as c;
use std::borrow::Cow;
use std::fmt;
use Press::{Either, Hold, Keys};

/// A client family with its own bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    /// The Vulkan session binary: Linux, Windows and Steam Deck.
    Desktop,
    Android,
    /// An iPhone, iPad or Apple TV. Its keys are a hardware keyboard's, so the caller lists
    /// them only while one is attached. The Apple About screen lists the remote's rows.
    Apple,
    /// A Mac: the iPad's chords, plus mouse capture, the mouse mode and fullscreen.
    Mac,
    /// The LG TV client: Magic Remote colour keys, and its own dial and escape chord.
    WebOS,
    /// The browser page, with a keyboard, a mouse and a pad.
    Web,
    /// The Samsung TV client: the browser page, with the remote's Back as its menu key.
    Tizen,
    /// The PS5 title: a DualSense, its touchpad click standing in for Create.
    Ps5,
}

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
#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub keys: Press,
    text: Text,
}

#[derive(Clone, Copy, Debug)]
enum Text {
    Message(fn() -> &'static str),
    /// A message with a chord filled in.
    Filled(fn() -> String),
}

impl Item {
    pub fn text(&self) -> Cow<'static, str> {
        match self.text {
            Text::Message(text) => text().into(),
            Text::Filled(text) => text().into(),
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

#[derive(Clone, Copy, Debug)]
pub struct Group {
    pub kind: Kind,
    pub items: &'static [Item],
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

const DESKTOP_KEYS: &[Item] = &[
    item(Keys("Ctrl+Alt+Shift+Q"), c::controls_release_input),
    item(Keys("Ctrl+Alt+Shift+D"), c::controls_disconnect),
    item(Keys("Ctrl+Alt+Shift+S"), c::controls_cycle_stats),
    item(Keys("Ctrl+Alt+Shift+O"), c::controls_open_dial),
    item(Keys("Ctrl+Alt+Shift+V"), c::controls_toggle_mic),
    item(Keys("Ctrl+Alt+Shift+M"), c::controls_switch_mouse_mode),
    item(Either("F11", "Alt+Enter"), c::controls_toggle_fullscreen),
];

const WEB_KEYS: &[Item] = &[
    item(Keys("Ctrl+Alt+Shift+Q"), c::controls_release_mouse),
    item(Keys("Esc"), c::controls_release_mouse_page),
    item(Keys("Ctrl+Alt+Shift+D"), c::controls_disconnect),
    item(Keys("Ctrl+Alt+Shift+S"), c::controls_cycle_stats),
    item(Keys("Ctrl+Alt+Shift+O"), c::controls_open_menu),
    item(Keys("Ctrl+Alt+Shift+V"), c::controls_toggle_mic),
    item(Keys("Ctrl+Alt+Shift+M"), c::controls_switch_mouse_mode),
    item(Either("F11", "Alt+Enter"), c::controls_toggle_fullscreen),
];

// A keyboard on the set: Esc still reaches the host, so it is not listed.
const TIZEN_KEYS: &[Item] = &[
    item(Keys("Ctrl+Alt+Shift+Q"), c::controls_release_mouse),
    item(Keys("Ctrl+Alt+Shift+D"), c::controls_disconnect),
    item(Keys("Ctrl+Alt+Shift+S"), c::controls_cycle_stats),
    item(Keys("Ctrl+Alt+Shift+O"), c::controls_open_menu),
    item(Keys("Ctrl+Alt+Shift+V"), c::controls_toggle_mic),
    item(Keys("Ctrl+Alt+Shift+M"), c::controls_switch_mouse_mode),
    item(Either("F11", "Alt+Enter"), c::controls_toggle_fullscreen),
];

const ANDROID_KEYS: &[Item] = &[
    item(Keys("Ctrl+Alt+Shift+Q"), c::controls_release_pointer),
    item(Keys("Ctrl+Alt+Shift+O"), c::controls_open_dial),
];

// Spelled out: the console's font has no ⌃ ⌥ ⌘ ⎋. The Apple About screen lists these rows
// in symbols (`ShortcutsReference.swift`), and a test holds the two together.
const MAC_KEYS: &[Item] = &[
    gesture(Gesture::Click, c::controls_capture_mouse_keyboard),
    item(
        Either("Control+Option+Shift+Q", "Command+Esc"),
        c::controls_release_mouse_keyboard,
    ),
    item(Keys("Control+Option+Shift+D"), c::controls_disconnect),
    item(Keys("Control+Option+Shift+S"), c::controls_cycle_stats),
    item(Keys("Control+Option+Shift+O"), c::controls_open_dial),
    item(
        Keys("Control+Option+Shift+M"),
        c::controls_switch_mouse_mode,
    ),
    item(Keys("Control+Option+Shift+C"), c::controls_toggle_clipboard),
    Item {
        keys: Keys("Control+Command+F"),
        text: Text::Filled(|| c::controls_toggle_fullscreen_after("Command+Esc")),
    },
    item(Keys("Control+Option+Shift+A"), c::controls_toggle_mic),
];

const APPLE_KEYS: &[Item] = &[
    item(
        Either("Control+Option+Shift+Q", "Command+Esc"),
        c::controls_release_pointer_device,
    ),
    item(Keys("Control+Option+Shift+D"), c::controls_disconnect),
    item(Keys("Control+Option+Shift+S"), c::controls_cycle_stats),
    item(Keys("Control+Option+Shift+O"), c::controls_open_dial),
    item(Keys("Control+Option+Shift+C"), c::controls_toggle_clipboard),
    item(Keys("Control+Option+Shift+A"), c::controls_toggle_mic),
];

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

const DESKTOP_PAD: &[Item] = &[
    item(Keys("Select + A"), c::controls_open_dial),
    item(Keys("Select + X"), c::controls_cycle_stats),
    item(Hold("Select"), c::controls_guide_hold),
    item(
        Keys("L1 + R1 + Start + Select"),
        c::controls_release_hold_disconnect,
    ),
];

const ANDROID_PAD: &[Item] = &[
    item(Keys("Select + A"), c::controls_open_dial),
    item(Keys("Select + X"), c::controls_cycle_stats),
    item(Keys("Select + Y"), c::controls_toggle_mic),
    item(Hold("Select"), c::controls_guide_hold),
    item(
        Keys("L1 + R1 + Start + Select"),
        c::controls_hold_disconnect,
    ),
];

const APPLE_PAD: &[Item] = &[
    item(Keys("Select + A"), c::controls_open_dial),
    item(Keys("Select + X"), c::controls_cycle_stats),
    item(Hold("Select"), c::controls_guide_hold),
    item(
        Keys("L1 + R1 + Start + Select"),
        c::controls_hold_disconnect,
    ),
];

const WEBOS_PAD: &[Item] = &[
    item(Keys("Select + A"), c::controls_open_dial),
    item(
        Keys("L1 + R1 + Start + Select"),
        c::controls_hold_disconnect,
    ),
];

/// The console keeps Create for itself, so the touchpad click takes Select's place.
const PS5_PAD: &[Item] = &[
    item(Keys("Touchpad + Cross"), c::controls_open_dial),
    item(
        Keys("L1 + R1 + Options + Touchpad"),
        c::controls_hold_disconnect,
    ),
];

const WEB_PAD: &[Item] = &[
    item(Keys("Select + A"), c::controls_open_menu),
    item(
        Keys("L1 + R1 + Start + Select"),
        c::controls_release_hold_disconnect,
    ),
];

/// The groups a client shows, remote and keyboard first. `touchscreen` is false on a TV, which has no
/// screen to touch. An Apple TV binds no keys either.
pub fn groups(client: Client, touchscreen: bool) -> Vec<Group> {
    let (remote, keys, touch, pad): (&[Item], &[Item], &[Item], &[Item]) = match client {
        Client::Desktop => (&[], DESKTOP_KEYS, DESKTOP_TOUCH, DESKTOP_PAD),
        Client::Android => (&[], ANDROID_KEYS, ANDROID_TOUCH, ANDROID_PAD),
        Client::Apple if touchscreen => (&[], APPLE_KEYS, &[], APPLE_PAD),
        Client::Apple => (&[], &[], &[], APPLE_PAD),
        Client::Mac => (&[], MAC_KEYS, &[], APPLE_PAD),
        Client::WebOS => (WEBOS_REMOTE, &[], &[], WEBOS_PAD),
        Client::Web => (&[], WEB_KEYS, &[], WEB_PAD),
        Client::Tizen => (TIZEN_REMOTE, TIZEN_KEYS, &[], WEB_PAD),
        Client::Ps5 => (&[], &[], &[], PS5_PAD),
    };
    let group = |kind, items| Group {
        kind,
        items,
        dualsense: client == Client::Ps5,
    };
    let mut out = Vec::new();
    if !remote.is_empty() {
        out.push(group(Kind::Remote, remote));
    }
    if !keys.is_empty() {
        out.push(group(Kind::Keyboard, keys));
    }
    if touchscreen && !touch.is_empty() {
        out.push(group(Kind::Touch, touch));
    }
    out.push(group(Kind::Pad, pad));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Client; 8] = [
        Client::Desktop,
        Client::Android,
        Client::Apple,
        Client::Mac,
        Client::WebOS,
        Client::Web,
        Client::Tizen,
        Client::Ps5,
    ];

    /// Every client names its dial and escape chord; the PS5's use the touchpad for Select.
    #[test]
    fn every_client_lists_the_controller_chords() {
        for c in ALL {
            let pad = groups(c, true).pop().unwrap();
            let chords = if c == Client::Ps5 {
                ["Touchpad + Cross", "L1 + R1 + Options + Touchpad"]
            } else {
                ["Select + A", "L1 + R1 + Start + Select"]
            };
            for keys in chords {
                assert!(
                    pad.items.iter().any(|i| i.keys == Keys(keys)),
                    "{c:?} lacks {keys}"
                );
            }
        }
    }

    /// Only the native pad paths bind Select + X; webOS opens stats from the dial or Green,
    /// the browser from the keyboard.
    #[test]
    fn the_stats_chord_is_native_only() {
        let has = |c| {
            groups(c, true)
                .iter()
                .flat_map(|g| g.items)
                .any(|i| i.keys == Keys("Select + X"))
        };
        assert!(has(Client::Desktop) && has(Client::Android));
        assert!(has(Client::Apple) && has(Client::Mac));
        assert!(!has(Client::WebOS) && !has(Client::Web) && !has(Client::Tizen));
    }

    /// The Apple About screen's keyboard chords for one platform, symbols spelled out as the
    /// lists here write them. Its file opens each platform's rows with `from` and ends the
    /// keyboard group at the first `groups.append`. A row's `keys:` is a chord literal, two
    /// chords through `L10n.controlsEither`, or a gesture message.
    fn about_keys(from: &str) -> Vec<String> {
        const ABOUT: &str = include_str!(
            "../../../../clients/apple/Sources/PunktfunkClient/Settings/ShortcutsReference.swift"
        );
        let spell = |s: &str| {
            [
                ("⌃", "Control+"),
                ("⌥", "Option+"),
                ("⇧", "Shift+"),
                ("⌘", "Command+"),
                ("⎋", "Esc"),
            ]
            .iter()
            .fold(s.to_string(), |s, (symbol, word)| s.replace(symbol, word))
        };
        fn quoted(s: &str) -> &str {
            s.split_once('"').expect("a closing quote").0
        }
        // `controlsGestureClick` is the accessor of `controls_gesture_click`.
        let gesture = |accessor: &str| {
            let key: String = accessor
                .chars()
                .flat_map(|c| {
                    if c.is_ascii_uppercase() {
                        vec!['_', c.to_ascii_lowercase()]
                    } else {
                        vec![c]
                    }
                })
                .collect();
            pf_l10n::get(&key)
                .expect("a gesture message")
                .text()
                .into_owned()
        };
        let block = ABOUT.split_once(from).expect("the platform's rows").1;
        let block = block.split_once("groups.append").expect("the group").0;
        block
            .split(".init(\"")
            .skip(1)
            .map(|row| {
                let expr = row.split_once("keys: ").expect("a row's keys").1;
                if let Some(chord) = expr.strip_prefix('"') {
                    spell(quoted(chord))
                } else if let Some(rest) = expr.strip_prefix("L10n.controlsEither(first: \"") {
                    let (first, rest) = rest.split_once('"').expect("the first chord");
                    let second = rest.split_once("second: \"").expect("the second chord").1;
                    format!("{} or {}", spell(first), spell(quoted(second)))
                } else if let Some(rest) = expr.strip_prefix("String(localized: L10n.") {
                    gesture(rest.split_once(')').expect("a gesture accessor").0)
                } else {
                    panic!("a keys expression this test does not read: {expr}")
                }
            })
            .collect()
    }

    /// The console and the About screen name one set of bindings, in the same order. The
    /// chords are data on both sides; the texts are messages.
    #[test]
    fn the_apple_keys_are_the_rows_the_about_screen_lists() {
        let ours =
            |items: &[Item]| -> Vec<String> { items.iter().map(|i| i.keys.to_string()).collect() };
        assert_eq!(ours(MAC_KEYS), about_keys("#if os(macOS)"));
        assert_eq!(
            ours(APPLE_KEYS),
            about_keys("#elseif os(iOS) || os(visionOS)")
        );
    }

    #[test]
    fn an_apple_tv_lists_no_keys() {
        let keys = |c, touchscreen| {
            groups(c, touchscreen)
                .iter()
                .any(|g| g.kind == Kind::Keyboard)
        };
        assert!(keys(Client::Apple, true) && keys(Client::Mac, true));
        assert!(!keys(Client::Apple, false));
    }

    #[test]
    fn a_tv_has_no_touch_group() {
        let touch = |touchscreen| {
            groups(Client::Android, touchscreen)
                .iter()
                .any(|g| g.kind == Kind::Touch)
        };
        assert!(!touch(false) && touch(true));
    }

    #[test]
    fn only_android_lists_the_pad_mute() {
        let has = |c| {
            groups(c, true)
                .iter()
                .flat_map(|g| g.items)
                .any(|i| i.keys == Keys("Select + Y"))
        };
        assert!(has(Client::Android));
        assert!(ALL.iter().filter(|c| has(**c)).count() == 1);
    }

    #[test]
    fn no_label_is_empty_or_ends_in_a_period() {
        for c in ALL {
            for i in groups(c, true).iter().flat_map(|g| g.items) {
                let text = i.text();
                assert!(!i.keys.to_string().is_empty() && !text.is_empty());
                assert!(!text.ends_with('.'), "{text}");
            }
        }
    }

    /// A client that writes chords and headings as text gets whole English lines.
    #[test]
    fn a_chord_and_a_group_read_as_one_line() {
        assert_eq!(Hold("Yellow").to_string(), "Hold Yellow");
        assert_eq!(Either("F11", "Alt+Enter").to_string(), "F11 or Alt+Enter");
        let desktop = groups(Client::Desktop, true);
        let headings: Vec<_> = desktop.iter().map(Group::heading).collect();
        assert_eq!(
            headings,
            [
                "Keyboard",
                "Touchscreen (Trackpad and Direct pointer modes)",
                "Controller (Select is Back or View)"
            ]
        );
        let pad = desktop.last().unwrap();
        assert_eq!(pad.note().as_deref(), Some("Select is Back or View"));
        let ps5 = groups(Client::Ps5, true).pop().unwrap();
        assert_eq!((ps5.title(), ps5.note()), ("DualSense", None));
        let fullscreen = MAC_KEYS[7].text();
        assert!(fullscreen.contains("(after Command+Esc,"), "{fullscreen}");
    }
}
