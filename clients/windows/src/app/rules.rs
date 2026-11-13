//! The shell's decisions as plain functions over plain data: what holds the console back, how a
//! link meets the shell, how an advert changes the host list. `mod.rs` owns the reactor wiring
//! and calls these, so a table test pins each rule without a window.

use super::Screen;
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

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Screen; 13] = [
        Screen::Trust,
        Screen::Hosts,
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
        Screen::Library,
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
            Screen::Hosts,
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
            gate_link("https://example.com", &Screen::Hosts, false),
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
            gate_link("punktfunk://pair", &Screen::Hosts, true),
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
}
