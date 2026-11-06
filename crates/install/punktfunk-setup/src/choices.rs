//! Stage two: the editable option set. Every default is derived from `Facts`.
//!
//! `design/installer-v2.md` is the contract; `defaults_table` below is that
//! table as a test. Flags and env twins pin a row before any UI shows it, so
//! `--yes` and the TUI resolve identically.
//!
//! Two defaults changed from the sh installer on purpose. The punktfunk group is **yes
//! everywhere**, not couch-boxes-only: it grants usbip attach, so the row label names the
//! grant in plain mode too and `--no-punktfunk-group` stays the opt-out. The clipboard is
//! **yes** as well, since each client opts in per host anyway.
//!
//! The Omarchy rows live here rather than in `punktfunk-omarchy`, which used to ask for
//! them itself. `omarchy_setup` stays the umbrella every row defaults to, so the frozen
//! `--no-omarchy-setup` still clears all three. The console certificate is not one of them:
//! every host install trusts it, Omarchy through the hand-off, everyone else in `exec`.

use serde::{Deserialize, Serialize};

use crate::facts::{Channel, Facts, Family};

/// The management API's home when Sunshine already holds 47990.
pub const DEFAULT_MGMT_PORT: u16 = 47991;

/// What "this machine only" means to the console's listener.
pub const LOOPBACK_BIND: &str = "127.0.0.1";

/// The console's default listen address (`PUNKTFUNK_UI_BIND` in host.env): every interface. The
/// console answers only peers on the local network or a VPN, never the internet.
pub const LAN_BIND: &str = "0.0.0.0";

/// `--web-bind`, `/WEBBIND` and their env twin: an address, `localhost`/`loopback` or
/// `lan`/`any`. `None` for anything else; the caller decides whether that fails the run.
pub fn parse_web_bind(raw: &str) -> Option<String> {
    match raw.trim() {
        "localhost" | "loopback" => Some(LOOPBACK_BIND.to_string()),
        "lan" | "any" => Some(LAN_BIND.to_string()),
        v if v.parse::<std::net::IpAddr>().is_ok() => Some(v.to_string()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    #[default]
    Install,
    Uninstall,
}

/// What to put on the box. Neither flag given means host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Components {
    pub host: bool,
    pub client: bool,
}

/// Everything the CLI pinned. `None` is "let the box decide".
#[derive(Debug, Clone, Default)]
pub struct Pins {
    pub action: Action,
    pub host: bool,
    pub client: bool,
    pub channel: Option<Channel>,
    pub gamestream: Option<bool>,
    pub clipboard: Option<bool>,
    pub punktfunk_group: Option<bool>,
    pub linger: Option<bool>,
    pub omarchy_setup: Option<bool>,
    pub omarchy_toasts: Option<bool>,
    pub omarchy_idle: Option<bool>,
    pub omarchy_theme: Option<bool>,
    pub console_cert: Option<bool>,
    pub mgmt_port: Option<u16>,
    pub web_bind: Option<String>,
    pub no_start: bool,
}

/// The couch-box image a default names. `Other` is any Game Mode / HTPC box without a name of its
/// own here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Couch {
    Bazzite,
    Nobara,
    Other,
}

/// Why the box chose a default: stored as the cause and worded where it is shown, so a
/// transcript prints English and a screen prints the person's language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Why {
    /// SteamOS' script always enables linger.
    SteamosLinger,
    /// SteamOS' script always joins the group.
    SteamosGroup,
    CouchHeadless(Couch),
    CouchPad(Couch),
    NoGraphicalSession,
}

impl Why {
    pub fn text(self) -> String {
        use pf_l10n::tools as t;
        let name = |couch| match couch {
            Couch::Bazzite => Some("Bazzite"),
            Couch::Nobara => Some("Nobara"),
            Couch::Other => None,
        };
        match self {
            Why::SteamosLinger => t::setup_why_linger_steamos().to_string(),
            Why::SteamosGroup => t::setup_why_group_steamos().to_string(),
            Why::NoGraphicalSession => t::setup_why_no_graphical_session().to_string(),
            Why::CouchHeadless(couch) => match name(couch) {
                Some(distro) => t::setup_why_couch_headless(distro),
                None => t::setup_why_couch_box_headless().to_string(),
            },
            Why::CouchPad(couch) => match name(couch) {
                Some(distro) => t::setup_why_group_couch(distro),
                None => t::setup_why_group_couch_box().to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Choices {
    pub action: Action,
    pub components: Components,
    pub channel: Channel,
    /// Set when `--channel` named a channel the box is not on — a switch, not an install.
    pub switch_from: Option<Channel>,
    pub punktfunk_group: bool,
    pub gamestream: bool,
    pub clipboard: bool,
    pub linger: bool,
    pub start: bool,
    /// Only true when a conflict was actually detected; the row does not surface otherwise.
    pub move_mgmt_port: bool,
    pub mgmt_port: u16,
    pub omarchy_setup: bool,
    pub omarchy_toasts: bool,
    pub omarchy_idle: bool,
    pub omarchy_theme: bool,
    /// Trust the console's certificate in the user's browser store. On by default on every
    /// host install and never a row on the screen: it is one certificate, this machine's own.
    pub console_cert: bool,
    /// Where the web console listens (`PUNKTFUNK_UI_BIND`). The local network unless the user or
    /// `--web-bind` says otherwise; the plugin-UI origin on 47993 follows it.
    pub web_bind: String,
    /// The console login password the user typed. `None` leaves it to the console's own
    /// first start, which generates one into `~/.config/punktfunk/web-password`. Never
    /// echoed: `exec` writes the file, the plan step carries no value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_password: Option<String>,
    /// Why the box chose these — shown next to the row when it is on.
    pub group_why: Option<Why>,
    pub linger_why: Option<Why>,
}

impl Choices {
    pub fn derive(facts: &Facts, pins: &Pins) -> Choices {
        let (channel, switch_from) = resolve_channel(facts, pins.channel);

        // The SteamOS build script joins both groups, enables linger and starts the units
        // itself, and there is no flag to tell it otherwise. Pin those rows to what will
        // happen rather than let a toggle promise something nothing reads.
        let steamos = facts.family == Family::Steamos;

        let couch = match facts.os.id.as_str() {
            "bazzite" => Couch::Bazzite,
            "nobara" => Couch::Nobara,
            _ => Couch::Other,
        };
        let linger_why = if steamos {
            Some(Why::SteamosLinger)
        } else if facts.couch_box {
            Some(Why::CouchHeadless(couch))
        } else if !facts.graphical_seat {
            Some(Why::NoGraphicalSession)
        } else {
            None
        };
        let group_why = if steamos {
            Some(Why::SteamosGroup)
        } else {
            facts.couch_box.then_some(Why::CouchPad(couch))
        };

        let omarchy_setup = pins.omarchy_setup.unwrap_or(true);
        let punktfunk_group = steamos || pins.punktfunk_group.unwrap_or(true);
        // Off next to Sunshine too: both hosts bind the Moonlight ports.
        let gamestream = pins.gamestream.unwrap_or(false);
        let linger = steamos
            || pins
                .linger
                .unwrap_or(facts.couch_box || !facts.graphical_seat);

        Choices {
            action: pins.action,
            components: Components {
                // Neither flag = host. `--client` alone is client-only.
                host: pins.host || !pins.client,
                client: pins.client,
            },
            channel,
            switch_from,
            punktfunk_group,
            gamestream,
            clipboard: pins.clipboard.unwrap_or(true),
            linger,
            start: steamos || !pins.no_start,
            // An operator's own bind stays unless `--mgmt-port` names a new one.
            move_mgmt_port: facts.sunshine_active
                && (facts.mgmt_bind.is_none() || pins.mgmt_port.is_some()),
            mgmt_port: pins.mgmt_port.unwrap_or(DEFAULT_MGMT_PORT),
            omarchy_setup,
            omarchy_toasts: pins.omarchy_toasts.unwrap_or(omarchy_setup),
            omarchy_idle: pins.omarchy_idle.unwrap_or(omarchy_setup),
            omarchy_theme: pins.omarchy_theme.unwrap_or(omarchy_setup),
            // The trust lands in this box's browser store; a box with no desktop has none.
            console_cert: pins.console_cert.unwrap_or(facts.desktop_sessions),
            // Flag, then whatever host.env already names, then the LAN. The middle arm is what
            // keeps a re-run from moving a console the operator placed: a bare Enter through the
            // question re-writes the value the box is already living by.
            web_bind: pins
                .web_bind
                .clone()
                .or_else(|| facts.web_bind.clone())
                .unwrap_or_else(|| LAN_BIND.to_string()),
            // Asked after the settings screen, so nothing derives it here.
            web_password: None,
            group_why: punktfunk_group.then_some(group_why).flatten(),
            linger_why: linger.then_some(linger_why).flatten(),
        }
    }
}

/// The channel-stickiness trap: without an explicit `--channel` the box wins, so a bare re-run
/// on a canary machine is never dragged back to stable.
fn resolve_channel(facts: &Facts, pinned: Option<Channel>) -> (Channel, Option<Channel>) {
    match (pinned, facts.current_channel) {
        (Some(asked), Some(current)) if asked != current => (asked, Some(current)),
        (Some(asked), _) => (asked, None),
        (None, Some(current)) => (current, None),
        (None, None) => (Channel::Stable, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::Family;
    use crate::fixtures::fresh_facts as fresh;

    /// One grammar for `--web-bind` and `/WEBBIND`; only the caller's strictness differs.
    #[test]
    fn web_bind_grammar() {
        for (raw, want) in [
            ("localhost", Some(LOOPBACK_BIND)),
            (" loopback ", Some(LOOPBACK_BIND)),
            ("lan", Some(LAN_BIND)),
            ("any", Some(LAN_BIND)),
            ("192.168.1.24", Some("192.168.1.24")),
            ("::1", Some("::1")),
            ("", None),
            ("lan-only", None),
            ("192.168.1", None),
        ] {
            assert_eq!(parse_web_bind(raw).as_deref(), want, "{raw:?}");
        }
    }

    // `design/installer-v2.md` defaults. A row changing here is a product decision.
    #[test]
    fn defaults_table() {
        let desktop = Choices::derive(&fresh("arch", Family::Pacman), &Pins::default());
        assert_eq!(
            desktop.components,
            Components {
                host: true,
                client: false
            }
        );
        assert_eq!(desktop.channel, Channel::Stable);
        assert!(
            desktop.punktfunk_group,
            "the D4 flip: yes on every box, not couch-only"
        );
        assert!(!desktop.gamestream);
        assert!(
            desktop.clipboard,
            "the D4 flip: the clipboard is on by default"
        );
        assert!(!desktop.linger, "a desktop has a graphical seat");
        assert!(desktop.start);
        assert!(
            !desktop.move_mgmt_port,
            "the row only surfaces on a detected conflict"
        );
        assert_eq!(desktop.mgmt_port, DEFAULT_MGMT_PORT);

        let couch = Choices::derive(&fresh("bazzite", Family::Sysext), &Pins::default());
        assert!(couch.punktfunk_group);
        assert!(couch.linger, "a couch box starts at boot");
        assert_eq!(couch.group_why, Some(Why::CouchPad(Couch::Bazzite)));
        assert_eq!(
            couch.group_why.map(Why::text).as_deref(),
            Some("Bazzite — virtual Steam Deck pad")
        );

        let mut seatless = fresh("debian", Family::Apt);
        seatless.graphical_seat = false;
        let seatless = Choices::derive(&seatless, &Pins::default());
        assert!(seatless.linger);
        assert_eq!(seatless.linger_why, Some(Why::NoGraphicalSession));
        assert_eq!(
            seatless.linger_why.map(Why::text).as_deref(),
            Some("no graphical session")
        );

        let mut sunshine = fresh("fedora", Family::Dnf);
        sunshine.sunshine_active = true;
        let sunshine = Choices::derive(&sunshine, &Pins::default());
        assert!(
            !sunshine.gamestream,
            "Sunshine keeps the Moonlight ports, so compat stays off"
        );
        assert!(sunshine.move_mgmt_port);

        let mut placed = fresh("fedora", Family::Dnf);
        placed.sunshine_active = true;
        placed.mgmt_bind = Some("127.0.0.1:48010".into());
        assert!(
            !Choices::derive(&placed, &Pins::default()).move_mgmt_port,
            "a re-run must not widen the operator's own bind"
        );
    }

    // A reason is stored as its cause; the English a transcript prints is the message's.
    #[test]
    fn a_reason_is_worded_where_it_is_shown() {
        for (why, text) in [
            (
                Why::CouchHeadless(Couch::Nobara),
                "Nobara hosts are usually headless",
            ),
            (
                Why::CouchHeadless(Couch::Other),
                "Game Mode / HTPC box hosts are usually headless",
            ),
            (
                Why::CouchPad(Couch::Other),
                "Game Mode / HTPC box — virtual Steam Deck pad",
            ),
            (
                Why::SteamosLinger,
                "the SteamOS installer always enables it",
            ),
            (Why::SteamosGroup, "the SteamOS installer always joins it"),
        ] {
            assert_eq!(why.text(), text);
        }
    }

    #[test]
    fn a_flag_beats_the_box() {
        let mut couch = fresh("bazzite", Family::Sysext);
        couch.sunshine_active = true;
        let pins = Pins {
            punktfunk_group: Some(false),
            linger: Some(false),
            gamestream: Some(false),
            ..Pins::default()
        };
        let c = Choices::derive(&couch, &pins);
        assert!(!c.punktfunk_group);
        assert!(!c.linger);
        assert!(!c.gamestream);
        assert!(
            c.group_why.is_none(),
            "a why-text never outlives the row being off"
        );
    }

    #[test]
    fn a_bare_re_run_follows_the_box_and_never_switches() {
        let mut canary = fresh("arch", Family::Pacman);
        canary.current_channel = Some(Channel::Canary);
        let c = Choices::derive(&canary, &Pins::default());
        assert_eq!(c.channel, Channel::Canary);
        assert_eq!(c.switch_from, None);
    }

    #[test]
    fn an_explicit_channel_on_a_box_already_there_is_not_a_switch() {
        let mut canary = fresh("arch", Family::Pacman);
        canary.current_channel = Some(Channel::Canary);
        let pins = Pins {
            channel: Some(Channel::Canary),
            ..Pins::default()
        };
        assert_eq!(Choices::derive(&canary, &pins).switch_from, None);
    }

    #[test]
    fn an_explicit_channel_the_box_is_not_on_is_a_switch_in_both_directions() {
        let mut canary = fresh("arch", Family::Pacman);
        canary.current_channel = Some(Channel::Canary);
        let to_stable = Pins {
            channel: Some(Channel::Stable),
            ..Pins::default()
        };
        let c = Choices::derive(&canary, &to_stable);
        assert_eq!(c.channel, Channel::Stable);
        assert_eq!(c.switch_from, Some(Channel::Canary));

        let mut stable = fresh("arch", Family::Pacman);
        stable.current_channel = Some(Channel::Stable);
        let to_canary = Pins {
            channel: Some(Channel::Canary),
            ..Pins::default()
        };
        assert_eq!(
            Choices::derive(&stable, &to_canary).switch_from,
            Some(Channel::Stable)
        );
    }

    // No repo (a source build): `--channel` sets one; there is nothing to switch from.
    #[test]
    fn a_channel_flag_on_a_box_with_no_repo_is_not_a_switch() {
        let pins = Pins {
            channel: Some(Channel::Canary),
            ..Pins::default()
        };
        let c = Choices::derive(&fresh("arch", Family::Pacman), &pins);
        assert_eq!(c.channel, Channel::Canary);
        assert_eq!(c.switch_from, None);
    }

    #[test]
    fn client_only_drops_the_host() {
        let pins = Pins {
            client: true,
            ..Pins::default()
        };
        let c = Choices::derive(&fresh("debian", Family::Apt), &pins);
        assert_eq!(
            c.components,
            Components {
                host: false,
                client: true
            }
        );
    }
}
