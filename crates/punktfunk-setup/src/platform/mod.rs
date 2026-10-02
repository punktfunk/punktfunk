//! One package family per implementation, behind `PkgBackend`.
//!
//! A winget backend produces the same `Step`s and every UI renders them unchanged
//! (`design/installer-v2.md` D3). Nothing above this module knows apt or pacman.
//!
//! Install commands are generated from the embedded `data/platforms.json`, so the D6
//! "verbatim" promise holds by construction. What can still drift is the split — which
//! entry is the repo block and which is the install — so `shape` tests pin every family.

pub mod windows;

use std::sync::OnceLock;

use crate::choices::Choices;
use crate::facts::{Channel, Facts, Family, DOCS, FLATPAK_APP};
use crate::plan::{switch_pkgs, Level, Step, StepAction};
use crate::seam::{BasePaths, CommandRunner};

/// The single source for every install line (`design/installer-v2.md` D6).
const PLATFORMS_JSON: &str = include_str!("../../../../data/platforms.json");

/// Removes the stable section, the canary section, or both.
const PACMAN_RM_REPO: &str =
    r"sudo sed -i '/^\[punktfunk\(-canary\)\{0,1\}\]$/,/^Server = /d' /etc/pacman.conf";

fn platforms() -> &'static serde_json::Value {
    static PARSED: OnceLock<serde_json::Value> = OnceLock::new();
    PARSED.get_or_init(|| {
        serde_json::from_str(PLATFORMS_JSON).expect("data/platforms.json is not valid JSON")
    })
}

pub fn install_lines(id: &str) -> Vec<String> {
    platforms()["platforms"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["id"] == id)
        .and_then(|p| p["install"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().map(str::to_string))
        .collect()
}

/// The platform's one-line floor, worded as the download page shows it. Empty for an
/// unknown id.
pub fn floor(id: &str) -> String {
    platforms()["platforms"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["id"] == id)
        .and_then(|p| p["floor"].as_str())
        .unwrap_or_default()
        .to_string()
}

/// Lines before `marker` are the repo block; the rest is the install.
fn split_at(id: &str, marker: &str) -> (Vec<String>, Vec<String>) {
    let lines = install_lines(id);
    let at = lines
        .iter()
        .position(|l| l.starts_with(marker))
        .unwrap_or_else(|| panic!("platforms.json '{id}' has no line starting with '{marker}'"));
    (lines[..at].to_vec(), lines[at..].to_vec())
}

pub trait PkgBackend {
    /// dnf's host package is `punktfunk`, not `punktfunk-host`.
    fn base_pkgs(&self) -> Vec<&'static str>;
    fn write_repo(&self, facts: &Facts, choices: &Choices) -> Vec<Step>;
    fn install(&self, facts: &Facts, choices: &Choices) -> Vec<Step>;
    fn switch(&self, facts: &Facts, choices: &Choices) -> Vec<Step>;
    fn uninstall(&self, facts: &Facts) -> Vec<Step>;
    fn current_channel(&self, paths: &BasePaths, run: &dyn CommandRunner) -> Option<Channel>;
    fn installed_pf(&self, run: &dyn CommandRunner) -> Vec<String>;
}

pub fn backend(family: Family) -> &'static dyn PkgBackend {
    match family {
        Family::Apt => &Apt,
        Family::Dnf => &Dnf,
        Family::Pacman => &Pacman,
        Family::Sysext => &Sysext,
        Family::Steamos => &Steamos,
        Family::Flatpak => &Flatpak,
    }
}

/// Keep the platforms.json line verbatim; append or swap in `punktfunk-client` when asked.
fn compose_install(base_line: &str, choices: &Choices) -> String {
    match (choices.components.host, choices.components.client) {
        (true, true) => format!("{base_line} punktfunk-client"),
        (false, true) => client_only(base_line),
        _ => base_line.to_string(),
    }
}

/// Reuse the family's flags; a hand-written `pacman -S` dropped `-Syu` and failed
/// "target not found" against a never-fetched repo.
fn client_only(base_line: &str) -> String {
    let head: Vec<&str> = base_line
        .split_whitespace()
        .take_while(|word| !word.starts_with("punktfunk"))
        .collect();
    format!("{} punktfunk-client", head.join(" "))
}

struct Apt;

impl PkgBackend for Apt {
    fn base_pkgs(&self) -> Vec<&'static str> {
        vec!["punktfunk-host", "punktfunk-web", "punktfunk-scripting"]
    }

    fn write_repo(&self, _facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (repo, _) = split_at("debian", "sudo apt install");
        repo.into_iter()
            .map(|line| {
                Step::run(if choices.channel == Channel::Canary {
                    line.replace(" stable main", " canary main")
                } else {
                    line
                })
            })
            .collect()
    }

    fn install(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (_, install) = split_at("debian", "sudo apt install");
        let mut steps = self.write_repo(facts, choices);
        steps.push(Step::run(compose_install(&install[0], choices)));
        steps
    }

    fn switch(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let mut steps = self.write_repo(facts, choices);
        steps.push(Step {
            action: StepAction::AptSwitch {
                pkgs: switch_pkgs(&self.base_pkgs(), &facts.installed_pf),
            },
            ends_run: false,
        });
        steps
    }

    fn uninstall(&self, facts: &Facts) -> Vec<Step> {
        let mut steps = vec![Step::run(UNIT_TEARDOWN)];
        if !facts.installed_pf.is_empty() {
            steps.push(Step::run(format!(
                "sudo apt purge {}",
                facts.installed_pf.join(" ")
            )));
        }
        steps.push(Step::run(
            "sudo rm -f /etc/apt/sources.list.d/punktfunk.list /etc/apt/keyrings/punktfunk.asc",
        ));
        steps.push(Step::run("sudo apt update"));
        steps
    }

    fn current_channel(&self, paths: &BasePaths, _run: &dyn CommandRunner) -> Option<Channel> {
        let text = paths.read(&paths.etc("apt/sources.list.d/punktfunk.list"))?;
        Some(if text.contains(" canary main") {
            Channel::Canary
        } else {
            Channel::Stable
        })
    }

    fn installed_pf(&self, run: &dyn CommandRunner) -> Vec<String> {
        run.probe(
            "dpkg-query",
            &["-W", "-f=${Package} ${db:Status-Status}\n", "punktfunk*"],
        )
        .map(|o| {
            o.stdout
                .lines()
                .filter_map(|l| {
                    let mut it = l.split_whitespace();
                    let name = it.next()?;
                    (it.next() == Some("installed")).then(|| name.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
    }
}

struct Dnf;

impl PkgBackend for Dnf {
    fn base_pkgs(&self) -> Vec<&'static str> {
        vec!["punktfunk", "punktfunk-web", "punktfunk-scripting"]
    }

    /// The repo block is one heredoc, so its lines rejoin into a single command.
    fn write_repo(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (repo, _) = split_at("fedora", "sudo dnf install");
        let mut steps = vec![Step::run(repo.join("\n"))];
        let group = match (facts.rpm_group.as_deref(), choices.channel) {
            (Some(g), Channel::Canary) => format!("{g}-canary"),
            (Some(g), Channel::Stable) => g.to_string(),
            (None, _) => return steps,
        };
        if group != "fedora-44" {
            steps.push(Step::run(format!(
                "sudo sed -i 's|/rpm/fedora-44|/rpm/{group}|' /etc/yum.repos.d/punktfunk.repo"
            )));
        }
        steps
    }

    fn install(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (_, install) = split_at("fedora", "sudo dnf install");
        let mut steps = self.write_repo(facts, choices);
        steps.push(Step::run(compose_install(&install[0], choices)));
        steps
    }

    /// `install` covers up and missing; `distro-sync` is what walks a version back down.
    fn switch(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (_, install) = split_at("fedora", "sudo dnf install");
        let mut steps = self.write_repo(facts, choices);
        steps.push(Step::run(install[0].clone()));
        steps.push(Step::run(format!(
            "sudo dnf distro-sync {}",
            switch_pkgs(&self.base_pkgs(), &facts.installed_pf).join(" ")
        )));
        steps
    }

    fn uninstall(&self, facts: &Facts) -> Vec<Step> {
        let mut steps = vec![Step::run(UNIT_TEARDOWN)];
        if !facts.installed_pf.is_empty() {
            steps.push(Step::run(format!(
                "sudo dnf remove {}",
                facts.installed_pf.join(" ")
            )));
        }
        steps.push(Step::run("sudo rm -f /etc/yum.repos.d/punktfunk.repo"));
        steps
    }

    fn current_channel(&self, paths: &BasePaths, _run: &dyn CommandRunner) -> Option<Channel> {
        let text = paths.read(&paths.etc("yum.repos.d/punktfunk.repo"))?;
        let canary = text
            .lines()
            .any(|l| l.starts_with("baseurl=") && l.contains("-canary"));
        Some(if canary {
            Channel::Canary
        } else {
            Channel::Stable
        })
    }

    fn installed_pf(&self, run: &dyn CommandRunner) -> Vec<String> {
        run.probe("rpm", &["-qa", "--qf", "%{NAME} ", "punktfunk*"])
            .map(|o| o.stdout.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }
}

struct Pacman;

impl Pacman {
    /// Same repo and packages; Omarchy's transaction shape is a different platforms.json entry.
    fn entry(facts: &Facts) -> &'static str {
        if facts.omarchy {
            "omarchy"
        } else {
            "arch"
        }
    }
}

impl PkgBackend for Pacman {
    fn base_pkgs(&self) -> Vec<&'static str> {
        vec!["punktfunk-host", "punktfunk-web", "punktfunk-scripting"]
    }

    fn write_repo(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (repo, _) = split_at(Self::entry(facts), "sudo pacman -S");
        repo.into_iter()
            .map(|line| {
                Step::run(if choices.channel == Channel::Canary {
                    // Both the grep guard (escaped brackets) and the printf body name the repo.
                    line.replace(r"punktfunk\]", r"punktfunk-canary\]")
                        .replace("[punktfunk]", "[punktfunk-canary]")
                } else {
                    line
                })
            })
            .collect()
    }

    /// Omarchy's libalpm hook aborts `-S` with `-u`, so `-Sy` then `-S`. The trailing
    /// hand-off line belongs to the Omarchy phase, not this one.
    fn install(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let (_, install) = split_at(Self::entry(facts), "sudo pacman -S");
        let mut steps = self.write_repo(facts, choices);
        let pkg_lines: Vec<&String> = install
            .iter()
            .filter(|l| l.starts_with("sudo pacman"))
            .collect();
        let (last, rest) = pkg_lines
            .split_last()
            .expect("pacman entry has an install line");
        steps.extend(rest.iter().map(|l| Step::run(l.to_string())));
        steps.push(Step::run(compose_install(last, choices)));
        steps
    }

    /// Drop the old section first or both repos stay enabled. Then `-Sy` and `-S`, never
    /// `-Syu`: a lower version on the way home is a no-op.
    fn switch(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        let mut steps = vec![Step::run(PACMAN_RM_REPO)];
        steps.extend(self.write_repo(facts, choices));
        steps.push(Step::run("sudo pacman -Sy"));
        steps.push(Step {
            action: StepAction::PacmanSwitch {
                pkgs: switch_pkgs(&self.base_pkgs(), &facts.installed_pf),
            },
            ends_run: false,
        });
        steps
    }

    /// `punktfunk-omarchy remove` ships in the host package, so it must run before pacman
    /// takes the binary away.
    fn uninstall(&self, facts: &Facts) -> Vec<Step> {
        let mut steps = vec![Step::run(UNIT_TEARDOWN)];
        if facts.omarchy {
            // Idempotent, and silent if setup never ran. Skip only when the binary is gone,
            // not planned away.
            steps.push(Step {
                action: StepAction::RunIfPresent {
                    program: "punktfunk-omarchy".into(),
                    cmd: "punktfunk-omarchy remove".into(),
                    warn_if_missing: None,
                },
                ends_run: false,
            });
        }
        if !facts.installed_pf.is_empty() {
            steps.push(Step::run(format!(
                "sudo pacman -Rns {}",
                facts.installed_pf.join(" ")
            )));
        }
        steps.push(Step::run(PACMAN_RM_REPO));
        steps
    }

    fn current_channel(&self, paths: &BasePaths, _run: &dyn CommandRunner) -> Option<Channel> {
        let text = paths.read(&paths.etc("pacman.conf"))?;
        if text.lines().any(|l| l.trim_end() == "[punktfunk-canary]") {
            Some(Channel::Canary)
        } else if text.lines().any(|l| l.trim_end() == "[punktfunk]") {
            Some(Channel::Stable)
        } else {
            None
        }
    }

    fn installed_pf(&self, run: &dyn CommandRunner) -> Vec<String> {
        run.probe("pacman", &["-Qq"])
            .map(|o| {
                o.stdout
                    .lines()
                    .map(str::trim)
                    .filter(|l| l.starts_with("punktfunk"))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }
}

struct Sysext;

impl PkgBackend for Sysext {
    fn base_pkgs(&self) -> Vec<&'static str> {
        vec![]
    }

    /// No repo file; punktfunk-sysext records the channel in its own conf.
    fn write_repo(&self, _facts: &Facts, _choices: &Choices) -> Vec<Step> {
        vec![]
    }

    fn install(&self, _facts: &Facts, choices: &Choices) -> Vec<Step> {
        let lines = install_lines("bazzite");
        let (install, fetch) = lines
            .split_last()
            .expect("bazzite entry has an install line");
        let mut steps: Vec<Step> = fetch.iter().map(Step::run).collect();
        steps.push(Step::run(if choices.channel == Channel::Canary {
            format!("{install} --channel canary")
        } else {
            install.clone()
        }));
        steps
    }

    /// The sysext script keeps its own per-feed rollback floor, so it moves both ways.
    fn switch(&self, _facts: &Facts, choices: &Choices) -> Vec<Step> {
        let lines = install_lines("bazzite");
        let (install, fetch) = lines
            .split_last()
            .expect("bazzite entry has an install line");
        let mut steps: Vec<Step> = fetch.iter().map(Step::run).collect();
        steps.push(Step::run(format!(
            "{install} --channel {}",
            choices.channel.as_str()
        )));
        steps
    }

    fn uninstall(&self, _facts: &Facts) -> Vec<Step> {
        vec![
            Step::run(UNIT_TEARDOWN),
            Step::run("sudo punktfunk-sysext remove"),
        ]
    }

    /// Conf is written only with `--channel`, so absence cannot tell untouched from stable.
    /// The installed binary breaks the tie.
    fn current_channel(&self, paths: &BasePaths, run: &dyn CommandRunner) -> Option<Channel> {
        if !run.which("punktfunk-host") {
            return None;
        }
        let text = paths
            .read(&paths.etc("punktfunk-sysext.conf"))
            .unwrap_or_default();
        let found = text.lines().find_map(|l| l.strip_prefix("CHANNEL="));
        Some(match found.map(str::trim) {
            Some("canary") => Channel::Canary,
            _ => Channel::Stable,
        })
    }

    fn installed_pf(&self, _run: &dyn CommandRunner) -> Vec<String> {
        vec![]
    }
}

/// SteamOS ships no package: `/usr` is read-only, so the host is compiled on the device inside
/// a Debian distrobox ABI-matched to the running OS, which is what keeps it working across OS
/// updates. `scripts/steamdeck/install.sh` is that build, and it also does the groups, linger,
/// tuning and unit start every other family gets from the phases below — so this is a hand-off
/// like Omarchy's, and it ends the run.
struct Steamos;

impl PkgBackend for Steamos {
    fn base_pkgs(&self) -> Vec<&'static str> {
        vec![]
    }

    fn write_repo(&self, _facts: &Facts, _choices: &Choices) -> Vec<Step> {
        vec![]
    }

    fn install(&self, _facts: &Facts, choices: &Choices) -> Vec<Step> {
        let lines = install_lines("steamos");
        let (build, clone) = lines
            .split_last()
            .expect("steamos entry has an install line");
        // `git clone` into an existing ~/punktfunk fails, and a re-run is documented as safe.
        // Whatever tree is there is the one to build; `update.sh --pull` is what moves it.
        let branch = format!("--branch {}", steamos_branch(choices.channel));
        let mut steps: Vec<Step> = clone
            .iter()
            .map(|line| {
                let line = line.replace("--branch stable", &branch);
                Step::run(format!("[ -d ~/punktfunk/.git ] || {line}"))
            })
            .collect();
        steps.extend(steamos_build(build, choices));
        steps
    }

    /// Move the checkout to the channel's branch, then rebuild through the same hand-off.
    /// `checkout` creates the local branch from `origin/` the first time.
    fn switch(&self, _facts: &Facts, choices: &Choices) -> Vec<Step> {
        let lines = install_lines("steamos");
        let build = lines.last().expect("steamos entry has an install line");
        let branch = steamos_branch(choices.channel);
        let mut steps = vec![
            Step::run("git -C ~/punktfunk fetch origin"),
            Step::run(format!("git -C ~/punktfunk checkout {branch}")),
            Step::run(format!(
                "git -C ~/punktfunk merge --ff-only origin/{branch}"
            )),
        ];
        steps.extend(steamos_build(build, choices));
        steps
    }

    /// Nothing here is a package — the build is spread across the user session and a handful of
    /// root-owned files. The units come off, including the rebuild check no other family has,
    /// and the rest is a documented sequence.
    fn uninstall(&self, _facts: &Facts) -> Vec<Step> {
        vec![
            Step::run(UNIT_TEARDOWN),
            Step::run("systemctl --user disable --now punktfunk-rebuild-check 2>/dev/null || true"),
            Step::note(
                Level::Warn,
                format!("the on-device build has no uninstall script — the build container, the files under your home and the root-owned tuning come off by hand: {DOCS}/uninstall#steamos--steam-deck-host-on-device-build"),
            ),
        ]
    }

    /// The branch `~/punktfunk` follows. `stable` is releases; any other tree pulls `main` or
    /// nothing, so it is canary. No checkout, no channel.
    fn current_channel(&self, paths: &BasePaths, _run: &dyn CommandRunner) -> Option<Channel> {
        let head = paths.read(&paths.home.join("punktfunk/.git/HEAD"))?;
        Some(if head.trim() == "ref: refs/heads/stable" {
            Channel::Stable
        } else {
            Channel::Canary
        })
    }

    fn installed_pf(&self, _run: &dyn CommandRunner) -> Vec<String> {
        vec![]
    }
}

/// The branch a Deck checkout follows per channel. The host reads it back from `.git/HEAD`
/// (`pf_update_check::detect::source_channel`), and `announce.yml` moves `stable`.
fn steamos_branch(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => "stable",
        Channel::Canary => "main",
    }
}

/// The on-device build and what must follow it. The last step ends the run.
fn steamos_build(build: &str, choices: &Choices) -> Vec<Step> {
    // The progress view captures the step's output, so without this line the build looks
    // frozen for its whole run.
    let mut steps = vec![Step::note(
        Level::Warn,
        "the build runs on this device — it asks for your sudo password first, then takes about 20 minutes on a first run (about a minute on a re-run) and prints nothing until it finishes",
    )];
    let mut line = build.to_string();
    if choices.gamestream {
        line.push_str(" --gamestream");
    }
    // The console here reads web.env, which the script writes; host.env's line would go
    // unread. The script keeps an existing web.env line. An address is shell-safe as it is.
    if choices.web_bind.parse::<std::net::IpAddr>().is_ok() {
        line.push_str(&format!(" --web-bind={}", choices.web_bind));
    }
    steps.push(Step::run(line));
    // After the build, never before: that script writes host.env defaults only when the
    // file is absent, and one of them (RADV_PERFTEST=video_encode) turns Vulkan encode on
    // for Van Gogh. The port move below still lands in host.env.
    if choices.clipboard {
        steps.push(Step::set_setting("clipboard", "files"));
    }
    // This step ends the run, so the conflict phase never gets to move the port.
    if choices.move_mgmt_port {
        steps.push(Step::set_env(
            "PUNKTFUNK_MGMT_BIND",
            format!("0.0.0.0:{}", choices.mgmt_port),
        ));
    }
    if let Some(last) = steps.last_mut() {
        last.ends_run = true;
    }
    steps
}

/// User-scope client for families with no native package. An unsupported distro is not
/// a dead end for the client.
struct Flatpak;

impl PkgBackend for Flatpak {
    fn base_pkgs(&self) -> Vec<&'static str> {
        vec![]
    }

    fn write_repo(&self, _facts: &Facts, _choices: &Choices) -> Vec<Step> {
        vec![]
    }

    /// One feed, no channel: `--channel` is a no-op and the line is platforms.json verbatim.
    fn install(&self, _facts: &Facts, _choices: &Choices) -> Vec<Step> {
        install_lines("linux-client")
            .into_iter()
            .map(Step::run)
            .collect()
    }

    fn switch(&self, facts: &Facts, choices: &Choices) -> Vec<Step> {
        self.install(facts, choices)
    }

    fn uninstall(&self, _facts: &Facts) -> Vec<Step> {
        vec![Step::run(format!("flatpak uninstall --user {FLATPAK_APP}"))]
    }

    fn current_channel(&self, _paths: &BasePaths, _run: &dyn CommandRunner) -> Option<Channel> {
        None
    }

    fn installed_pf(&self, _run: &dyn CommandRunner) -> Vec<String> {
        vec![]
    }
}

/// User units go off first: package removal cannot see the enable symlinks in `$HOME`.
const UNIT_TEARDOWN: &str =
    "systemctl --user disable --now punktfunk-host punktfunk-web punktfunk-scripting 2>/dev/null || true";

#[cfg(test)]
mod tests {
    use super::*;

    // Fail the build here, not at install time on a box.
    #[test]
    fn every_host_platform_parses_and_carries_install_lines() {
        for id in ["debian", "arch", "omarchy", "fedora", "bazzite", "steamos"] {
            assert!(!install_lines(id).is_empty(), "{id} has no install lines");
        }
    }

    // The Windows refusal quotes platforms.json's floor line; the check itself uses the
    // build number. Both must name the same build.
    #[test]
    fn the_windows_floor_names_the_build_the_check_uses() {
        assert!(floor("windows").contains(&windows::plan::MIN_HOST_BUILD.to_string()));
    }

    // The split is the one assumption. A reordered line here fails instead of on a box.
    #[test]
    fn shape_of_every_family_split() {
        let (repo, install) = split_at("debian", "sudo apt install");
        assert_eq!(repo.len(), 4, "keyring dir, key, sources line, update");
        assert_eq!(
            install,
            ["sudo apt install punktfunk-host punktfunk-web punktfunk-scripting"]
        );

        let (repo, install) = split_at("arch", "sudo pacman -S");
        assert_eq!(repo.len(), 3, "key add, lsign, pacman.conf section");
        assert_eq!(
            install,
            ["sudo pacman -Syu punktfunk-host punktfunk-web punktfunk-scripting"]
        );

        let (repo, install) = split_at("omarchy", "sudo pacman -S");
        assert_eq!(repo.len(), 3);
        assert_eq!(install[0], "sudo pacman -Sy");
        assert!(install[1].starts_with("sudo pacman -S punktfunk-host"));
        assert_eq!(
            install[2], "punktfunk-omarchy setup",
            "the hand-off is its own phase"
        );

        let (repo, install) = split_at("fedora", "sudo dnf install");
        assert_eq!(
            repo.len(),
            11,
            "the whole heredoc, rejoined into one command"
        );
        assert_eq!(
            repo[0],
            "sudo tee /etc/yum.repos.d/punktfunk.repo >/dev/null <<'REPO'"
        );
        assert_eq!(repo[10], "REPO");
        assert_eq!(
            install,
            ["sudo dnf install punktfunk punktfunk-web punktfunk-scripting"]
        );

        assert_eq!(
            install_lines("bazzite").len(),
            2,
            "fetch the script, run it"
        );

        let lines = install_lines("steamos");
        assert_eq!(lines.len(), 2, "clone the source, run the on-device build");
        assert!(lines[0].starts_with("git clone"), "{:?}", lines[0]);
        assert!(
            lines[0].contains("--branch stable"),
            "canary swaps this for main: {:?}",
            lines[0]
        );
        assert!(
            lines[1].ends_with("scripts/steamdeck/install.sh"),
            "the gamestream flag is appended to this line: {:?}",
            lines[1]
        );
    }

    #[test]
    fn a_deck_checkout_reports_the_branch_it_follows() {
        let root = tempfile::tempdir().unwrap();
        let paths = BasePaths::rooted(root.path());
        let run = crate::seam::FakeRunner::new();
        assert_eq!(
            Steamos.current_channel(&paths, &run),
            None,
            "no checkout yet"
        );
        let git = paths.home.join("punktfunk/.git");
        std::fs::create_dir_all(&git).unwrap();
        for (head, channel) in [
            ("ref: refs/heads/stable\n", Channel::Stable),
            ("ref: refs/heads/main\n", Channel::Canary),
            (
                "169730fe73000f4e71411af65e24c82324af8ccb\n",
                Channel::Canary,
            ),
        ] {
            std::fs::write(git.join("HEAD"), head).unwrap();
            assert_eq!(
                Steamos.current_channel(&paths, &run),
                Some(channel),
                "{head}"
            );
        }
    }
}
