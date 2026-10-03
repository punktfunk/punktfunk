//! The in-stream controls, as plain text for each client's reference screen.
//!
//! Data only: the Linux and Windows clients and the console draw it in their own UI. Each
//! list is what that client binds, so a row here moves with the binding it names
//! (`pf-presenter`'s `chord_of`, [`crate::gamepad`]'s Select chords and escape chord, the
//! Android client's key and touch handlers, the webOS client's remote and dial, the browser's
//! `input.ts`).

/// A client family with its own bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Client {
    /// The Vulkan session binary: Linux, Windows and Steam Deck.
    Desktop,
    Android,
    /// Controller chords only. The Apple About screen lists the keyboard and remote rows, and
    /// knows which device it runs on.
    Apple,
    /// The LG TV client: Magic Remote colour keys, and its own dial and escape chord.
    WebOS,
    /// The browser page, with a keyboard, a mouse and a pad.
    Web,
    /// The Samsung TV client: the browser page, with the remote's Back as its menu key.
    Tizen,
}

/// What to press, then what it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Item {
    pub keys: &'static str,
    pub text: &'static str,
}

#[derive(Clone, Copy, Debug)]
pub struct Group {
    pub title: &'static str,
    pub items: &'static [Item],
}

const fn item(keys: &'static str, text: &'static str) -> Item {
    Item { keys, text }
}

const WEBOS_REMOTE: &[Item] = &[
    item("Green", "Cycle the statistics overlay"),
    item("Yellow", "Cycle the log overlay"),
    item("Blue", "Show the on-screen keyboard"),
    item("Red", "Right mouse button"),
    item("Back", "Escape on the host"),
    item("Hold Back", "Open the disconnect dialog"),
    item("Home", "Go to the TV's home screen"),
];

const TIZEN_REMOTE: &[Item] = &[item("Back", "Open the quick actions menu")];

const DESKTOP_KEYS: &[Item] = &[
    item("Ctrl+Alt+Shift+Q", "Release input, or capture it again"),
    item("Ctrl+Alt+Shift+D", "Disconnect"),
    item("Ctrl+Alt+Shift+S", "Cycle the statistics overlay"),
    item("Ctrl+Alt+Shift+O", "Open the quick actions dial"),
    item("Ctrl+Alt+Shift+V", "Mute or unmute the microphone"),
    item("Ctrl+Alt+Shift+M", "Switch the mouse mode"),
    item("F11 or Alt+Enter", "Toggle fullscreen"),
];

const WEB_KEYS: &[Item] = &[
    item("Ctrl+Alt+Shift+Q", "Release the mouse, or capture it again"),
    item("Esc", "Release the mouse, where the page captured it"),
    item("Ctrl+Alt+Shift+D", "Disconnect"),
    item("Ctrl+Alt+Shift+S", "Cycle the statistics overlay"),
    item("Ctrl+Alt+Shift+O", "Open the quick actions menu"),
    item("Ctrl+Alt+Shift+V", "Mute or unmute the microphone"),
    item("Ctrl+Alt+Shift+M", "Switch the mouse mode"),
    item("F11 or Alt+Enter", "Toggle fullscreen"),
];

// A keyboard on the set: Esc still reaches the host, so it is not listed.
const TIZEN_KEYS: &[Item] = &[
    item("Ctrl+Alt+Shift+Q", "Release the mouse, or capture it again"),
    item("Ctrl+Alt+Shift+D", "Disconnect"),
    item("Ctrl+Alt+Shift+S", "Cycle the statistics overlay"),
    item("Ctrl+Alt+Shift+O", "Open the quick actions menu"),
    item("Ctrl+Alt+Shift+V", "Mute or unmute the microphone"),
    item("Ctrl+Alt+Shift+M", "Switch the mouse mode"),
    item("F11 or Alt+Enter", "Toggle fullscreen"),
];

const ANDROID_KEYS: &[Item] = &[
    item(
        "Ctrl+Alt+Shift+Q",
        "Release the pointer, or capture it again",
    ),
    item("Ctrl+Alt+Shift+O", "Open the quick actions dial"),
];

const DESKTOP_TOUCH: &[Item] = &[
    item("Three-finger tap", "Cycle the statistics overlay"),
    item("Two-finger twist", "Open the quick actions dial"),
];

const ANDROID_TOUCH: &[Item] = &[
    item("Back", "Open the quick actions dial"),
    item("Three-finger tap", "Cycle the statistics overlay"),
    item("Three-finger swipe up", "Show the keyboard"),
    item("Three-finger swipe down", "Hide the keyboard"),
    item("Two-finger twist", "Open the quick actions dial"),
];

const DESKTOP_PAD: &[Item] = &[
    item("Select + A", "Open the quick actions dial"),
    item("Select + X", "Cycle the statistics overlay"),
    item(
        "Hold Select",
        "Press the host's guide button, where Hold Select for guide is on",
    ),
    item(
        "L1 + R1 + Start + Select",
        "Release input; hold to disconnect",
    ),
];

const ANDROID_PAD: &[Item] = &[
    item("Select + A", "Open the quick actions dial"),
    item("Select + X", "Cycle the statistics overlay"),
    item("Select + Y", "Mute or unmute the microphone"),
    item(
        "Hold Select",
        "Press the host's guide button, where Hold Select for guide is on",
    ),
    item("L1 + R1 + Start + Select", "Hold to disconnect"),
];

const APPLE_PAD: &[Item] = &[
    item("Select + A", "Open the quick actions dial"),
    item("Select + X", "Cycle the statistics overlay"),
    item(
        "Hold Select",
        "Press the host's guide button, where Hold Select for guide is on",
    ),
    item("L1 + R1 + Start + Select", "Hold to disconnect"),
];

const WEBOS_PAD: &[Item] = &[
    item("Select + A", "Open the quick actions dial"),
    item("L1 + R1 + Start + Select", "Hold to disconnect"),
];

const WEB_PAD: &[Item] = &[
    item("Select + A", "Open the quick actions menu"),
    item(
        "L1 + R1 + Start + Select",
        "Release input; hold to disconnect",
    ),
];

/// The groups a client shows, remote and keyboard first. `touchscreen` is false on a TV, which has no
/// screen to touch.
pub fn groups(client: Client, touchscreen: bool) -> Vec<Group> {
    let (remote, keys, touch, pad): (&[Item], &[Item], &[Item], &[Item]) = match client {
        Client::Desktop => (&[], DESKTOP_KEYS, DESKTOP_TOUCH, DESKTOP_PAD),
        Client::Android => (&[], ANDROID_KEYS, ANDROID_TOUCH, ANDROID_PAD),
        Client::Apple => (&[], &[], &[], APPLE_PAD),
        Client::WebOS => (WEBOS_REMOTE, &[], &[], WEBOS_PAD),
        Client::Web => (&[], WEB_KEYS, &[], WEB_PAD),
        Client::Tizen => (TIZEN_REMOTE, TIZEN_KEYS, &[], WEB_PAD),
    };
    let mut out = Vec::new();
    if !remote.is_empty() {
        out.push(Group {
            title: "Remote",
            items: remote,
        });
    }
    if !keys.is_empty() {
        out.push(Group {
            title: "Keyboard",
            items: keys,
        });
    }
    if touchscreen && !touch.is_empty() {
        out.push(Group {
            title: "Touchscreen (Trackpad and Direct pointer modes)",
            items: touch,
        });
    }
    out.push(Group {
        title: "Controller (Select is Back or View)",
        items: pad,
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Client; 6] = [
        Client::Desktop,
        Client::Android,
        Client::Apple,
        Client::WebOS,
        Client::Web,
        Client::Tizen,
    ];

    #[test]
    fn every_client_lists_the_controller_chords() {
        for c in ALL {
            let pad = groups(c, true).pop().unwrap();
            for keys in ["Select + A", "L1 + R1 + Start + Select"] {
                assert!(
                    pad.items.iter().any(|i| i.keys == keys),
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
                .any(|i| i.keys == "Select + X")
        };
        assert!(has(Client::Desktop) && has(Client::Android) && has(Client::Apple));
        assert!(!has(Client::WebOS) && !has(Client::Web) && !has(Client::Tizen));
    }

    #[test]
    fn a_tv_has_no_touch_group() {
        assert!(groups(Client::Android, false)
            .iter()
            .all(|g| !g.title.starts_with("Touch")));
        assert!(groups(Client::Android, true)
            .iter()
            .any(|g| g.title.starts_with("Touch")));
    }

    #[test]
    fn only_android_lists_the_pad_mute() {
        let has = |c| {
            groups(c, true)
                .iter()
                .flat_map(|g| g.items)
                .any(|i| i.keys == "Select + Y")
        };
        assert!(has(Client::Android));
        assert!(ALL.iter().filter(|c| has(**c)).count() == 1);
    }

    #[test]
    fn no_label_is_empty_or_ends_in_a_period() {
        for c in ALL {
            for i in groups(c, true).iter().flat_map(|g| g.items) {
                assert!(!i.keys.is_empty() && !i.text.is_empty());
                assert!(!i.text.ends_with('.'), "{}", i.text);
            }
        }
    }
}
