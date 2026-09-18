//! The wizard's state, as pure data (WP2.1): which steps this run walks, which rows
//! Configure shows, and what each edit does to the plan.
//!
//! Sibling of `ui/summary.rs`'s `Screen`, same bargain, Windows types: the reactor wizard is
//! a renderer over this struct, so every step/row rule is testable on the Linux and macOS
//! lanes without a WinUI in sight. No cursor or key model here — a mouse-driven wizard edits
//! rows directly, so the state is rows + setters, not a selection machine.
//!
//! Re-resolution is free by construction, the summary-screen rule: the screen owns
//! `WinChoices` and the plan rebuilds from `(WinFacts, WinChoices)` whenever asked, so
//! toggling Moonlight compat re-derives the service argv with no cache to invalidate. The
//! step list re-derives the same way — the stepper always shows *this run's* real path
//! (design D9), so the Network step materializes and dematerializes as the choices change.

use super::choices::{NetworkAnswer, WinChoices};
use super::plan::{self, Artifact, WinPlan};
use super::WinFacts;

/// The D9 step frame. `Network` is the one conditional step (D12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizStep {
    Welcome,
    Configure,
    Network,
    Install,
    Done,
}

impl WizStep {
    pub fn title(self) -> &'static str {
        match self {
            WizStep::Welcome => "Welcome",
            WizStep::Configure => "Configure",
            WizStep::Network => "Network",
            WizStep::Install => "Install",
            WizStep::Done => "Done",
        }
    }
}

/// One editable Configure row. The task set is today's `[Tasks]` table (D5's names are the
/// silent contract; these are their wizard faces) plus the fresh-only password row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Driver,
    Gamepad,
    HdrLayer,
    Gamestream,
    PublicFw,
    StartService,
    Tray,
    WebBind,
    Password,
    DesktopIcon,
}

/// How the renderer edits a row. `TriState` is the upgrade face of the two settings the
/// service persists itself: `None` passes nothing and the box keeps its state
/// (`choices.rs`'s rule), which a two-state toggle cannot say honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Editor {
    Toggle(bool),
    TriState(Option<bool>),
    Password(Option<String>),
    /// The console's listen address. `None` is the upgrade face: pass nothing, keep host.env's.
    Bind(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct WinScreen {
    pub facts: WinFacts,
    pub choices: WinChoices,
    pub artifact: Artifact,
}

impl WinScreen {
    pub fn new(facts: WinFacts, choices: WinChoices, artifact: Artifact) -> WinScreen {
        WinScreen {
            facts,
            choices,
            artifact,
        }
    }

    pub fn fresh(&self) -> bool {
        self.facts.installed_for(self.artifact).is_none()
    }

    /// This run's real path (D9): the Network step exists only while its trigger holds.
    ///
    /// Trigger: host leg, a Public network, and the public rules not already opted through
    /// the Configure row — but once the step was *answered* it stays, because the user is
    /// standing on the answer (`OpenPublicRules` sets the row and must not eat its own step).
    pub fn steps(&self) -> Vec<WizStep> {
        let mut steps = vec![WizStep::Welcome, WizStep::Configure];
        let triggered = self.artifact == Artifact::Host
            && !self.facts.public_networks().is_empty()
            && !(self.choices.network == NetworkAnswer::Skip
                && self.choices.allow_public_fw == Some(true));
        if triggered {
            steps.push(WizStep::Network);
        }
        steps.extend([WizStep::Install, WizStep::Done]);
        steps
    }

    /// The Configure rows, in the `[Tasks]` order. The password row is fresh-only and only
    /// when no web install already left one — an upgrade keeps the password the box has.
    pub fn rows(&self) -> Vec<Field> {
        match self.artifact {
            Artifact::Client => vec![Field::DesktopIcon],
            Artifact::Host => {
                let mut rows = vec![
                    Field::Driver,
                    Field::Gamepad,
                    Field::HdrLayer,
                    Field::Gamestream,
                    Field::PublicFw,
                    Field::StartService,
                    Field::Tray,
                    Field::WebBind,
                ];
                if self.fresh() && !self.facts.web_password_present {
                    rows.push(Field::Password);
                }
                rows
            }
        }
    }

    /// The rows grouped for the wizard's Configure page; the transcript keeps the flat
    /// `[Tasks]` order. Every row lands in exactly one section (tested).
    pub fn sections(&self) -> Vec<(&'static str, Vec<Field>)> {
        let rows = self.rows();
        let group = |title: &'static str, fields: &[Field]| {
            let fields: Vec<Field> = fields
                .iter()
                .copied()
                .filter(|f| rows.contains(f))
                .collect();
            (title, fields)
        };
        [
            group("Drivers", &[Field::Driver, Field::Gamepad, Field::HdrLayer]),
            group("Network", &[Field::Gamestream, Field::PublicFw]),
            group("After install", &[Field::StartService, Field::Tray]),
            group("Web console", &[Field::WebBind, Field::Password]),
            group("Shortcuts", &[Field::DesktopIcon]),
        ]
        .into_iter()
        .filter(|(_, fields)| !fields.is_empty())
        .collect()
    }

    pub fn editor(&self, field: Field) -> Editor {
        let c = &self.choices;
        match field {
            Field::Driver => Editor::Toggle(c.install_driver),
            Field::Gamepad => Editor::Toggle(c.install_gamepad),
            Field::HdrLayer => Editor::Toggle(c.install_hdr_layer),
            // Fresh installs always carry `Some` for these two (`derive`), so the tri-state
            // face only ever appears where "keep the box's setting" is a real option.
            Field::Gamestream if self.fresh() => Editor::Toggle(c.gamestream.unwrap_or(false)),
            Field::Gamestream => Editor::TriState(c.gamestream),
            Field::PublicFw if self.fresh() => Editor::Toggle(c.allow_public_fw.unwrap_or(false)),
            Field::PublicFw => Editor::TriState(c.allow_public_fw),
            Field::StartService => Editor::Toggle(c.start_service),
            Field::Tray => Editor::Toggle(c.tray_autostart),
            Field::WebBind => Editor::Bind(c.web_bind.clone()),
            Field::Password => Editor::Password(c.web_password.clone()),
            Field::DesktopIcon => Editor::Toggle(c.desktop_icon),
        }
    }

    pub fn label(field: Field) -> &'static str {
        match field {
            Field::Driver => "Virtual display driver",
            Field::Gamepad => "Gamepad drivers",
            Field::HdrLayer => "HDR Vulkan layer",
            Field::Gamestream => "Moonlight compat",
            Field::PublicFw => "Public-network firewall rules",
            Field::StartService => "Start the service",
            Field::Tray => "Tray autostart",
            Field::WebBind => "Web console reachable from",
            Field::Password => "Web console password",
            Field::DesktopIcon => "Desktop shortcut",
        }
    }

    /// The row's explanation. Owned here so the D11 rule ("why-text names Sunshine when
    /// live") is state, not renderer copy.
    pub fn why(&self, field: Field) -> String {
        match field {
            Field::Driver => "The virtual display the host streams from.".into(),
            Field::Gamepad => "Controllers arrive on the host as real gamepads.".into(),
            Field::HdrLayer => "HDR for Vulkan and DXVK titles.".into(),
            Field::Gamestream => match self.competitor() {
                Some(who) => format!(
                    "Serve Moonlight, Artemis, or another third-party client. {who} owns the Moonlight ports while it runs — this plane can't bind until it stops."
                ),
                None => {
                    "Serve Moonlight, Artemis, or another third-party client. Punktfunk's own apps don't need this.".into()
                }
            },
            Field::PublicFw => {
                "Open the firewall on networks marked Public. Off, the host is reachable on private networks only.".into()
            }
            Field::StartService => "Start streaming as soon as the install finishes.".into(),
            Field::Tray => "The status icon next to the clock, for every user.".into(),
            Field::WebBind => {
                "Where the console answers: your local network (never the internet), this PC only, or one address such as a VPN interface. It is how you pair devices and change every setting.".into()
            }
            Field::Password => {
                "Generated for you — keep it or type your own. It signs you into the web console and is shown again on the finish page.".into()
            }
            Field::DesktopIcon => "A Punktfunk shortcut on the desktop.".into(),
        }
    }

    /// The D11 coexistence row (informational, visible only when a competitor is live).
    pub fn coexistence_note(&self) -> Option<String> {
        if !self.facts.needs_coexistence() {
            return None;
        }
        let who = self.competitor().unwrap_or("another streaming host");
        Some(format!(
            "{who} detected — punktfunk's management API moves to :{}. {who} keeps :{}.",
            super::MGMT_PORT_MOVED,
            super::MGMT_PORT,
        ))
    }

    /// The competitor's human name — the service name minus the `Service` suffix.
    fn competitor(&self) -> Option<&str> {
        if !self.facts.needs_coexistence() {
            return None;
        }
        Some(
            self.facts
                .competing_hosts
                .first()
                .map(|s| s.strip_suffix("Service").unwrap_or(s))
                .unwrap_or("another streaming host"),
        )
    }

    /// The Public networks the D12 step names, joined for copy.
    pub fn public_network_names(&self) -> String {
        self.facts
            .public_networks()
            .iter()
            .map(|n| n.name.clone())
            .collect::<Vec<_>>()
            .join("', '")
    }

    pub fn set_bool(&mut self, field: Field, on: bool) {
        let c = &mut self.choices;
        match field {
            Field::Driver => c.install_driver = on,
            Field::Gamepad => c.install_gamepad = on,
            Field::HdrLayer => c.install_hdr_layer = on,
            Field::Gamestream => c.gamestream = Some(on),
            Field::PublicFw => c.allow_public_fw = Some(on),
            Field::StartService => c.start_service = on,
            Field::Tray => c.tray_autostart = on,
            Field::DesktopIcon => c.desktop_icon = on,
            Field::WebBind | Field::Password => {}
        }
    }

    /// The tri-state "keep the box's setting" arm, upgrades only.
    pub fn keep_box_setting(&mut self, field: Field) {
        match field {
            Field::Gamestream => self.choices.gamestream = None,
            Field::PublicFw => self.choices.allow_public_fw = None,
            _ => {}
        }
    }

    pub fn set_password(&mut self, password: String) {
        self.choices.web_password = Some(password);
    }

    /// The bind row. `None` is the upgrade's "keep what host.env says"; anything else is written
    /// through `--web-bind`, and the plan validates nothing, so a free-typed address lands as-is.
    pub fn set_bind(&mut self, bind: Option<String>) {
        self.choices.web_bind = bind;
    }

    pub fn set_network(&mut self, answer: NetworkAnswer) {
        self.choices.network = answer;
    }

    /// The choices the plan actually runs on. Answer (b) of the D12 step *is* today's
    /// `allowpublicfw` task — derived at plan time rather than stored, so switching answers
    /// can never leave a stale firewall opt-in behind.
    pub fn effective_choices(&self) -> WinChoices {
        let mut c = self.choices.clone();
        if c.network == NetworkAnswer::OpenPublicRules {
            c.allow_public_fw = Some(true);
        }
        c
    }

    pub fn plan(&self) -> WinPlan {
        plan::build(&self.facts, &self.effective_choices(), self.artifact, false)
    }
}

#[cfg(test)]
mod tests {
    use super::super::args::InnoArgs;
    use super::super::{NetCategory, NetProfile, TaskState, WinInstall};
    use super::*;
    use crate::seam::Env;

    fn fresh_facts() -> WinFacts {
        WinFacts {
            os_build: 26200,
            arch: "x64".into(),
            installed: None,
            host_env_present: false,
            web_password_present: false,
            mgmt_bind_set: false,
            competing_hosts: vec![],
            mgmt_port_in_use: false,
            networks: vec![],
            steam_audio_drivers: true,
            tray_autostart: false,
            vulkan_layer_registered: false,
            web_task: TaskState::Absent,
            scripting_task: TaskState::Absent,
            inno_uninstaller: false,
            client_installed: None,
        }
    }

    fn upgrade_facts() -> WinFacts {
        WinFacts {
            installed: Some(WinInstall {
                version: Some("0.34.0".into()),
                location: Some(r"C:\Program Files\punktfunk\".into()),
            }),
            host_env_present: true,
            web_password_present: true,
            ..fresh_facts()
        }
    }

    fn public_facts() -> WinFacts {
        WinFacts {
            networks: vec![NetProfile {
                name: "Cafe".into(),
                category: NetCategory::Public,
            }],
            ..fresh_facts()
        }
    }

    fn screen_of(facts: WinFacts, artifact: Artifact) -> WinScreen {
        let choices = WinChoices::derive(&facts, Artifact::Host);
        WinScreen::new(facts, choices, artifact)
    }

    #[test]
    fn a_fresh_host_shows_the_task_rows_plus_the_password_row() {
        let s = screen_of(fresh_facts(), Artifact::Host);
        let rows = s.rows();
        assert_eq!(rows.last(), Some(&Field::Password));
        assert_eq!(rows.len(), 9);
        assert!(matches!(s.editor(Field::Gamestream), Editor::Toggle(false)));
    }

    #[test]
    fn an_upgrade_drops_the_password_row_and_goes_tri_state() {
        let s = screen_of(upgrade_facts(), Artifact::Host);
        assert!(!s.rows().contains(&Field::Password));
        assert_eq!(s.editor(Field::Gamestream), Editor::TriState(None));
        assert_eq!(s.editor(Field::PublicFw), Editor::TriState(None));
    }

    // The bind row survives an upgrade, unlike the password: it is how an operator opens or
    // closes the console later, and `None` is the "keep what host.env says" face it needs.
    #[test]
    fn the_bind_row_is_on_every_host_run_and_defaults_to_the_lan_when_fresh() {
        let fresh = screen_of(fresh_facts(), Artifact::Host);
        assert!(fresh.rows().contains(&Field::WebBind));
        assert_eq!(
            fresh.editor(Field::WebBind),
            Editor::Bind(Some("0.0.0.0".into()))
        );
        let upgrade = screen_of(upgrade_facts(), Artifact::Host);
        assert!(upgrade.rows().contains(&Field::WebBind));
        assert_eq!(upgrade.editor(Field::WebBind), Editor::Bind(None));
    }

    // Every row belongs to exactly one section, in row order — the wizard shows nothing
    // twice and forgets nothing.
    #[test]
    fn the_sections_partition_the_rows() {
        for (facts, artifact) in [
            (fresh_facts(), Artifact::Host),
            (upgrade_facts(), Artifact::Host),
            (fresh_facts(), Artifact::Client),
        ] {
            let s = screen_of(facts, artifact);
            let flat: Vec<Field> = s.sections().into_iter().flat_map(|(_, f)| f).collect();
            assert_eq!(flat, s.rows());
        }
        let titles: Vec<&str> = screen_of(fresh_facts(), Artifact::Host)
            .sections()
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert_eq!(
            titles,
            ["Drivers", "Network", "After install", "Web console"]
        );
        assert_eq!(
            screen_of(fresh_facts(), Artifact::Client).sections()[0].0,
            "Shortcuts"
        );
    }

    #[test]
    fn a_client_screen_is_one_row() {
        let s = screen_of(fresh_facts(), Artifact::Client);
        assert_eq!(s.rows(), [Field::DesktopIcon]);
        assert_eq!(
            s.steps(),
            [
                WizStep::Welcome,
                WizStep::Configure,
                WizStep::Install,
                WizStep::Done
            ]
        );
    }

    /// The summary screen's re-resolution rule, on the Windows sibling.
    #[test]
    fn toggling_moonlight_compat_re_derives_the_service_argv() {
        let mut s = screen_of(fresh_facts(), Artifact::Host);
        assert!(s
            .plan()
            .commands()
            .iter()
            .any(|c| c.contains("--gamestream=off")));
        s.set_bool(Field::Gamestream, true);
        assert!(s
            .plan()
            .commands()
            .iter()
            .any(|c| c.contains("--gamestream=on")));
    }

    #[test]
    fn keep_box_setting_passes_nothing_on_an_upgrade() {
        let mut s = screen_of(upgrade_facts(), Artifact::Host);
        s.set_bool(Field::Gamestream, true);
        assert!(s
            .plan()
            .commands()
            .iter()
            .any(|c| c.contains("--gamestream=on")));
        s.keep_box_setting(Field::Gamestream);
        assert!(!s
            .plan()
            .commands()
            .iter()
            .any(|c| c.contains("--gamestream")));
    }

    // The stepper never shows a ghost step (D9): no Public network, no Network dot.
    #[test]
    fn the_network_step_materializes_only_on_a_public_network_host_run() {
        let s = screen_of(fresh_facts(), Artifact::Host);
        assert!(!s.steps().contains(&WizStep::Network));
        let s = screen_of(public_facts(), Artifact::Host);
        assert!(s.steps().contains(&WizStep::Network));
        let s = screen_of(public_facts(), Artifact::Client);
        assert!(!s.steps().contains(&WizStep::Network));
    }

    // Opting into the public rules on the Configure row serves the step's purpose, so the
    // step dematerializes; answering the step itself must not eat the step.
    #[test]
    fn the_network_step_follows_the_public_fw_row_but_survives_its_own_answer() {
        let mut s = screen_of(public_facts(), Artifact::Host);
        s.set_bool(Field::PublicFw, true);
        assert!(!s.steps().contains(&WizStep::Network));
        s.set_bool(Field::PublicFw, false);
        s.set_network(NetworkAnswer::OpenPublicRules);
        assert!(s.steps().contains(&WizStep::Network));
    }

    /// WP2.1c's acceptance, stated early: answer (b) is provably the same plan step as
    /// `/MERGETASKS=allowpublicfw`.
    #[test]
    fn open_public_rules_flips_the_same_plan_step_as_the_task_flag() {
        let mut s = screen_of(public_facts(), Artifact::Host);
        s.set_network(NetworkAnswer::OpenPublicRules);
        let via_step = s.plan();

        let facts = public_facts();
        let mut choices = WinChoices::derive(&facts, Artifact::Host);
        let args = InnoArgs::parse(&[r#"/MERGETASKS="allowpublicfw""#.to_string()]);
        choices.apply(&args, &Env::default());
        let via_flag = plan::build(&facts, &choices, Artifact::Host, false);

        let service = |p: &WinPlan| {
            p.commands()
                .iter()
                .find(|c| c.contains("service install"))
                .cloned()
        };
        assert_eq!(service(&via_step), service(&via_flag));
        assert!(service(&via_step)
            .unwrap()
            .contains("--allow-public-network=on"));
    }

    /// WP2.2's D12 leftover: the step is reachable from Reconfigure. An upgrade derives the
    /// public-rules row to "keep the box's setting", which leaves the trigger armed, and the
    /// answer reaches the plan even though nothing else about the box is rewritten.
    #[test]
    fn reconfigure_on_a_public_network_reaches_the_network_step() {
        let facts = WinFacts {
            networks: public_facts().networks,
            ..upgrade_facts()
        };
        let mut s = screen_of(facts, Artifact::Host);
        assert!(!s.fresh());
        assert!(s.steps().contains(&WizStep::Network));
        s.set_network(NetworkAnswer::OpenPublicRules);
        let commands = s.plan().commands();
        assert!(commands
            .iter()
            .any(|c| c.contains("--allow-public-network=on")));
        assert!(!commands.iter().any(|c| c.contains("--gamestream")));
    }

    #[test]
    fn make_private_reaches_the_plan_and_switching_answers_leaves_no_stale_opt_in() {
        let mut s = screen_of(public_facts(), Artifact::Host);
        s.set_network(NetworkAnswer::OpenPublicRules);
        s.set_network(NetworkAnswer::MakePrivate("Cafe".into()));
        let plan = s.plan();
        assert!(plan
            .steps()
            .any(|a| matches!(a, super::super::plan::WinAction::MakeNetworkPrivate { network } if network == "Cafe")));
        assert!(!plan
            .commands()
            .iter()
            .any(|c| c.contains("--allow-public-network=on")));
    }

    #[test]
    fn the_coexistence_row_appears_and_names_the_competitor() {
        let facts = WinFacts {
            competing_hosts: vec!["SunshineService".into()],
            ..fresh_facts()
        };
        let s = screen_of(facts, Artifact::Host);
        let note = s.coexistence_note().unwrap();
        assert!(note.contains("Sunshine detected"));
        assert!(note.contains(":47991"));
        assert!(s.why(Field::Gamestream).contains("Sunshine owns"));
        assert!(screen_of(fresh_facts(), Artifact::Host)
            .coexistence_note()
            .is_none());
    }
}
