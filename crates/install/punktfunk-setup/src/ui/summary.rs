//! Settings-list state: rows, cursor, and key transitions. No terminal, no I/O.
//!
//! Every resolved choice is visible at once. Enter on Go installs; ↑↓ + Enter
//! edits one row. The row model lives here so tests inject keys; swapping the
//! renderer stays a rendering change.
//!
//! The screen owns `Choices`. `plan()` rebuilds from `(Facts, Choices)` on
//! each call, so a Moonlight-compat toggle adds the gamestream firewall line
//! with no cache to invalidate.
//!
//! Row `why` texts match the sh installer verbatim so a default still names
//! the usbip-attach grant and that each client opts in.
//!
//! Pin `lines()`. Evidence: `design/installer-v2.md`.

use crate::choices::{Action, Choices};
use crate::facts::{Channel, Facts, Family};
use crate::plan::{self, Plan};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Components,
    Channel,
    Group,
    Gamestream,
    Clipboard,
    Linger,
    Start,
    OmarchyToasts,
    OmarchyIdle,
    OmarchyTheme,
}

/// Separators in the mockup are drawn, never selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    /// First so Enter on arrival installs.
    Go,
    Row(Field),
    Uninstall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    /// Must not move the cursor.
    Ignored,
}

/// `Edit` is where the renderer opens its own prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Idle,
    Edit(Field),
    Run(Action),
    Cancel,
}

pub struct Screen {
    pub facts: Facts,
    pub choices: Choices,
    pub items: Vec<Item>,
    pub cursor: usize,
}

impl Screen {
    pub fn new(facts: Facts, choices: Choices) -> Screen {
        // SteamOS's build script owns the groups, linger and the unit start. Those rows would be
        // toggles nothing reads, so they are not offered. Its channel is the checkout's branch.
        let steamos = facts.family == Family::Steamos;
        let mut items = vec![Item::Go];
        items.extend([Item::Row(Field::Components), Item::Row(Field::Channel)]);
        // A client listens on nothing fixed, so none of the host wiring rows apply to it.
        if choices.components.host {
            if !steamos {
                items.push(Item::Row(Field::Group));
            }
            items.extend([Item::Row(Field::Gamestream), Item::Row(Field::Clipboard)]);
            // Linger only earns a row where it changes something: on a box with a graphical
            // seat the host still waits for a session, so the row would promise an appliance.
            if !steamos && (!facts.graphical_seat || facts.couch_box) {
                items.push(Item::Row(Field::Linger));
            }
            if !steamos {
                items.push(Item::Row(Field::Start));
            }
            // Omarchy's own options, asked here instead of again by `punktfunk-omarchy setup`.
            if facts.omarchy {
                items.extend([
                    Item::Row(Field::OmarchyToasts),
                    Item::Row(Field::OmarchyIdle),
                    Item::Row(Field::OmarchyTheme),
                ]);
            }
        }
        if facts.fully_installed() {
            items.push(Item::Uninstall);
        }
        Screen {
            facts,
            choices,
            items,
            cursor: 0,
        }
    }

    /// Installed host: retitle and offer Uninstall.
    pub fn manage_mode(&self) -> bool {
        self.facts.fully_installed()
    }

    pub fn title(&self) -> String {
        if self.manage_mode() {
            let version = self
                .facts
                .host_version
                .clone()
                .unwrap_or_else(|| "punktfunk".into());
            let channel = self
                .facts
                .current_channel
                .map_or("no repo", Channel::as_str);
            format!("{version} · {channel}")
        } else {
            format!("punktfunk setup{}", self.suffix())
        }
    }

    fn suffix(&self) -> String {
        format!(
            "  ·  {} · {}",
            self.facts.os.pretty,
            self.facts.family.as_str()
        )
    }

    pub fn selected(&self) -> Item {
        self.items[self.cursor]
    }

    pub fn key(&mut self, key: Key) -> Step {
        match key {
            Key::Up => {
                self.cursor = (self.cursor + self.items.len() - 1) % self.items.len();
                Step::Idle
            }
            Key::Down => {
                self.cursor = (self.cursor + 1) % self.items.len();
                Step::Idle
            }
            Key::Ignored => Step::Idle,
            Key::Cancel => Step::Cancel,
            Key::Enter => match self.selected() {
                Item::Go => Step::Run(Action::Install),
                Item::Uninstall => Step::Run(Action::Uninstall),
                Item::Row(field) => Step::Edit(field),
            },
        }
    }

    pub fn set_bool(&mut self, field: Field, on: bool) {
        match field {
            Field::Group => self.choices.punktfunk_group = on,
            Field::Gamestream => self.choices.gamestream = on,
            Field::Clipboard => self.choices.clipboard = on,
            Field::Linger => self.choices.linger = on,
            Field::Start => self.choices.start = on,
            Field::OmarchyToasts => self.choices.omarchy_toasts = on,
            Field::OmarchyIdle => self.choices.omarchy_idle = on,
            Field::OmarchyTheme => self.choices.omarchy_theme = on,
            _ => return,
        }
        self.rederive();
    }

    pub fn set_channel(&mut self, channel: Channel) {
        self.choices.channel = channel;
        self.choices.switch_from = match self.facts.current_channel {
            Some(current) if current != channel => Some(current),
            _ => None,
        };
    }

    pub fn set_components(&mut self, host: bool, client: bool) {
        // Neither selected is not a thing to install; keep the host rather than do nothing.
        self.choices.components.host = host || !client;
        self.choices.components.client = client;
        let rebuilt = Screen::new(self.facts.clone(), self.choices.clone());
        self.items = rebuilt.items;
        self.cursor = self.cursor.min(self.items.len() - 1);
    }

    /// An off row must not explain a grant it is not making.
    fn rederive(&mut self) {
        if !self.choices.punktfunk_group {
            self.choices.group_why = None;
        }
        if !self.choices.linger {
            self.choices.linger_why = None;
        }
    }

    pub fn plan(&self) -> Plan {
        plan::build(&self.facts, &self.choices)
    }

    pub fn label(field: Field) -> &'static str {
        match field {
            Field::Components => "This machine is",
            Field::Channel => "Channel",
            Field::Group => "Full controller support",
            Field::Gamestream => "Third-party clients",
            Field::Clipboard => "Shared clipboard",
            Field::Linger => "Start at boot",
            Field::Start => "Host service",
            Field::OmarchyToasts => "Desktop notifications",
            Field::OmarchyIdle => "Keep the screen awake",
            Field::OmarchyTheme => "Match the Omarchy theme",
        }
    }

    pub fn value(&self, field: Field) -> String {
        let c = &self.choices;
        let yn = |on: bool| if on { "yes" } else { "no " };
        match field {
            // The download page's words, so the terminal reads like the page that sent you here.
            Field::Components => match (c.components.host, c.components.client) {
                (true, true) => "the host, plus the client app".into(),
                (false, true) => "the device I play on (client)".into(),
                _ => "the PC I stream from (host)".into(),
            },
            Field::Channel => match c.switch_from {
                Some(from) => format!("{} → {}", from.as_str(), c.channel.as_str()),
                None => c.channel.as_str().to_string(),
            },
            Field::Group => match (&c.group_why, c.punktfunk_group) {
                (Some(why), true) => format!("yes — {why}"),
                (_, true) => "yes — joins the punktfunk group (grants usbip attach)".into(),
                _ => "no  — the pad arrives as a plain Xbox 360 controller".into(),
            },
            Field::Gamestream if c.gamestream => "yes".into(),
            Field::Gamestream => "no  — punktfunk's own apps don't need it".into(),
            Field::Clipboard => yn(c.clipboard).trim_end().to_string(),
            Field::Linger => match (&c.linger_why, c.linger) {
                (Some(why), true) => format!("yes — {why}"),
                (_, true) => "yes".into(),
                _ => "no  — the host waits for your session".into(),
            },
            Field::Start => yn(c.start).trim_end().to_string(),
            Field::OmarchyToasts => yn(c.omarchy_toasts).trim_end().to_string(),
            Field::OmarchyIdle => yn(c.omarchy_idle).trim_end().to_string(),
            Field::OmarchyTheme => yn(c.omarchy_theme).trim_end().to_string(),
        }
    }

    /// Editor prompt: the sh installer's text, verbatim.
    pub fn why(field: Field) -> &'static str {
        match field {
            Field::Components => "Host: the PC you stream from. Client: the device you play on. One PC can be both.",
            Field::Channel => "canary is the latest main build; stable is the released one.",
            Field::Group => "Joins the punktfunk group for paddles, trackpads and gyro. It grants usbip attach, so only on a machine you trust.",
            Field::Gamestream => "Serve Moonlight, Artemis, or another third-party client. Punktfunk's own apps don't need this.",
            Field::Clipboard => "Share the clipboard between this host and your clients. Each client still opts in per host.",
            Field::Linger => "Starts the host at boot so it is reachable before anyone logs in. It can only stream once a session exists.",
            Field::Start => "Enable and start the services once the install finishes. Off installs the files and starts nothing.",
            Field::OmarchyToasts => "A toast when a device asks to pair, and when a stream starts or stops.",
            Field::OmarchyIdle => "Holds off the idle lock for as long as a stream is running.",
            Field::OmarchyTheme => "The console follows whatever theme Omarchy is on, instead of its own palette.",
        }
    }

    /// Golden and plain-mode text of the screen.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![self.title()];
        let rule = "─".repeat(63);
        out.push(rule.clone());
        for item in &self.items {
            if let Item::Row(field) = item {
                out.push(format!(
                    "  {:<25}{}",
                    Screen::label(*field),
                    self.value(*field)
                ));
            }
        }
        out.push(rule);
        let actions: Vec<&str> = self
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Uninstall => Some("Uninstall"),
                _ => None,
            })
            .collect();
        out.push(format!("  {}", actions.join(" · ")));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::choices::Pins;
    use crate::facts::{Family, Firewall};

    fn facts(id: &str, family: Family) -> Facts {
        Facts {
            missing: vec!["host".into()],
            firewall: Firewall::Ufw,
            ip: None,
            ..crate::fixtures::fresh_facts(id, family)
        }
    }

    fn screen() -> Screen {
        let f = facts("arch", Family::Pacman);
        let c = Choices::derive(&f, &Pins::default());
        Screen::new(f, c)
    }

    #[test]
    fn enter_on_arrival_installs() {
        let mut s = screen();
        assert_eq!(s.selected(), Item::Go);
        assert_eq!(s.key(Key::Enter), Step::Run(Action::Install));
    }

    #[test]
    fn the_cursor_moves_and_wraps() {
        let mut s = screen();
        let last = s.items.len() - 1;
        assert_eq!(s.key(Key::Up), Step::Idle);
        assert_eq!(s.cursor, last, "up from the top wraps to the bottom");
        assert_eq!(s.key(Key::Down), Step::Idle);
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn enter_on_a_row_asks_the_renderer_to_edit_it() {
        let mut s = screen();
        s.key(Key::Down);
        s.key(Key::Down);
        assert_eq!(s.key(Key::Enter), Step::Edit(Field::Channel));
    }

    #[test]
    fn cancel_leaves_without_running_anything() {
        let mut s = screen();
        assert_eq!(s.key(Key::Cancel), Step::Cancel);
    }

    #[test]
    fn turning_moonlight_compat_on_adds_the_gamestream_firewall_line() {
        let mut s = screen();
        assert!(!s
            .plan()
            .commands()
            .iter()
            .any(|c| c.contains("punktfunk-gamestream")));
        s.set_bool(Field::Gamestream, true);
        assert!(
            s.plan()
                .commands()
                .iter()
                .any(|c| c == "sudo ufw allow punktfunk-gamestream"),
            "the firewall step did not appear: {:?}",
            s.plan().commands()
        );
    }

    #[test]
    fn turning_the_group_off_drops_the_usermod_and_its_why_text() {
        let mut s = screen();
        assert!(s
            .plan()
            .commands()
            .iter()
            .any(|c| c.contains("usermod -aG punktfunk")));
        s.set_bool(Field::Group, false);
        assert!(!s
            .plan()
            .commands()
            .iter()
            .any(|c| c.contains("usermod -aG punktfunk")));
        assert!(s.choices.group_why.is_none());
    }

    #[test]
    fn the_group_row_always_states_the_usbip_grant() {
        let s = screen();
        assert!(s.value(Field::Group).contains("usbip attach"));
        assert!(Screen::why(Field::Group).contains("usbip attach"));
    }

    #[test]
    fn choosing_a_channel_the_box_is_not_on_shows_it_as_a_switch() {
        let mut f = facts("arch", Family::Pacman);
        f.current_channel = Some(Channel::Stable);
        let c = Choices::derive(&f, &Pins::default());
        let mut s = Screen::new(f, c);
        s.set_channel(Channel::Canary);
        assert_eq!(s.value(Field::Channel), "stable → canary");
        s.set_channel(Channel::Stable);
        assert_eq!(
            s.value(Field::Channel),
            "stable",
            "back where it started is not a switch"
        );
    }

    #[test]
    fn a_client_only_screen_drops_every_host_row() {
        let f = facts("debian", Family::Apt);
        let c = Choices::derive(
            &f,
            &Pins {
                client: true,
                ..Pins::default()
            },
        );
        let s = Screen::new(f, c);
        let fields: Vec<Field> = s
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Row(f) => Some(*f),
                _ => None,
            })
            .collect();
        assert_eq!(fields, [Field::Components, Field::Channel]);
    }

    #[test]
    fn switching_components_to_client_only_reshapes_the_screen() {
        let mut s = screen();
        s.set_components(false, true);
        assert!(!s.items.contains(&Item::Row(Field::Group)));
        assert!(s.cursor < s.items.len(), "the cursor was left past the end");
    }

    #[test]
    fn an_installed_box_re_titles_and_offers_uninstall() {
        let mut f = facts("arch", Family::Pacman);
        f.missing = vec![];
        f.host_version = Some("punktfunk-host 0.34.0".into());
        f.current_channel = Some(Channel::Canary);
        let c = Choices::derive(&f, &Pins::default());
        let s = Screen::new(f, c);
        assert!(s.manage_mode());
        assert_eq!(s.title(), "punktfunk-host 0.34.0 · canary");
        assert!(s.items.contains(&Item::Uninstall));
    }

    #[test]
    fn a_fresh_box_offers_no_uninstall() {
        assert!(!screen().items.contains(&Item::Uninstall));
    }
}
