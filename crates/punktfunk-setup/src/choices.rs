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

/// What `--web-bind` writes for "this machine only".
pub const LOOPBACK_BIND: &str = "127.0.0.1";

/// The console's default listen address (`PUNKTFUNK_UI_BIND` in host.env): every interface. The
/// console answers only peers on the local network or a VPN, never the internet.
pub const LAN_BIND: &str = "0.0.0.0";

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
    pub group_why: Option<String>,
    pub linger_why: Option<String>,
}

impl Choices {
    pub fn derive(facts: &Facts, pins: &Pins) -> Choices {
        let (channel, switch_from) = resolve_channel(facts, pins.channel);

        // The SteamOS build script joins both groups, enables linger and starts the units
        // itself, and there is no flag to tell it otherwise. Pin those rows to what will
        // happen rather than let a toggle promise something nothing reads.
        let steamos = facts.family == Family::Steamos;

        let couch_why = match facts.os.id.as_str() {
            "bazzite" => "Bazzite",
            "nobara" => "Nobara",
            _ => "Game Mode / HTPC box",
        };
        let linger_why = if steamos {
            Some("the SteamOS installer always enables it".to_string())
        } else if facts.couch_box {
            Some(format!("{couch_why} hosts are usually headless"))
        } else if !facts.graphical_seat {
            Some("no graphical session".to_string())
        } else {
            None
        };
        let group_why = if steamos {
            Some("the SteamOS installer always joins it".to_string())
        } else {
            facts
                .couch_box
                .then(|| format!("{couch_why} — virtual Steam Deck pad"))
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
            move_mgmt_port: facts.sunshine_active,
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
    use crate::facts::{Family, Firewall, Nvidia, OsRelease};

    /// A box with nothing on it, which every case below varies one field of.
    fn fresh(id: &str, family: Family) -> Facts {
        Facts {
            os: OsRelease {
                id: id.into(),
                id_like: String::new(),
                version_id: String::new(),
                pretty: id.into(),
            },
            family,
            omarchy: id == "omarchy",
            docs_page: String::new(),
            host_punt: None,
            has_flatpak_client: false,
            rpm_group: None,
            floor: None,
            couch_box: id == "bazzite" || id == "nobara",
            graphical_seat: true,
            desktop_sessions: true,
            sunshine_active: false,
            current_channel: None,
            installed_pf: vec![],
            missing: vec!["host".into(), "web-console".into(), "plugin-runner".into()],
            host_version: None,
            has_web_server: false,
            has_omarchy_bin: false,
            has_ujust: false,
            in_input_group: false,
            in_punktfunk_group: false,
            has_input_group: true,
            nvidia: Nvidia::Absent,
            firewall: Firewall::None,
            systemd_pid1: true,
            user_manager: true,
            web_unit_present: true,
            web_password_present: false,
            web_bind: None,
            scripting_unit_disabled: false,
            ip: Some("192.168.1.10".into()),
            user: "pf".into(),
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
        assert_eq!(
            couch.group_why.as_deref(),
            Some("Bazzite — virtual Steam Deck pad")
        );

        let mut seatless = fresh("debian", Family::Apt);
        seatless.graphical_seat = false;
        let seatless = Choices::derive(&seatless, &Pins::default());
        assert!(seatless.linger);
        assert_eq!(seatless.linger_why.as_deref(), Some("no graphical session"));

        let mut sunshine = fresh("fedora", Family::Dnf);
        sunshine.sunshine_active = true;
        let sunshine = Choices::derive(&sunshine, &Pins::default());
        assert!(
            !sunshine.gamestream,
            "Sunshine keeps the Moonlight ports, so compat stays off"
        );
        assert!(sunshine.move_mgmt_port);
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
