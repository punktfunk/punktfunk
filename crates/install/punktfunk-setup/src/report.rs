//! The text around the plan: banner, choices summary, verify, next steps.
//!
//! Two honesty rules, both tested. Never print the console URL as fact when
//! `punktfunk-web-server` is not on the box. NVIDIA warnings fire on a
//! successful install: the install succeeded, and streaming still will not
//! work until the driver does.
//!
//! Under `--yes` the choices summary is the only place the punktfunk-group
//! grant is stated, so the row names it (`design/installer-v2.md`).
//!
//! `banner`, `detected` and `choices_summary` print only on the plain transcript: the screens
//! replace them, so they stay English literals. The rest reaches the screens too and reads
//! `pf_l10n`.

use pf_l10n::tools as t;

use crate::choices::{Action, Choices, Why, LAN_BIND, LOOPBACK_BIND};
use crate::exec::{Opts, Outcome};
use crate::facts::{Facts, Family, Nvidia, DOCS};
use crate::seam::CommandRunner;
use crate::ui::Reporter;

/// Printing the password is one command, and it is the same one in the step that offers to
/// generate it, in the outro, and in the docs. It prints until the first sign-in only: the
/// console then stores a salted hash, and a forgotten password is reset, not read.
pub const PASSWORD_READ: &str =
    "sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password";

/// [`PASSWORD_READ`] for this box. SteamOS keeps the password in `web.env`, which its script
/// writes.
pub fn password_read(family: Family) -> &'static str {
    match family {
        Family::Steamos => "sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web.env",
        _ => PASSWORD_READ,
    }
}

pub fn banner(ui: &dyn Reporter) {
    ui.blank();
    ui.line("  punktfunk guided host installer — PREVIEW");
    ui.line(&format!(
        "  The per-distro pages stay the documented path; this automates them. Docs: {DOCS}/install"
    ));
    ui.line("  Re-running is safe. Ctrl-C stops between steps.");
    ui.blank();
}

pub fn detected(ui: &dyn Reporter, facts: &Facts) {
    ui.say(&format!(
        "Detected {} → {} (guide: {})",
        facts.os.pretty,
        facts.family.as_str(),
        facts.docs_page
    ));
}

/// Nothing here has run yet — this is the last screen before anything touches the box.
pub fn choices_summary(ui: &dyn Reporter, choices: &Choices) {
    if choices.action == Action::Uninstall {
        return;
    }
    ui.say("Choices (nothing below has run yet)");
    // A client listens on nothing fixed and joins no groups. Host rows would be
    // four questions with no consequence.
    if !choices.components.host {
        ui.line(&format!("  Channel: {}", choices.channel.as_str()));
        return;
    }
    let rows: [(&str, bool, &Option<Why>); 4] = [
        (
            "Full controller (joins the punktfunk group — grants usbip attach)",
            choices.punktfunk_group,
            &choices.group_why,
        ),
        (
            "Third-party clients (Moonlight, Artemis)",
            choices.gamestream,
            &None,
        ),
        ("Shared clipboard", choices.clipboard, &None),
        (
            "Start at boot with nobody logged in",
            choices.linger,
            &choices.linger_why,
        ),
    ];
    for (label, on, why) in rows {
        let value = if on { "yes" } else { "no" };
        match why {
            Some(why) if on => ui.line(&format!("  {label}: {value}  ({})", why.text())),
            _ => ui.line(&format!("  {label}: {value}")),
        }
    }
    // Not a yes/no, and it belongs here anyway: under --yes this summary is the only place who
    // can reach the console is stated, and it prints before the first privileged command.
    ui.line(&format!(
        "  Web console reachable from: {}",
        bind_label(choices)
    ));
}

/// The console's bind in the words the question asked it in.
fn bind_label(choices: &Choices) -> String {
    match choices.web_bind.as_str() {
        LOOPBACK_BIND => "this machine only".to_string(),
        LAN_BIND => "this local network".to_string(),
        addr => addr.to_string(),
    }
}

/// What `--uninstall` deliberately left behind, so a reinstall picks it up.
pub fn uninstall_outro(ui: &dyn Reporter) {
    ui.blank();
    for line in wrap(t::setup_uninstall_outro(), OUTRO_COLUMNS - INDENT.len()) {
        ui.line(&format!("{INDENT}{line}"));
    }
    ui.line(&format!(
        "{INDENT}{}",
        t::setup_uninstall_cleanups(format!("{DOCS}/uninstall#linux-hosts"))
    ));
}

/// Every outro line starts under the gutter's text.
const INDENT: &str = "  ";

/// Where a long outro sentence breaks, indent included.
const OUTRO_COLUMNS: usize = 100;

/// Greedy word wrap: a line holds as many words as fit in `width` columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

pub fn verify(
    ui: &dyn Reporter,
    run: &dyn CommandRunner,
    facts: &Facts,
    choices: &Choices,
    outcome: &Outcome,
    opts: Opts,
) {
    ui.say(t::setup_phase_checking());
    if outcome.started && !opts.dry {
        std::thread::sleep(std::time::Duration::from_secs(2));
        if run
            .probe(
                "systemctl",
                &["--user", "is-active", "--quiet", "punktfunk-host"],
            )
            .is_some_and(|o| o.ok())
        {
            ui.ok(t::setup_verify_running());
        } else {
            ui.warn(&t::setup_verify_not_active(format!(
                "{DOCS}/troubleshooting#the-linux-host-service-wont-start"
            )));
        }
        let listening = run
            .probe("ss", &["-lun"])
            .is_some_and(|o| o.ok() && o.stdout.contains(":9777 "));
        if listening {
            ui.ok(t::setup_verify_listening());
        } else {
            ui.warn(t::setup_verify_not_listening());
        }
    }
    nvidia_warnings(ui, facts);
    next_steps(ui, run, facts, choices, outcome, opts);
}

/// The install succeeded; these NVIDIA failures are silent and still block encode.
fn nvidia_warnings(ui: &dyn Reporter, facts: &Facts) {
    match facts.nvidia {
        Nvidia::Absent => {}
        Nvidia::NoDriver => ui.warn(&t::setup_nvidia_no_driver(&facts.docs_page)),
        Nvidia::ModuleNotLoaded => ui.warn(&t::setup_nvidia_no_module(format!(
            "{DOCS}/troubleshooting#nvidia-smi-says-it-cant-communicate-with-the-driver"
        ))),
        Nvidia::Ok => {}
    }
}

fn next_steps(
    ui: &dyn Reporter,
    run: &dyn CommandRunner,
    facts: &Facts,
    choices: &Choices,
    outcome: &Outcome,
    opts: Opts,
) {
    let ip = facts.ip.clone().unwrap_or_else(|| "<host-ip>".to_string());
    ui.blank();
    // The download page's third step, in its words, so the terminal ends where the page does.
    ui.line(&format!("{INDENT}{}", t::setup_outro_done()));
    // This box serves nothing. Console URL and pairing belong on the host, not localhost.
    if !choices.components.host {
        ui.line(&format!("{INDENT}{}", t::setup_outro_client()));
        ui.line(&format!("  {DOCS}/install-client · {DOCS}/pairing"));
        ui.blank();
        return;
    }
    // Probe again, not Facts: the install that just ran is usually what put the
    // console on the box. `--dry-run` shows the same text (it installs nothing).
    if run.which("punktfunk-web-server") || opts.dry {
        ui.line(&format!("{INDENT}{}", t::setup_outro_host()));
        // The URL has to be the one that answers. A loopback console is not at the box's LAN
        // address, and printing it there is how an operator concludes the install failed.
        let console = match choices.web_bind.as_str() {
            LOOPBACK_BIND => t::setup_outro_console_loopback("https://127.0.0.1:47992"),
            LAN_BIND => t::setup_outro_console(format!("https://{ip}:47992")),
            addr => t::setup_outro_console(format!("https://{addr}:47992")),
        };
        ui.line(&format!("{INDENT}{console}"));
        let password = match choices.web_password {
            Some(_) => t::setup_outro_password_typed().to_string(),
            None => t::setup_outro_password_read(password_read(facts.family)),
        };
        ui.line(&format!("{INDENT}{password}"));
    } else {
        ui.line(&format!("{INDENT}{}", t::setup_outro_no_console()));
        ui.line(&format!(
            "{INDENT}{}",
            t::setup_outro_install_line(&facts.docs_page)
        ));
    }
    if outcome.relogin {
        ui.line(&format!("{INDENT}{}", t::setup_outro_relogin()));
    }
    if choices.move_mgmt_port || facts.sunshine_active {
        ui.line(&format!(
            "{INDENT}{}",
            t::setup_outro_sunshine(format!("{DOCS}/switching-from-sunshine"))
        ));
    }
    ui.line(&format!("  {DOCS}/troubleshooting · {}", facts.docs_page));
    ui.blank();
}
