//! The CLI: flags, env twins, the TTY probe, and the mode dispatch.
//!
//! Same flags, same env twins, same exit codes as `scripts/install.sh` (0 done, 1
//! unsupported system or a step failed, 2 bad usage), plus `--host`/`--client` and
//! the demo flags that are outside it.
//!
//! No TTY behaves exactly like `--yes`. Probe by opening `/dev/tty`: in a container
//! or under a service the node exists but open returns ENXIO, so `-r`/`-w` still
//! succeed.

use std::path::PathBuf;
use std::process::ExitCode;

use punktfunk_setup::choices::{Action, Choices, Pins, LAN_BIND, LOOPBACK_BIND};
use punktfunk_setup::exec::{Executor, Opts};
use punktfunk_setup::facts::{Facts, Family, Floor, DOCS};
use punktfunk_setup::plan;
use punktfunk_setup::report;
use punktfunk_setup::seam::{BasePaths, CommandRunner, Env, SystemRunner};
use punktfunk_setup::ui::logo;
use punktfunk_setup::ui::summary::{Screen, Step};
use punktfunk_setup::ui::term::{ConsoleTerm, Terminal};
use punktfunk_setup::ui::theme::Caps;
use punktfunk_setup::ui::tui::Tui;
use punktfunk_setup::ui::{Plain, Reporter};

/// 140 ms: long enough that a demo step reads as work, short enough to sit through.
const DEMO_LATENCY_MS: u64 = 140;

/// The mark needs a width to decide whether it fits. 80 is the safe assumption when the
/// terminal will not say.
fn terminal_width() -> u16 {
    ConsoleTerm::open().map_or(80, |t| t.width())
}

const USAGE: &str = r#"punktfunk guided installer (preview)

usage: punktfunk-setup [options]
  -y, --yes             no prompts: take every default (also the behaviour without a terminal)
  --host | --client     what to install (default: host; combinable)
  --channel stable|canary   package channel (default stable; canary = latest main build). On a box
                        that already has it this SWITCHES channel, either direction.
  --gamestream | --no-gamestream   Moonlight/Artemis/third-party clients (default depends on the box)
  --clipboard | --no-clipboard     shared clipboard (default yes)
  --punktfunk-group | --no-punktfunk-group   full controller / virtual Steam Deck pad (default yes;
                        it joins the punktfunk group, which grants usbip attach)
  --linger | --no-linger           start at boot with nobody logged in (default depends on the box)
  --console-cert | --no-console-cert     trust the console's certificate in Chromium (default yes)
  --omarchy-setup | --no-omarchy-setup   run `punktfunk-omarchy setup` after an Omarchy install
                        (the umbrella: --no-omarchy-setup clears the three rows below too)
  --omarchy-toasts | --no-omarchy-toasts pairing and stream toasts
  --omarchy-idle | --no-omarchy-idle     keep the screen awake while a stream runs
  --omarchy-theme | --no-omarchy-theme   follow the Omarchy theme in the console
  --mgmt-port N         port to move the management API to if Sunshine/Apollo holds 47990 (default 47991)
  --web-bind ADDR       where the web console listens: lan (default), localhost, or one address
  --no-start            install and configure, but don't enable the services
  -v, --verbose         echo every command instead of collapsing to a progress line
  --uninstall           stop the services and remove the packages + repo (config stays)
  --dry-run             print every command it would run, change nothing
  --facts FILE          load a box description instead of probing this one
  --demo PRESET         walk the whole flow against a canned box, changing nothing
  --fail PHASE          with --demo: show how a failure in that phase renders
  -h, --help            this text

Every option has an environment twin for scripted installs: PUNKTFUNK_INSTALL_YES=1,
PUNKTFUNK_INSTALL_CHANNEL, PUNKTFUNK_INSTALL_GAMESTREAM, PUNKTFUNK_INSTALL_CLIPBOARD,
PUNKTFUNK_INSTALL_PUNKTFUNK_GROUP, PUNKTFUNK_INSTALL_LINGER, PUNKTFUNK_INSTALL_CONSOLE_CERT,
PUNKTFUNK_INSTALL_OMARCHY_SETUP, PUNKTFUNK_INSTALL_MGMT_PORT, PUNKTFUNK_INSTALL_WEB_BIND
(1/0 for the flags)."#;

/// 2 is bad usage, matching the sh installer's contract.
const BAD_USAGE: u8 = 2;

/// `--web-bind` / its env twin. `lan` is the friendly spelling of 0.0.0.0; anything else has to
/// parse as an address, so a typo cannot quietly leave the console somewhere nobody is listening.
fn web_bind(raw: &str) -> Result<String, (u8, String)> {
    match raw.trim() {
        "localhost" | "loopback" => Ok(LOOPBACK_BIND.to_string()),
        "lan" | "any" => Ok(LAN_BIND.to_string()),
        v if v.parse::<std::net::IpAddr>().is_ok() => Ok(v.to_string()),
        _ => Err((
            BAD_USAGE,
            "--web-bind must be an address, or 'localhost' or 'lan'".to_string(),
        )),
    }
}

struct Cli {
    pins: Pins,
    yes: bool,
    dry: bool,
    facts_file: Option<PathBuf>,
    demo: Option<String>,
    fail: Option<String>,
    verbose: bool,
}

fn env_flag(env: &Env, key: &str) -> Option<bool> {
    env.get(key).map(|v| v == "1")
}

fn parse(args: Vec<String>, env: &Env) -> Result<Cli, (u8, String)> {
    // Env first, then flags overwrite — the same order as the sh installer.
    let mut cli = Cli {
        pins: Pins {
            channel: env
                .get("PUNKTFUNK_INSTALL_CHANNEL")
                .and_then(|v| v.parse().ok()),
            gamestream: env_flag(env, "PUNKTFUNK_INSTALL_GAMESTREAM"),
            clipboard: env_flag(env, "PUNKTFUNK_INSTALL_CLIPBOARD"),
            punktfunk_group: env_flag(env, "PUNKTFUNK_INSTALL_PUNKTFUNK_GROUP"),
            linger: env_flag(env, "PUNKTFUNK_INSTALL_LINGER"),
            omarchy_setup: env_flag(env, "PUNKTFUNK_INSTALL_OMARCHY_SETUP"),
            omarchy_toasts: env_flag(env, "PUNKTFUNK_INSTALL_OMARCHY_TOASTS"),
            omarchy_idle: env_flag(env, "PUNKTFUNK_INSTALL_OMARCHY_IDLE"),
            omarchy_theme: env_flag(env, "PUNKTFUNK_INSTALL_OMARCHY_THEME"),
            console_cert: env_flag(env, "PUNKTFUNK_INSTALL_CONSOLE_CERT"),
            mgmt_port: env
                .get("PUNKTFUNK_INSTALL_MGMT_PORT")
                .map(|v| v.parse().unwrap_or(0)),
            web_bind: match env.get("PUNKTFUNK_INSTALL_WEB_BIND") {
                Some(v) => Some(web_bind(v)?),
                None => None,
            },
            ..Pins::default()
        },
        yes: env.get("PUNKTFUNK_INSTALL_YES") == Some("1"),
        dry: env.get("PUNKTFUNK_INSTALL_DRY_RUN") == Some("1"),
        facts_file: None,
        demo: None,
        fail: None,
        verbose: false,
    };
    if env.get("PUNKTFUNK_INSTALL_CHANNEL").is_some() && cli.pins.channel.is_none() {
        return Err((BAD_USAGE, "--channel must be stable or canary".into()));
    }

    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        let mut value = || inline.clone().or_else(|| it.next());
        match flag.as_str() {
            "-y" | "--yes" => cli.yes = true,
            "--host" => cli.pins.host = true,
            "--client" => cli.pins.client = true,
            "--channel" => {
                let raw = value().unwrap_or_default();
                cli.pins.channel =
                    Some(raw.parse().map_err(|()| {
                        (BAD_USAGE, "--channel must be stable or canary".to_string())
                    })?);
            }
            "--gamestream" => cli.pins.gamestream = Some(true),
            "--no-gamestream" => cli.pins.gamestream = Some(false),
            "--clipboard" => cli.pins.clipboard = Some(true),
            "--no-clipboard" => cli.pins.clipboard = Some(false),
            "--punktfunk-group" => cli.pins.punktfunk_group = Some(true),
            "--no-punktfunk-group" => cli.pins.punktfunk_group = Some(false),
            "--linger" => cli.pins.linger = Some(true),
            "--no-linger" => cli.pins.linger = Some(false),
            "--omarchy-setup" => cli.pins.omarchy_setup = Some(true),
            "--no-omarchy-setup" => cli.pins.omarchy_setup = Some(false),
            "--omarchy-toasts" => cli.pins.omarchy_toasts = Some(true),
            "--no-omarchy-toasts" => cli.pins.omarchy_toasts = Some(false),
            "--omarchy-idle" => cli.pins.omarchy_idle = Some(true),
            "--no-omarchy-idle" => cli.pins.omarchy_idle = Some(false),
            "--omarchy-theme" => cli.pins.omarchy_theme = Some(true),
            "--no-omarchy-theme" => cli.pins.omarchy_theme = Some(false),
            "--console-cert" => cli.pins.console_cert = Some(true),
            "--no-console-cert" => cli.pins.console_cert = Some(false),
            "--mgmt-port" => {
                let raw = value().unwrap_or_default();
                cli.pins.mgmt_port = Some(
                    raw.parse()
                        .map_err(|_| (BAD_USAGE, "--mgmt-port must be a number".to_string()))?,
                );
            }
            "--web-bind" => {
                let raw = value().unwrap_or_default();
                cli.pins.web_bind = Some(web_bind(&raw)?);
            }
            "--no-start" => cli.pins.no_start = true,
            "--uninstall" => cli.pins.action = Action::Uninstall,
            "--dry-run" => cli.dry = true,
            "-v" | "--verbose" => cli.verbose = true,
            "--facts" => cli.facts_file = value().map(PathBuf::from),
            "--demo" => cli.demo = value(),
            "--fail" => cli.fail = value(),
            "-h" | "--help" => return Err((0, USAGE.to_string())),
            other => {
                return Err((BAD_USAGE, format!("unknown option: {other}\n\n{USAGE}")));
            }
        }
    }
    if cli.pins.mgmt_port == Some(0) {
        return Err((BAD_USAGE, "--mgmt-port must be a number".into()));
    }
    Ok(cli)
}

fn main() -> ExitCode {
    let env = Env::from_env();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse(args, &env) {
        Ok(cli) => cli,
        Err((0, text)) => {
            println!("{text}");
            return ExitCode::SUCCESS;
        }
        Err((code, text)) => {
            eprintln!("{text}");
            return ExitCode::from(code);
        }
    };

    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_ok();
    let demo = cli.demo.clone();
    let yes = cli.yes || !tty;
    let plain = Plain::stdio(env.get("NO_COLOR").is_none());
    let paths = match &demo {
        Some(_) => punktfunk_setup::demo::sandbox_paths(),
        None => BasePaths::from_env(),
    };
    let caps = Caps::detect(&env, tty, terminal_width());

    let mut runner = SystemRunner::new();
    let user = env
        .get("USER")
        .map(str::to_string)
        .or_else(|| runner.first_line("id", &["-un"]))
        .unwrap_or_default();
    runner.exports.push(("USER".to_string(), user));

    // A demo box is canned: skip preflight, and nothing may be probed.
    let facts = if let Some(name) = &demo {
        match punktfunk_setup::demo::preset(name) {
            Some(facts) => facts,
            None => {
                plain.die(&format!(
                    "unknown --demo preset '{name}'. Try: {}",
                    punktfunk_setup::demo::PRESETS.join(", ")
                ));
                return ExitCode::FAILURE;
            }
        }
    } else {
        if let Err(msg) = preflight(&env, &paths, &mut runner) {
            report::banner(&plain);
            plain.die(&msg);
            return ExitCode::FAILURE;
        }
        match load_facts(&cli, &paths, &runner, &env) {
            Ok(facts) => facts,
            Err(msg) => {
                report::banner(&plain);
                plain.die(&msg);
                return ExitCode::FAILURE;
            }
        }
    };

    // The TUI is the interactive surface only. No terminal or `--yes` stays on the plain
    // output, byte for byte — CI containers and scripts depend on it.
    let interactive = tty && !yes;
    let mut console = if interactive {
        ConsoleTerm::open()
    } else {
        None
    };
    let tui = console
        .as_mut()
        .map(|term| Tui::new(term as &mut dyn Terminal, caps, logo::FRAME_MS));
    let ui: &dyn Reporter = match &tui {
        Some(tui) => tui,
        None => &plain,
    };

    let mut choices = Choices::derive(&facts, &cli.pins);
    let opts = Opts {
        dry: cli.dry,
        // Quiet only where the progress line is drawn: plain mode stays the full transcript
        // CI reads, and -v asks for it on purpose.
        quiet: interactive && !cli.verbose && !cli.dry,
        tty,
    };

    if tui.is_none() {
        report::banner(ui);
        report::detected(ui, &facts);
    }

    // Floors sit after the uninstall dispatch: a box below them must still be able to clean up.
    if choices.action != Action::Uninstall
        && let Some(floor) = &facts.floor
    {
        match floor {
            Floor::Die(msg) => {
                ui.die(msg);
                return ExitCode::FAILURE;
            }
            Floor::Confirm(msg) => {
                ui.warn(msg);
                if !ask(interactive, "Continue anyway?") {
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    if let Some(tui) = &tui {
        let parts = logo::Parts {
            host: choices.components.host,
            client: choices.components.client,
        };
        let drawn = tui.intro(logo::intro_level(&caps, yes), parts);
        let mut screen = Screen::new(facts.clone(), choices.clone());
        match tui.settings(&mut screen, drawn) {
            Step::Cancel => {
                tui.outro(&["Nothing was changed.".to_string()]);
                return ExitCode::SUCCESS;
            }
            Step::Run(action) => choices.action = action,
            Step::Idle | Step::Edit(_) => unreachable!("the settings loop only ends on a choice"),
        }
        // Edits live on the screen, not in the pins.
        let action = choices.action;
        choices = screen.choices;
        choices.action = action;
        // After the settings screen, not a row on it: the console password is the one thing
        // a fresh host install leaves the user needing, and a row is too easy to walk past.
        // A box that already has one keeps it — this never overwrites a password in use.
        if action == Action::Install
            && choices.components.host
            && !facts.web_password_present
            && facts.family != Family::Steamos
        {
            let ip = facts.ip.clone().unwrap_or_else(|| "this box".to_string());
            choices.web_password =
                tui.web_password(&format!("https://{ip}:47992"), report::PASSWORD_READ);
        }
        // Right after it, for the same reason: the console is the whole product surface, and who
        // can reach it is the one thing a host install must not decide behind the user's back.
        // `--web-bind` pins it, and then there is nothing to ask.
        if action == Action::Install && choices.components.host && cli.pins.web_bind.is_none() {
            let ip = facts.ip.clone().unwrap_or_else(|| "this box".to_string());
            if let Some(bind) = tui.web_bind(&ip, &choices.web_bind) {
                choices.web_bind = bind;
            }
        }
    } else {
        report::choices_summary(ui, &choices);
    }

    // A distro with no punktfunk repo stops a host install only: a client install takes the
    // flatpak line. Checked after the screen, because switching to client-only is how a
    // user gets past it.
    if choices.action != Action::Uninstall
        && choices.components.host
        && let Some(msg) = &facts.host_punt
    {
        ui.die(msg);
        return ExitCode::FAILURE;
    }

    let plan = plan::build(&facts, &choices);
    let demo_runner = demo.as_ref().map(|_| {
        let at = cli
            .fail
            .as_deref()
            .and_then(|phase| punktfunk_setup::demo::fail_index(&plan, phase));
        punktfunk_setup::demo::DemoRunner::new(DEMO_LATENCY_MS, at)
    });
    let run: &dyn CommandRunner = match &demo_runner {
        Some(demo) => demo,
        None => &runner,
    };
    let exec = Executor {
        paths: &paths,
        run,
        ui,
        opts,
    };
    if let Some(t) = &tui
        && !cli.verbose
        && !opts.dry
    {
        t.begin_progress(plan.phases.len());
    }
    let outcome = match exec.execute(&plan, &facts, &choices) {
        Ok(outcome) => outcome,
        Err(failed) => {
            ui.die(&failed.0);
            return ExitCode::FAILURE;
        }
    };

    if let Some(t) = &tui {
        t.end_progress();
    }

    if choices.action == Action::Uninstall {
        report::uninstall_outro(ui);
        return ExitCode::SUCCESS;
    }
    // A hand-off ends the plan early and its own summary went into the progress view's capture
    // buffer, so this is the only report that says whether the box ended up working.
    report::verify(ui, run, &facts, &choices, &outcome, opts);
    ExitCode::SUCCESS
}

fn preflight(env: &Env, paths: &BasePaths, runner: &mut SystemRunner) -> Result<(), String> {
    let linux =
        std::env::consts::OS == "linux" || env.get("PUNKTFUNK_INSTALL_OS_RELEASE").is_some();
    if !linux {
        return Err(format!(
            "this installer is for Linux hosts — Windows: {DOCS}/windows-host"
        ));
    }
    let root = runner.first_line("id", &["-u"]).as_deref() == Some("0");
    if env.get("SUDO_USER").is_some() && root {
        return Err("run this as your normal user, not under sudo — it calls sudo itself where needed, and the host runs as you (host.env, the services)".into());
    }
    if !runner.which("curl") {
        return Err("curl is required (install it with your package manager first)".into());
    }
    // Root without sudo (a minimal Debian container): a shim so the verbatim `sudo …`
    // lines from platforms.json still work.
    if root && !runner.which("sudo") {
        let dir = std::env::temp_dir().join(format!("punktfunk-setup-{}", std::process::id()));
        if std::fs::create_dir_all(&dir).is_ok()
            && std::fs::write(dir.join("sudo"), "#!/bin/sh\nexec \"$@\"\n").is_ok()
        {
            set_executable(&dir.join("sudo"));
            runner.path_prefix = Some(dir);
        }
    }
    if !paths.os_release.exists() {
        return Err(format!(
            "no /etc/os-release — can't tell which distro this is: {DOCS}/install"
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn set_executable(_path: &std::path::Path) {}

fn load_facts(
    cli: &Cli,
    paths: &BasePaths,
    runner: &SystemRunner,
    env: &Env,
) -> Result<Facts, String> {
    if let Some(file) = &cli.facts_file {
        let text = std::fs::read_to_string(file)
            .map_err(|e| format!("couldn't read {}: {e}", file.display()))?;
        return serde_json::from_str(&text)
            .map_err(|e| format!("{} is not a Facts document: {e}", file.display()));
    }
    Facts::probe(paths, runner, env).map_err(|punt| punt.message())
}

/// Default behind Enter. Without a terminal the default stands, so a version-floor confirm
/// that defaults to no aborts under `--yes`.
fn ask(interactive: bool, question: &str) -> bool {
    if !interactive {
        return false;
    }
    use std::io::{BufRead, Write};
    let Ok(mut tty) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    else {
        return false;
    };
    let _ = write!(tty, "? {question} [y/N] ");
    let _ = tty.flush();
    let mut answer = String::new();
    let Ok(tty_read) = std::fs::File::open("/dev/tty") else {
        return false;
    };
    if std::io::BufReader::new(tty_read)
        .read_line(&mut answer)
        .is_err()
    {
        return false;
    }

    matches!(answer.trim(), "y" | "Y" | "yes" | "YES")
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_setup::facts::Channel;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn an_unknown_option_is_bad_usage() {
        let err = parse(args(&["--nope"]), &Env::default()).err().unwrap();
        assert_eq!(err.0, BAD_USAGE);
        assert!(err.1.starts_with("unknown option: --nope"));
    }

    #[test]
    fn help_exits_zero_and_names_every_env_twin() {
        let err = parse(args(&["--help"]), &Env::default()).err().unwrap();
        assert_eq!(err.0, 0);
        for twin in [
            "PUNKTFUNK_INSTALL_YES",
            "PUNKTFUNK_INSTALL_CHANNEL",
            "PUNKTFUNK_INSTALL_GAMESTREAM",
            "PUNKTFUNK_INSTALL_CLIPBOARD",
            "PUNKTFUNK_INSTALL_PUNKTFUNK_GROUP",
            "PUNKTFUNK_INSTALL_LINGER",
            "PUNKTFUNK_INSTALL_OMARCHY_SETUP",
            "PUNKTFUNK_INSTALL_MGMT_PORT",
        ] {
            assert!(err.1.contains(twin), "--help does not mention {twin}");
        }
    }

    #[test]
    fn a_bad_channel_or_port_is_bad_usage() {
        assert_eq!(
            parse(args(&["--channel", "beta"]), &Env::default())
                .err()
                .unwrap()
                .0,
            BAD_USAGE
        );
        assert_eq!(
            parse(args(&["--mgmt-port", "http"]), &Env::default())
                .err()
                .unwrap()
                .0,
            BAD_USAGE
        );
    }

    #[test]
    fn both_the_split_and_the_equals_spelling_parse() {
        let split = parse(args(&["--channel", "canary"]), &Env::default()).unwrap();
        let equals = parse(args(&["--channel=canary"]), &Env::default()).unwrap();
        assert_eq!(split.pins.channel, Some(Channel::Canary));
        assert_eq!(equals.pins.channel, Some(Channel::Canary));
    }

    #[test]
    fn a_flag_overrides_its_env_twin() {
        let env = Env::of(&[
            ("PUNKTFUNK_INSTALL_CHANNEL", "canary"),
            ("PUNKTFUNK_INSTALL_PUNKTFUNK_GROUP", "1"),
        ]);
        let cli = parse(args(&["--channel", "stable", "--no-punktfunk-group"]), &env).unwrap();
        assert_eq!(cli.pins.channel, Some(Channel::Stable));
        assert_eq!(cli.pins.punktfunk_group, Some(false));
    }

    #[test]
    fn env_twins_stand_when_no_flag_contradicts_them() {
        let env = Env::of(&[
            ("PUNKTFUNK_INSTALL_YES", "1"),
            ("PUNKTFUNK_INSTALL_LINGER", "0"),
            ("PUNKTFUNK_INSTALL_OMARCHY_SETUP", "0"),
            ("PUNKTFUNK_INSTALL_MGMT_PORT", "48000"),
        ]);
        let cli = parse(vec![], &env).unwrap();
        assert!(cli.yes);
        assert_eq!(cli.pins.linger, Some(false));
        assert_eq!(cli.pins.omarchy_setup, Some(false));
        assert_eq!(cli.pins.mgmt_port, Some(48000));
    }

    // Smoke needs a way to decline the Omarchy hand-off without a TTY.
    #[test]
    fn the_omarchy_hand_off_has_a_flag_and_an_env_twin() {
        let flag = parse(args(&["--no-omarchy-setup"]), &Env::default()).unwrap();
        assert_eq!(flag.pins.omarchy_setup, Some(false));
        let env = Env::of(&[("PUNKTFUNK_INSTALL_OMARCHY_SETUP", "1")]);
        assert_eq!(parse(vec![], &env).unwrap().pins.omarchy_setup, Some(true));
    }

    #[test]
    fn components_default_to_host_and_client_is_opt_in() {
        assert!(!parse(vec![], &Env::default()).unwrap().pins.client);
        assert!(
            parse(args(&["--client"]), &Env::default())
                .unwrap()
                .pins
                .client
        );
        let both = parse(args(&["--host", "--client"]), &Env::default()).unwrap();
        assert!(both.pins.host && both.pins.client);
    }
}
