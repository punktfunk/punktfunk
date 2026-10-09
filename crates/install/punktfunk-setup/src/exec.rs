//! Stage four: run a `Plan`. Echo, sudo shim, no-confirm rewrites, TTY stdin, per-step results.
//!
//! `--dry-run` walks the same code and returns before the spawn, so what dry-run prints
//! is what a real run executes. There is no second rendering path to drift.
//!
//! Four `StepAction`s resolve here rather than in `plan`, because each needs something
//! the previous step created: apt madison pins need the rewritten repo, pacman's
//! availability split needs the `-Sy`, and the unit enable needs the user manager
//! linger may have just made. See `design/installer-v2.md`.

use crate::choices::Choices;
use crate::facts::{Channel, Facts, DOCS};
use crate::plan::{Level, Plan, StepAction};
use crate::seam::{BasePaths, CommandRunner, Stdin};
use crate::ui::Reporter;

/// A package manager must not stop for its own confirmation: running the installer is the
/// consent, and the settings screen already listed what it installs. Ported verbatim from the
/// sh installer's `--yes` rewrite table; `-Syu` is tested before `-S` so it wins.
const NONINTERACTIVE: [(&str, &str); 11] = [
    ("flatpak install --user ", "flatpak install --user -y "),
    ("flatpak uninstall --user ", "flatpak uninstall --user -y "),
    ("sudo apt install ", "sudo apt install -y "),
    ("sudo dnf install ", "sudo dnf install -y "),
    ("sudo pacman -Syu ", "sudo pacman -Syu --noconfirm "),
    ("sudo pacman -S ", "sudo pacman -S --noconfirm "),
    ("sudo dnf distro-sync ", "sudo dnf distro-sync -y "),
    ("sudo apt purge ", "sudo apt purge -y "),
    ("sudo dnf remove ", "sudo dnf remove -y "),
    ("sudo pacman -Rns ", "sudo pacman -Rns --noconfirm "),
    ("sudo pacman -Rdd ", "sudo pacman -Rdd --noconfirm "),
];

/// The NSS nickname the trust entry is filed under — the same one `punktfunk-omarchy` uses, so
/// either path replaces the other's entry instead of stacking a second.
const CERT_NICK: &str = "punktfunk-console";

pub fn noninteractive(cmd: &str) -> String {
    for (from, to) in NONINTERACTIVE {
        if let Some(rest) = cmd.strip_prefix(from) {
            return format!("{to}{rest}");
        }
    }
    cmd.to_string()
}

#[derive(Debug, Clone, Copy)]
pub struct Opts {
    pub dry: bool,
    /// The progress line is up: a step's output is captured and shown only when it fails.
    pub quiet: bool,
    /// A terminal sudo's password prompt can reach.
    pub tty: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub relogin: bool,
    pub started: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed(pub String);

pub struct Executor<'a> {
    pub paths: &'a BasePaths,
    pub run: &'a dyn CommandRunner,
    pub ui: &'a dyn Reporter,
    pub opts: Opts,
}

impl Executor<'_> {
    pub fn execute(
        &self,
        plan: &Plan,
        facts: &Facts,
        choices: &Choices,
    ) -> Result<Outcome, Failed> {
        // Client-only enables no services; verify must not look for a host never installed.
        let mut outcome = Outcome {
            started: choices.start && choices.components.host,
            ..Outcome::default()
        };
        for phase in &plan.phases {
            self.ui.say(&phase.title);
            for step in &phase.steps {
                let ran = self.step(&step.action, facts, choices, &mut outcome)?;
                // A skipped hand-off did nothing, so the generic wiring it would replace still runs.
                if step.ends_run && ran && !self.opts.dry {
                    return Ok(outcome);
                }
            }
        }
        Ok(outcome)
    }

    /// `Ok(true)` when the step did something. A skipped conditional step is `false`.
    fn step(
        &self,
        action: &StepAction,
        facts: &Facts,
        choices: &Choices,
        outcome: &mut Outcome,
    ) -> Result<bool, Failed> {
        match action {
            StepAction::Run(cmd) => {
                // Group changes apply only after re-login; the outro must say so. The
                // SteamOS build joins input and punktfunk itself, and its own notice is captured.
                if cmd.contains("usermod -aG")
                    || cmd.contains("add-user-to-input-group")
                    || cmd.contains("steamdeck/install.sh")
                {
                    outcome.relogin = true;
                }
                self.shell(cmd, facts).map(|()| true)
            }
            StepAction::RunIfPresent {
                program,
                cmd,
                warn_if_missing,
            } => {
                if !self.opts.dry && !self.run.which(program) {
                    if let Some(text) = warn_if_missing {
                        self.ui.warn(text);
                    }
                    return Ok(false);
                }
                self.shell(cmd, facts).map(|()| true)
            }
            StepAction::Note(Level::Ok, text) => {
                self.ui.ok(text);
                Ok(false)
            }
            StepAction::Note(Level::Warn, text) => {
                self.ui.warn(text);
                Ok(false)
            }
            StepAction::SetEnv { key, value } => {
                self.set_env(key, value);
                Ok(true)
            }
            StepAction::SetSetting { id, value } => {
                self.set_setting(id, value);
                Ok(true)
            }
            StepAction::AptSwitch { pkgs } => self.apt_switch(pkgs, facts, choices).map(|()| true),
            StepAction::PacmanSwitch { pkgs } => self.pacman_switch(pkgs, facts).map(|()| true),
            StepAction::Linger => self.linger(facts),
            StepAction::StartUnits { units } => self.start_units(units, outcome, facts),
            // No password, no file: an empty `PUNKTFUNK_UI_PASSWORD=` line is a console
            // that admits nobody, which is worse than the generated one this skips to.
            StepAction::WebPassword => match choices.web_password.as_deref() {
                Some(password) => {
                    self.web_password(password);
                    Ok(true)
                }
                None => Ok(false),
            },
            StepAction::TrustCert => self.trust_cert(facts, outcome),
        }
    }

    /// Echo the command, then run it — the non-interactive form is what both the echo and the
    /// spawn get, so the transcript is what actually ran.
    fn shell(&self, cmd: &str, facts: &Facts) -> Result<(), Failed> {
        let cmd = noninteractive(cmd);
        self.ui.plus(&cmd);
        if self.opts.dry {
            return Ok(());
        }
        let stdin = if self.opts.tty {
            Stdin::Tty
        } else {
            Stdin::Null
        };
        let failed = format!(
            "that step failed — fix it and re-run (the script is safe to repeat), or follow the page by hand: {}",
            facts.docs_page
        );
        if !self.opts.quiet {
            return self.run.run_shell(&cmd, stdin).map_err(|_| Failed(failed));
        }
        // Nothing was echoed while the step ran, so the tail of its output is the only clue
        // the user gets. Thirty lines: a package manager's error sits at the end.
        self.run.run_shell_quiet(&cmd, stdin).map_err(|output| {
            let skip = output.len().saturating_sub(30);
            let mut text = failed;
            if skip < output.len() {
                text.push_str("\n   the step's last lines:");
                for line in &output[skip..] {
                    text.push_str("\n     ");
                    text.push_str(line);
                }
            }
            Failed(text)
        })
    }

    /// Chromium reads the per-user NSS store, so one `certutil -A` there makes the console open
    /// with no security warning. Firefox keeps its own store and still shows one. The host
    /// mints `native-cert.pem` on its first start, after the unit enable returns, so this
    /// waits for the file. Delete-then-add so a re-minted identity replaces the old trust.
    fn trust_cert(&self, facts: &Facts, outcome: &Outcome) -> Result<bool, Failed> {
        let cert = self.paths.config.join("punktfunk/native-cert.pem");
        let db = self.paths.home.join(".pki/nssdb");
        let shown = |p: &std::path::Path| p.display().to_string().replace('\\', "/");
        let db_arg = format!("sql:{}", shown(&db));
        let cmd = format!(
            "certutil -A -d {db_arg} -n {CERT_NICK} -t C,, -i {}",
            shown(&cert)
        );
        if self.opts.dry {
            self.ui.plus(&cmd);
            return Ok(true);
        }
        if !self.run.which("certutil") {
            self.ui.warn(&format!(
                "certutil is not installed, so the console keeps its browser warning — {} and re-run",
                facts.family.certutil_package()
            ));
            return Ok(false);
        }
        if !outcome.started {
            self.ui.warn("the host is not running, so there is no certificate to trust yet — re-run once it is");
            return Ok(false);
        }
        // Through the runner, not the filesystem: a demo box answers at once instead of
        // waiting five seconds for a file its sandbox never writes.
        let wait = format!(
            "for i in $(seq 20); do [ -r '{}' ] && exit 0; sleep 0.25; done; exit 1",
            shown(&cert)
        );
        if !self.run.probe("sh", &["-c", &wait]).is_some_and(|o| o.ok()) {
            self.ui.warn(&format!(
                "the host has not written its certificate yet ({}) — re-run to trust it",
                shown(&cert)
            ));
            return Ok(false);
        }
        let db_file = format!("{}/cert9.db", shown(&db));
        if !self
            .run
            .probe("test", &["-f", &db_file])
            .is_some_and(|o| o.ok())
        {
            let _ = self.run.probe("mkdir", &["-p", &shown(&db)]);
            let _ = self
                .run
                .probe("certutil", &["-N", "-d", &db_arg, "--empty-password"]);
        }
        let _ = self
            .run
            .probe("certutil", &["-D", "-d", &db_arg, "-n", CERT_NICK]);
        self.ui.plus(&cmd);
        let added = self
            .run
            .probe(
                "certutil",
                &[
                    "-A",
                    "-d",
                    &db_arg,
                    "-n",
                    CERT_NICK,
                    "-t",
                    "C,,",
                    "-i",
                    &shown(&cert),
                ],
            )
            .is_some_and(|o| o.ok());
        if added {
            self.ui.ok("console certificate trusted — a Chromium that is already running picks it up on restart");
        } else {
            self.ui.warn("certutil refused the certificate — the browser warning stays; click through once instead");
        }
        Ok(true)
    }

    /// The console's login password, in the file its unit reads as an `EnvironmentFile`.
    /// 0600 and never echoed — `web-init.sh` writes a generated one only when this is absent,
    /// so landing it before the units start is what makes the user's own password the one.
    /// The console salts and hashes this line the first time it signs someone in, and the file
    /// then holds only the hash; writing a fresh line back here is how the password is reset.
    fn web_password(&self, password: &str) {
        let path = self.paths.config.join("punktfunk/web-password");
        let shown = path.display().to_string().replace('\\', "/");
        if self.opts.dry {
            self.ui.ok(&format!("would write your password to {shown}"));
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
            set_mode(dir, 0o700);
        }
        match std::fs::write(&path, format!("PUNKTFUNK_UI_PASSWORD={password}\n")) {
            Ok(()) => {
                set_mode(&path, 0o600);
                self.ui.ok(&format!("your password → {shown}"));
            }
            // Not fatal: the console generates one on its first start, and the outro says
            // where to read it.
            Err(e) => self.ui.warn(&format!(
                "couldn't write {shown} — {e}; the console will generate one instead"
            )),
        }
    }

    /// Replace or append one `KEY=VALUE` line in host.env, creating it on first use.
    fn set_env(&self, key: &str, value: &str) {
        let path = self.paths.host_env();
        // Linux box paths; goldens must match on every OS. PathBuf::join writes `\` on Windows.
        let shown = path.display().to_string().replace('\\', "/");
        if self.opts.dry {
            self.ui.ok(&format!("would set {key}={value} in {shown}"));
            return;
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let prefix = format!("{key}=");
        let mut lines: Vec<String> = existing.lines().map(str::to_string).collect();
        match lines.iter_mut().find(|l| l.starts_with(&prefix)) {
            Some(line) => *line = format!("{key}={value}"),
            None => lines.push(format!("{key}={value}")),
        }
        let mut body = lines.join("\n");
        body.push('\n');
        if std::fs::write(&path, body).is_ok() {
            self.ui.ok(&format!("{key}={value} → {shown}"));
        } else {
            self.ui.warn(&format!("couldn't write {shown}"));
        }
    }

    /// Merge one key into `host-settings.json` through the host's own store: the registry
    /// validates it, every other key stays, and the file is owner-only as the host writes it.
    fn set_setting(&self, id: &str, value: &serde_json::Value) {
        let path = self.paths.host_settings();
        let shown = path.display().to_string().replace('\\', "/");
        if self.opts.dry {
            self.ui.ok(&format!("would set {id}={value} in {shown}"));
            return;
        }
        let patch = serde_json::Map::from_iter([(id.to_string(), value.clone())]);
        match pf_host_config::save_at(&path, &patch) {
            Ok(()) => self.ui.ok(&format!("{id}={value} → {shown}")),
            Err(e) => self.ui.warn(&format!("couldn't write {shown} — {e}")),
        }
    }

    /// apt will not walk back to a lower candidate, so name the exact version. After the repo
    /// rewrite the target channel is the only punktfunk source, so madison's first row is it.
    fn apt_switch(&self, pkgs: &[String], facts: &Facts, choices: &Choices) -> Result<(), Failed> {
        let mut pins = String::new();
        for pkg in pkgs {
            if self.opts.dry {
                pins.push_str(&format!(" {pkg}=<version>"));
                continue;
            }
            // A package the target channel does not carry keeps what it has. Naming it with no
            // version would drag it to the highest version from any source.
            if let Some(version) = self
                .run
                .first_line("apt-cache", &["madison", pkg])
                .and_then(|l| l.split_whitespace().nth(2).map(str::to_string))
            {
                pins.push_str(&format!(" {pkg}={version}"));
            }
        }
        if pins.is_empty() {
            return Err(Failed(format!(
                "the {} apt channel offers no punktfunk packages — check /etc/apt/sources.list.d/punktfunk.list ({DOCS}/channels)",
                choices.channel.as_str()
            )));
        }
        self.shell(&format!("sudo apt install --allow-downgrades{pins}"), facts)
    }

    /// A package the target does not carry cannot be named (the transaction aborts) or kept
    /// (files collide), so a switch installs exactly what the target offers and removes the rest.
    fn pacman_switch(&self, pkgs: &[String], facts: &Facts) -> Result<(), Failed> {
        let mut want = String::new();
        let mut drop = String::new();
        for pkg in pkgs {
            let available = self.opts.dry
                || self
                    .run
                    .probe("pacman", &["-Si", pkg])
                    .is_some_and(|o| o.ok());
            if available {
                want.push_str(&format!(" {pkg}"));
            } else {
                let channel = facts.current_channel.map_or("target", Channel::as_str);
                self.ui.warn(&format!(
                    "{pkg} is not on the {channel} channel — removing it (the {channel} packages carry its files)"
                ));
                drop.push_str(&format!(" {pkg}"));
            }
        }
        // -Rdd, not -R: the next -S replaces whatever depended on the leaving package. A
        // dependency check against the outgoing set would refuse the removal that makes room.
        if !drop.is_empty() {
            self.shell(&format!("sudo pacman -Rdd{drop}"), facts)?;
        }
        self.shell(&format!("sudo pacman -S{want}"), facts)
    }

    /// A container has no logind: enable-linger fails and would mean nothing. `--dry-run`
    /// still prints the command — it reports what a real box would do.
    fn linger(&self, facts: &Facts) -> Result<bool, Failed> {
        if self.opts.dry || facts.systemd_pid1 {
            return self
                .shell(r#"sudo loginctl enable-linger "$USER""#, facts)
                .map(|()| true);
        }
        self.ui.warn(
            "no systemd as PID 1 here (a container?) — skipping linger, nothing would honour it",
        );
        Ok(false)
    }

    /// Probed here, not in `plan`: linger may have just created the user manager. Asking
    /// earlier prints the enable command and stops for nothing.
    fn start_units(
        &self,
        units: &[String],
        outcome: &mut Outcome,
        facts: &Facts,
    ) -> Result<bool, Failed> {
        let live = self
            .run
            .probe("systemctl", &["--user", "show-environment"])
            .is_some_and(|o| o.ok());
        if !live {
            self.ui.warn(
                "no user systemd session here (ssh without a login session?) — run this from a terminal in your desktop session:",
            );
            self.ui
                .detail("systemctl --user enable --now punktfunk-host punktfunk-web");
            outcome.started = false;
            return Ok(false);
        }
        if !self.opts.dry {
            let _ = self.run.probe("systemctl", &["--user", "daemon-reload"]);
        }
        self.shell(
            &format!("systemctl --user enable --now {}", units.join(" ")),
            facts,
        )
        .map(|()| true)
    }
}

/// Owner-only bits for the password file and the config dir. A umask cannot be trusted to
/// have done it: 0022 leaves a secret world-readable.
#[cfg(unix)]
fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &std::path::Path, _mode: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_yes_rewrite_table_matches_the_sh_installer() {
        assert_eq!(
            noninteractive("sudo apt install punktfunk-host"),
            "sudo apt install -y punktfunk-host"
        );
        assert_eq!(
            noninteractive("sudo pacman -Syu punktfunk-host"),
            "sudo pacman -Syu --noconfirm punktfunk-host"
        );
        assert_eq!(
            noninteractive("sudo pacman -S punktfunk-host"),
            "sudo pacman -S --noconfirm punktfunk-host"
        );
        assert_eq!(
            noninteractive("sudo pacman -Rdd punktfunk-icons"),
            "sudo pacman -Rdd --noconfirm punktfunk-icons"
        );
    }

    // -Syu is not a prefix of -S; the -Syu rule is first. Reverse them and `-Syu` gets `--noconfirm` twice.
    #[test]
    fn syu_is_rewritten_once_not_twice() {
        let once = noninteractive("sudo pacman -Syu punktfunk-host");
        assert_eq!(once.matches("--noconfirm").count(), 1);
    }

    #[test]
    fn a_command_with_no_rule_is_left_alone() {
        assert_eq!(
            noninteractive("sudo ufw allow punktfunk-web"),
            "sudo ufw allow punktfunk-web"
        );
        assert_eq!(
            noninteractive("systemctl --user enable --now punktfunk-host"),
            "systemctl --user enable --now punktfunk-host"
        );
    }
}
