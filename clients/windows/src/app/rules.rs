//! The shell's decisions as plain functions over plain data: what holds the console back, how a
//! link meets the shell, how an advert changes the host list, where Back leads, how the pane
//! sits, what a key does. `mod.rs` owns the reactor wiring and calls these, so a table test pins
//! each rule without a window.

use super::{Dest, Screen};
use pf_client_core::deeplink::{self, DeepLink};
use pf_client_core::discovery::{DiscoveredHost, DiscoveryEvent};
use pf_l10n::client;

/// Whether something holds the shell, so a controller connecting waits for the next first pad
/// instead of opening the console: a connect, a stream, a pairing, a trust question, a speed
/// test, or a who-plays question (`picker`, `seat`).
pub(super) fn console_waits(screen: &Screen, picker: bool, seat: bool) -> bool {
    picker
        || seat
        || matches!(
            screen,
            Screen::Stream
                | Screen::Connecting
                | Screen::Waking
                | Screen::RequestAccess
                | Screen::Pair
                | Screen::Trust
                | Screen::SpeedTest
        )
}

/// What an arriving link does before a plan is made.
#[derive(Debug, PartialEq)]
pub(super) enum LinkGate {
    /// Say it on the status line and go to the host list.
    Refuse(String),
    /// Say it on the status line; the screen stays where it is.
    Hold(String),
    /// A link that parsed and may proceed to a plan.
    Plan(DeepLink),
}

/// A link never preempts a session: its child, or a connect still waiting on a wake or an
/// approval. `session_running` is the child; the screens cover the waits that have none. While
/// one is alive every outcome, a refusal included, leaves the screen where it is: a refusal that
/// went home would drop the wait's Cancel page and disarm the screen test for the next link.
pub(super) fn gate_link(url: &str, screen: &Screen, session_running: bool) -> LinkGate {
    let busy = matches!(
        screen,
        Screen::Stream | Screen::Connecting | Screen::Waking | Screen::RequestAccess
    ) || session_running;
    match deeplink::parse(url) {
        Err(e) if busy => LinkGate::Hold(e.message()),
        Err(e) => LinkGate::Refuse(e.message()),
        Ok(_) if busy => LinkGate::Hold(client::link_session_running().into()),
        Ok(link) => LinkGate::Plan(link),
    }
}

/// Fold one discovery event into the live advert list: a refresh replaces its host in place
/// (one entry per `key`), a goodbye or TTL expiry drops the advert by its mDNS `fullname`.
pub(super) fn fold_advert(acc: &mut Vec<DiscoveredHost>, event: DiscoveryEvent) {
    match event {
        DiscoveryEvent::Resolved(h) => {
            if let Some(e) = acc.iter_mut().find(|e| e.key == h.key) {
                *e = h;
            } else {
                acc.push(h);
            }
        }
        DiscoveryEvent::Removed { fullname } => acc.retain(|e| e.fullname != fullname),
    }
}

/// The "Open this link?" body. It names the host and the game, because that is how someone
/// tells their own shortcut from a link a web page handed them, and what the host set for it.
pub(super) fn link_prompt(
    name: &str,
    addr: &str,
    launch: Option<&str>,
    overrides: Option<&pf_client_core::library::GameOverrides>,
) -> String {
    let mut s = client::link_confirm_asks(name, addr);
    if let Some(id) = launch {
        s.push_str("\n\n");
        s.push_str(&client::link_confirm_launch(id));
    }
    if let Some(line) = overrides.and_then(pf_client_core::library::host_line) {
        s.push_str("\n\n");
        s.push_str(&line);
    }
    s.push_str("\n\n");
    s.push_str(client::link_confirm_guessable());
    s
}

/// Below this client width the pane leaves the page and opens over it.
const PANE_BESIDE_WIDTH: f64 = 600.0;
/// From this client width the pane opens with its labels unless the player closed it.
const PANE_OPEN_WIDTH: f64 = 900.0;

/// How the navigation pane sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pane {
    /// Off the page. The title bar's toggle opens it over the page (`true`).
    Over(bool),
    /// Beside the page: open with its labels (`true`), or the icon rail.
    Beside(bool),
}

/// The pane at a client `width`. `choice` is the toggle's last word on a pane beside the page,
/// `over` whether the pane is up over a narrow one.
pub(super) fn pane(width: f64, choice: Option<bool>, over: bool) -> Pane {
    if width < PANE_BESIDE_WIDTH {
        Pane::Over(over)
    } else {
        Pane::Beside(choice.unwrap_or(width >= PANE_OPEN_WIDTH))
    }
}

/// Where the title bar's Back leads.
#[derive(Debug, PartialEq)]
pub(super) enum Back {
    Screen(Screen),
    /// Out of a host's page, to the host list.
    HostList,
    /// Out of the hidden titles, to the shelf.
    Shelf,
}

/// Back from `screen`, with Home as `dest`, `host_page` and `hidden_titles` leave it. `None`
/// where there is nowhere to go back to, or the screen has its own Cancel.
pub(super) fn back(
    screen: &Screen,
    dest: Dest,
    host_page: bool,
    hidden_titles: bool,
) -> Option<Back> {
    match screen {
        Screen::Settings | Screen::Help | Screen::SpeedTest => Some(Back::Screen(Screen::Home)),
        Screen::Licenses => Some(Back::Screen(Screen::Settings)),
        Screen::Home => match dest {
            Dest::Hosts if host_page => Some(Back::HostList),
            Dest::Library if hidden_titles => Some(Back::Shelf),
            _ => None,
        },
        _ => None,
    }
}

/// What a key pressed in the shell's window asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shortcut {
    Hosts,
    Library,
    /// Show the Library with the caret in its search box.
    Search,
    AddHost,
    /// Scan for hosts, or reload the shelf.
    Reload,
    Settings,
    Back,
}

/// The shortcut for virtual key `vk` with Ctrl and Alt as held.
pub(super) fn shortcut(vk: u32, ctrl: bool, alt: bool) -> Option<Shortcut> {
    const LEFT: u32 = 0x25;
    const F5: u32 = 0x74;
    const COMMA: u32 = 0xBC;
    match (vk, ctrl, alt) {
        (0x31, true, false) => Some(Shortcut::Hosts),
        (0x32, true, false) => Some(Shortcut::Library),
        (0x46, true, false) => Some(Shortcut::Search),
        (0x4E, true, false) => Some(Shortcut::AddHost),
        (0x52, true, false) | (F5, false, false) => Some(Shortcut::Reload),
        (COMMA, true, false) => Some(Shortcut::Settings),
        (LEFT, false, true) => Some(Shortcut::Back),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Screen; 12] = [
        Screen::Trust,
        Screen::Home,
        Screen::Connecting,
        Screen::RequestAccess,
        Screen::Waking,
        Screen::WakeParked,
        Screen::Stream,
        Screen::Settings,
        Screen::Licenses,
        Screen::Help,
        Screen::Pair,
        Screen::SpeedTest,
    ];

    #[test]
    fn the_console_waits_on_a_connect_a_pairing_or_a_question() {
        let waits = [
            Screen::Connecting,
            Screen::RequestAccess,
            Screen::Waking,
            Screen::Stream,
            Screen::Pair,
            Screen::Trust,
            Screen::SpeedTest,
        ];
        for s in ALL {
            assert_eq!(console_waits(&s, false, false), waits.contains(&s));
        }
        // A parked wake is a result screen, not a connect in flight: the console may open.
        assert!(!console_waits(&Screen::WakeParked, false, false));
        // A profile question or a seat wait holds the shell over any screen.
        for s in ALL {
            assert!(console_waits(&s, true, false));
            assert!(console_waits(&s, false, true));
        }
    }

    fn host(key: &str, fullname: &str, addr: &str) -> DiscoveredHost {
        DiscoveredHost {
            key: key.into(),
            fullname: fullname.into(),
            name: key.into(),
            addr: addr.into(),
            port: 9777,
            fp_hex: String::new(),
            pair: "required".into(),
            mgmt_port: None,
            mac: Vec::new(),
            os: String::new(),
            wire: Vec::new(),
        }
    }

    fn addrs(acc: &[DiscoveredHost]) -> Vec<(&str, &str)> {
        acc.iter()
            .map(|h| (h.key.as_str(), h.addr.as_str()))
            .collect()
    }

    #[test]
    fn adverts_fold_by_key_and_drop_by_fullname() {
        let mut acc = Vec::new();
        fold_advert(
            &mut acc,
            DiscoveryEvent::Resolved(host("a", "a.1", "10.0.0.1")),
        );
        fold_advert(
            &mut acc,
            DiscoveryEvent::Resolved(host("b", "b.1", "10.0.0.2")),
        );
        assert_eq!(addrs(&acc), [("a", "10.0.0.1"), ("b", "10.0.0.2")]);

        // A refresh keeps the host's place and takes the new address.
        fold_advert(
            &mut acc,
            DiscoveryEvent::Resolved(host("a", "a.1", "10.0.0.9")),
        );
        assert_eq!(addrs(&acc), [("a", "10.0.0.9"), ("b", "10.0.0.2")]);

        // The same host under a new instance name replaces the old advert; the old name's
        // goodbye then finds nothing to drop.
        fold_advert(
            &mut acc,
            DiscoveryEvent::Resolved(host("a", "a.2", "10.0.0.9")),
        );
        fold_advert(
            &mut acc,
            DiscoveryEvent::Removed {
                fullname: "a.1".into(),
            },
        );
        assert_eq!(addrs(&acc), [("a", "10.0.0.9"), ("b", "10.0.0.2")]);

        fold_advert(
            &mut acc,
            DiscoveryEvent::Removed {
                fullname: "a.2".into(),
            },
        );
        assert_eq!(addrs(&acc), [("b", "10.0.0.2")]);
        fold_advert(
            &mut acc,
            DiscoveryEvent::Removed {
                fullname: "gone".into(),
            },
        );
        assert_eq!(addrs(&acc), [("b", "10.0.0.2")]);
    }

    const VALID: &str = "punktfunk://connect/Desk";

    #[test]
    fn a_link_dials_from_an_idle_screen_and_holds_on_a_session() {
        let busy = [
            Screen::Stream,
            Screen::Connecting,
            Screen::Waking,
            Screen::RequestAccess,
        ];
        let parsed = deeplink::parse(VALID).unwrap();
        for s in ALL {
            let want = if busy.contains(&s) {
                LinkGate::Hold(client::link_session_running().into())
            } else {
                LinkGate::Plan(parsed.clone())
            };
            assert_eq!(
                gate_link(VALID, &s, false),
                want,
                "screen busy={}",
                busy.contains(&s)
            );
            // A live child holds a link on any screen.
            assert_eq!(
                gate_link(VALID, &s, true),
                LinkGate::Hold(client::link_session_running().into())
            );
        }
        assert_eq!(
            client::link_session_running(),
            "A session is already running \u{2014} end it first."
        );
    }

    #[test]
    fn a_bad_link_is_refused_on_an_idle_screen() {
        let msg = deeplink::parse("punktfunk://pair").unwrap_err().message();
        assert_eq!(
            msg,
            "Pairing can't be done from a link \u{2014} pair the host in Punktfunk first."
        );
        for s in [
            Screen::Home,
            Screen::Settings,
            Screen::WakeParked,
            Screen::Pair,
        ] {
            assert_eq!(
                gate_link("punktfunk://pair", &s, false),
                LinkGate::Refuse(msg.clone())
            );
        }
        let msg = deeplink::parse("https://example.com")
            .unwrap_err()
            .message();
        assert_eq!(msg, "That isn't a Punktfunk link.");
        assert_eq!(
            gate_link("https://example.com", &Screen::Home, false),
            LinkGate::Refuse(msg)
        );
    }

    #[test]
    fn a_bad_link_during_a_connect_leaves_the_screen() {
        let msg = deeplink::parse("punktfunk://pair").unwrap_err().message();
        for s in [
            Screen::Stream,
            Screen::Connecting,
            Screen::Waking,
            Screen::RequestAccess,
        ] {
            assert_eq!(
                gate_link("punktfunk://pair", &s, false),
                LinkGate::Hold(msg.clone())
            );
        }
        assert_eq!(
            gate_link("punktfunk://pair", &Screen::Home, true),
            LinkGate::Hold(msg)
        );
    }

    #[test]
    fn the_link_prompt_names_the_host_and_the_game() {
        assert_eq!(
            link_prompt("Desk", "192.168.1.5", None, None),
            "A link asks to connect to Desk (192.168.1.5).\n\nIt names the host by its label \
             or address, which anything that can open a link could guess. Shortcuts made in \
             Punktfunk name the host's id and connect without asking."
        );
        assert_eq!(
            link_prompt("Desk", "192.168.1.5", Some("steam:570"), None),
            "A link asks to connect to Desk (192.168.1.5).\n\nIt also asks the host to launch \
             \u{201c}steam:570\u{201d}.\n\nIt names the host by its label or address, which \
             anything that can open a link could guess. Shortcuts made in Punktfunk name the \
             host's id and connect without asking."
        );
        let set = pf_client_core::library::GameOverrides {
            lines: Some(1080),
            ..Default::default()
        };
        assert!(
            link_prompt("Desk", "192.168.1.5", Some("steam:570"), Some(&set))
                .contains("\u{201d}.\n\nHost: 1080p\n\nIt names")
        );
    }

    #[test]
    fn the_pane_follows_the_width_until_the_toggle_speaks() {
        assert_eq!(pane(1200.0, None, false), Pane::Beside(true));
        assert_eq!(pane(760.0, None, false), Pane::Beside(false));
        assert_eq!(pane(1200.0, Some(false), false), Pane::Beside(false));
        assert_eq!(pane(760.0, Some(true), false), Pane::Beside(true));
        // A narrow window takes the pane off the page whatever the toggle said beside it.
        assert_eq!(pane(480.0, Some(true), false), Pane::Over(false));
        assert_eq!(pane(480.0, None, true), Pane::Over(true));
    }

    #[test]
    fn back_leaves_a_drill_in_and_nothing_else() {
        let home = |dest, host, hidden| back(&Screen::Home, dest, host, hidden);
        assert_eq!(home(Dest::Hosts, false, false), None);
        assert_eq!(home(Dest::Hosts, true, false), Some(Back::HostList));
        assert_eq!(home(Dest::Library, false, true), Some(Back::Shelf));
        // A host page left open under the Library is not what the Library goes back to.
        assert_eq!(home(Dest::Library, true, false), None);
        for s in ALL {
            let want = match s {
                Screen::Settings | Screen::Help | Screen::SpeedTest => {
                    Some(Back::Screen(Screen::Home))
                }
                Screen::Licenses => Some(Back::Screen(Screen::Settings)),
                _ => None,
            };
            assert_eq!(back(&s, Dest::Hosts, false, false), want);
        }
    }

    #[test]
    fn keys_map_to_the_shell_shortcuts() {
        assert_eq!(shortcut(0x31, true, false), Some(Shortcut::Hosts));
        assert_eq!(shortcut(0x32, true, false), Some(Shortcut::Library));
        assert_eq!(shortcut(0x46, true, false), Some(Shortcut::Search));
        assert_eq!(shortcut(0x4E, true, false), Some(Shortcut::AddHost));
        assert_eq!(shortcut(0x52, true, false), Some(Shortcut::Reload));
        assert_eq!(shortcut(0x74, false, false), Some(Shortcut::Reload));
        assert_eq!(shortcut(0xBC, true, false), Some(Shortcut::Settings));
        assert_eq!(shortcut(0x25, false, true), Some(Shortcut::Back));
        // A bare letter is typing, and AltGr (Ctrl+Alt) is a character.
        assert_eq!(shortcut(0x46, false, false), None);
        assert_eq!(shortcut(0x31, true, true), None);
    }
}
