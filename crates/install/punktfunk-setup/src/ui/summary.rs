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

use pf_l10n::tools as t;

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

impl Field {
    const ALL: [Field; 10] = [
        Field::Components,
        Field::Channel,
        Field::Group,
        Field::Gamestream,
        Field::Clipboard,
        Field::Linger,
        Field::Start,
        Field::OmarchyToasts,
        Field::OmarchyIdle,
        Field::OmarchyTheme,
    ];
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
                .map_or(t::setup_channel_none(), Channel::as_str);
            format!("{version} · {channel}")
        } else {
            format!("{}{}", t::setup_title(), self.suffix())
        }
    }

    /// The distro and how the host gets onto it. SteamOS builds on the device, so its method is
    /// words; the others are a package manager's name.
    fn suffix(&self) -> String {
        let method = match self.facts.family {
            Family::Steamos => t::setup_family_on_device(),
            family => family.as_str(),
        };
        format!("  ·  {} · {method}", self.facts.os.pretty)
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
            Field::Components => t::setup_row_components(),
            Field::Channel => t::setup_row_channel(),
            Field::Group => t::setup_row_group(),
            Field::Gamestream => t::setup_row_gamestream(),
            Field::Clipboard => t::setup_row_clipboard(),
            Field::Linger => t::setup_row_linger(),
            Field::Start => t::setup_row_start(),
            Field::OmarchyToasts => t::setup_row_omarchy_toasts(),
            Field::OmarchyIdle => t::setup_row_omarchy_idle(),
            Field::OmarchyTheme => t::setup_row_omarchy_theme(),
        }
    }

    /// The label column: the widest label in this language and two columns of air, the same
    /// width whichever rows the screen shows.
    pub fn label_width() -> usize {
        let widest = Field::ALL
            .iter()
            .map(|f| Screen::label(*f).chars().count())
            .max()
            .unwrap_or(0);
        widest + 2
    }

    /// A yes/no value with its reason, the word padded to the wider of the two so the
    /// reasons line up under each other.
    fn with_reason(on: bool, reason: &str) -> String {
        let (yes, no) = (t::setup_yes(), t::setup_no());
        let width = yes.chars().count().max(no.chars().count());
        let word = if on { yes } else { no };
        format!("{word:<width$} — {reason}")
    }

    pub fn value(&self, field: Field) -> String {
        let c = &self.choices;
        let yn = |on: bool| (if on { t::setup_yes() } else { t::setup_no() }).to_string();
        match field {
            // The download page's words, so the terminal reads like the page that sent you here.
            Field::Components => match (c.components.host, c.components.client) {
                (true, true) => t::setup_components_both().into(),
                (false, true) => t::setup_components_client().into(),
                _ => t::setup_components_host().into(),
            },
            Field::Channel => match c.switch_from {
                Some(from) => format!("{} → {}", from.as_str(), c.channel.as_str()),
                None => c.channel.as_str().to_string(),
            },
            Field::Group => match (c.group_why, c.punktfunk_group) {
                (Some(why), true) => Screen::with_reason(true, &why.text()),
                (_, true) => Screen::with_reason(true, t::setup_why_group_default()),
                _ => Screen::with_reason(false, t::setup_why_group_off()),
            },
            Field::Gamestream if c.gamestream => yn(true),
            Field::Gamestream => Screen::with_reason(false, t::setup_why_gamestream_off()),
            Field::Clipboard => yn(c.clipboard),
            Field::Linger => match (c.linger_why, c.linger) {
                (Some(why), true) => Screen::with_reason(true, &why.text()),
                (_, true) => yn(true),
                _ => Screen::with_reason(false, t::setup_why_linger_off()),
            },
            Field::Start => yn(c.start),
            Field::OmarchyToasts => yn(c.omarchy_toasts),
            Field::OmarchyIdle => yn(c.omarchy_idle),
            Field::OmarchyTheme => yn(c.omarchy_theme),
        }
    }

    /// Editor prompt: the sh installer's text, verbatim.
    pub fn why(field: Field) -> &'static str {
        match field {
            Field::Components => t::setup_row_components_hint(),
            Field::Channel => t::setup_row_channel_hint(),
            Field::Group => t::setup_row_group_hint(),
            Field::Gamestream => t::setup_gamestream_hint(),
            Field::Clipboard => t::setup_row_clipboard_hint(),
            Field::Linger => t::setup_row_linger_hint(),
            Field::Start => t::setup_row_start_hint(),
            Field::OmarchyToasts => t::setup_row_omarchy_toasts_hint(),
            Field::OmarchyIdle => t::setup_row_omarchy_idle_hint(),
            Field::OmarchyTheme => t::setup_row_omarchy_theme_hint(),
        }
    }

    /// Golden and plain-mode text of the screen.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![self.title()];
        let rule = "─".repeat(63);
        out.push(rule.clone());
        let width = Screen::label_width();
        for item in &self.items {
            if let Item::Row(field) = item {
                out.push(format!(
                    "  {:<width$}{}",
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
                Item::Uninstall => Some(t::setup_uninstall_action()),
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
