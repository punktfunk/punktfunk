//! Windows transcript bookends: detected, chosen, what to do next.
//!
//! Sibling of `report.rs`: same `Reporter` vocabulary, Windows words. The outro
//! is the two facts a user must leave knowing — the moved management port when a
//! competitor is live, and the unreachable-host warning when the network is Public.
//!
//! Only the silent `/LOG` transcript prints `detected`, `choices_summary` and `outro`.
//! `outro` and the bind label read `pf_l10n`; `detected` and the choices rows stay English.
//! `footnotes` also fills the wizard's Done page.
//!
//! Goldens pin the strings. Design: `design/installer-v2-windows.md`.

use pf_l10n::tools as t;

use super::choices::{NetworkAnswer, WinChoices};
use super::plan::Artifact;
use super::WinFacts;
use crate::choices::{DEFAULT_MGMT_PORT, LAN_BIND, LOOPBACK_BIND};
use crate::facts::DOCS;
use crate::ui::Reporter;

pub fn detected(ui: &dyn Reporter, facts: &WinFacts, artifact: Artifact) {
    let what = match artifact {
        Artifact::Host => "host",
        Artifact::Client => "client",
    };
    ui.say(&format!(
        "Detected Windows (build {}), {} — {what} installer",
        facts.os_build, facts.arch
    ));
    if let Some(installed) = facts.installed_for(artifact) {
        ui.ok(&format!(
            "existing install: {} at {}",
            installed.version.as_deref().unwrap_or("unknown version"),
            installed.location.as_deref().unwrap_or("the default dir"),
        ));
    }
}

pub fn choices_summary(ui: &dyn Reporter, choices: &WinChoices, artifact: Artifact) {
    ui.say("Choices (nothing below has run yet)");
    let yn = |v: bool| if v { "yes" } else { "no" };
    match artifact {
        Artifact::Host => {
            ui.line(&format!(
                "  Virtual display driver: {}",
                yn(choices.install_driver)
            ));
            ui.line(&format!(
                "  Gamepad drivers: {}",
                yn(choices.install_gamepad)
            ));
            ui.line(&format!(
                "  HDR Vulkan layer: {}",
                yn(choices.install_hdr_layer)
            ));
            let opt = |v: Option<bool>| match v {
                Some(true) => "yes",
                Some(false) => "no",
                None => "keep the box's setting",
            };
            ui.line(&format!("  Moonlight compat: {}", opt(choices.gamestream)));
            ui.line(&format!(
                "  Public-network firewall rules: {}",
                opt(choices.allow_public_fw)
            ));
            ui.line(&format!(
                "  Start the service: {}",
                yn(choices.start_service)
            ));
            ui.line(&format!("  Tray autostart: {}", yn(choices.tray_autostart)));
            // A silent install never sees the wizard, so this is the only place it is told who
            // can reach the console.
            ui.line(&format!(
                "  Web console reachable from: {}",
                bind_label(choices)
            ));
        }
        Artifact::Client => {
            ui.line(&format!("  Desktop shortcut: {}", yn(choices.desktop_icon)));
        }
    }
}

/// The bind in the words the wizard's row uses. `None` leaves host.env untouched.
fn bind_label(choices: &WinChoices) -> String {
    match choices.web_bind.as_deref() {
        None => t::setup_bind_value_keep().to_string(),
        Some(LOOPBACK_BIND) => t::setup_bind_value_loopback().to_string(),
        Some(LAN_BIND) => t::setup_bind_value_lan().to_string(),
        Some(addr) => addr.to_string(),
    }
}

pub fn outro(
    ui: &dyn Reporter,
    facts: &WinFacts,
    choices: &WinChoices,
    artifact: Artifact,
    host: &str,
) {
    ui.blank();
    ui.line(&format!("  {}", t::setup_win_outro_next()));
    match artifact {
        Artifact::Host => {
            // The URL has to be the one that answers: a loopback console is not at this host's
            // network address, and printing it there reads as a failed install.
            match choices.web_bind.as_deref() {
                Some(LOOPBACK_BIND) => {
                    ui.line(&format!("  1. {}", t::setup_win_outro_console_loopback()));
                    ui.line(&format!(
                        "     {}",
                        t::setup_win_outro_console_loopback_body()
                    ));
                }
                _ => ui.line(&format!("  1. {}", t::setup_win_outro_console(host))),
            }
            if !facts.web_password_present {
                ui.line(&format!("     {}", t::setup_win_outro_password()));
            }
            ui.line(&format!(
                "  2. {}",
                t::setup_win_outro_install_client(format!("{DOCS}/install-client"))
            ));
            for note in footnotes(facts, choices) {
                ui.line(&format!("  {note}"));
            }
        }
        Artifact::Client => {
            ui.line(&format!("  1. {}", t::setup_win_outro_open_client()));
            ui.line(&format!(
                "  2. {}",
                t::setup_win_outro_pairing_help(format!("{DOCS}/pairing"))
            ));
        }
    }
    ui.line(&format!(
        "  {}",
        t::setup_win_outro_stuck(format!("{DOCS}/troubleshooting"))
    ));
    ui.blank();
}

/// The D11/D12 footnotes: the moved console port and the unreachable-host warning — the
/// two facts a user must leave the installer knowing. The wizard's Done page renders these
/// as cards; the transcript's outro indents them.
pub fn footnotes(facts: &WinFacts, choices: &WinChoices) -> Vec<String> {
    let mut notes = Vec::new();
    if facts.needs_coexistence() {
        notes.push(t::setup_win_note_coexist(
            DEFAULT_MGMT_PORT,
            format!("{DOCS}/switching-from-sunshine"),
        ));
    }
    if matches!(choices.network, NetworkAnswer::Skip)
        && choices.allow_public_fw != Some(true)
        && !facts.public_networks().is_empty()
    {
        notes.push(format!(
            "⚠ {}",
            t::setup_win_note_public(format!("{DOCS}/troubleshooting#windows-firewall"))
        ));
    }
    notes
}
