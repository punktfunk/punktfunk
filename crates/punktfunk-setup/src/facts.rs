//! A read-only probe of the box: everything later stages are allowed to know about it.
//!
//! Stage one of `Facts → Choices → Plan → Execute`. All I/O goes through
//! `seam::BasePaths` and `seam::CommandRunner`; nothing here takes a unix-only type,
//! so `--demo` and the tests stay honest on a Mac. See `design/installer-v2.md`.
//!
//! Detection punts (NixOS, an unknown distro) are `Punt`. Version floors are
//! `Floor` data, because `--uninstall` must keep working on a box below them.

use serde::{Deserialize, Serialize};

use crate::seam::{BasePaths, CommandRunner, Env};

pub const DOCS: &str = "https://docs.punktfunk.unom.io/docs";

/// User-scope client id when the family has no native package.
pub const FLATPAK_APP: &str = "io.unom.Punktfunk";

/// The three binaries the installer asks about separately. "Is the host there?" is the
/// wrong question: a box with the host but no console never grows one, however often
/// the installer runs.
pub const BINARIES: [(&str, &str); 3] = [
    ("punktfunk-host", "host"),
    ("punktfunk-web-server", "web-console"),
    ("punktfunk-scripting", "plugin-runner"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Family {
    Apt,
    Dnf,
    Pacman,
    Sysext,
    /// No package at all: SteamOS has a read-only `/usr`, so the host is compiled on the
    /// device and the install is a hand-off to that build.
    Steamos,
    /// No host repo here. The client still installs, user-scope, from the flatpak line —
    /// which is why an unknown distro is a family rather than a dead end.
    Flatpak,
}

impl Family {
    pub fn as_str(self) -> &'static str {
        match self {
            Family::Apt => "apt",
            Family::Dnf => "dnf",
            Family::Pacman => "pacman",
            Family::Sysext => "sysext",
            Family::Steamos => "on-device build",
            Family::Flatpak => "flatpak",
        }
    }

    /// Native `punktfunk-client` in the host repo; otherwise the client is a user-scope flatpak.
    pub fn has_native_client(self) -> bool {
        matches!(self, Family::Apt | Family::Dnf | Family::Pacman)
    }

    /// Every host install puts the console on the box: the package lines name it and the
    /// SteamOS build script builds it (`--no-web` is that script's own opt-out).
    pub fn installs_console(self) -> bool {
        self != Family::Flatpak
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Canary,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Canary => "canary",
        }
    }
}

impl std::str::FromStr for Channel {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "stable" => Ok(Channel::Stable),
            "canary" => Ok(Channel::Canary),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Firewall {
    Firewalld,
    Ufw,
    None,
}

/// NVIDIA half of the verify pass. The install succeeds in every state; `NoDriver` and
/// `ModuleNotLoaded` mean nothing will encode until the user acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Nvidia {
    Absent,
    NoDriver,
    ModuleNotLoaded,
    Ok,
}

/// A distro this installer will not act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Punt {
    NotLinux,
    NoOsRelease,
    NixOs,
    Unsupported(String),
}

impl Punt {
    pub fn message(&self) -> String {
        match self {
            Punt::NotLinux => {
                format!("this installer is for Linux hosts — Windows: {DOCS}/windows-host")
            }
            Punt::NoOsRelease => {
                format!("no /etc/os-release — can't tell which distro this is: {DOCS}/install")
            }
            Punt::NixOs => {
                format!("NixOS: add the flake input and enable the module instead — {DOCS}/nixos")
            }
            Punt::Unsupported(pretty) => {
                format!("no package repo for '{pretty}' yet — {DOCS}/developers/build-from-source")
            }
        }
    }
}

/// A version floor the package itself cannot express. Checked only before an install, so a
/// box below the floor can still be uninstalled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Floor {
    /// Fatal: below the glibc floor, or a Fedora with no RPM group.
    Die(String),
    /// Warn, then ask "Continue anyway?" with default no — so `--yes` aborts.
    Confirm(String),
}

/// `/etc/os-release`, parsed the way `.`-sourcing it would read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsRelease {
    pub id: String,
    pub id_like: String,
    pub version_id: String,
    pub pretty: String,
}

impl OsRelease {
    pub fn parse(text: &str) -> Self {
        let mut os = OsRelease::default();
        for line in text.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            if key.starts_with('#') {
                continue;
            }
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(value)
                .to_string();
            match key.trim() {
                "ID" => os.id = value,
                "ID_LIKE" => os.id_like = value,
                "VERSION_ID" => os.version_id = value,
                "PRETTY_NAME" => os.pretty = value,
                _ => {}
            }
        }
        if os.pretty.is_empty() {
            os.pretty = os.id.clone();
        }
        os
    }

    pub fn like(&self, needle: &str) -> bool {
        self.id == needle || self.id_like.split_whitespace().any(|w| w == needle)
    }

    /// `VERSION_ID` before the first dot, as a number. `0` when it is not one.
    pub fn major(&self) -> u32 {
        self.version_id
            .split('.')
            .next()
            .unwrap_or("")
            .parse()
            .unwrap_or(0)
    }
}

/// Everything the plan is allowed to know about this machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub os: OsRelease,
    pub family: Family,
    /// Omarchy is a pacman flavour, not a family: same repo, same packages, different
    /// transaction shape and a hand-off afterwards.
    pub omarchy: bool,
    pub docs_page: String,
    /// Why a host install cannot happen here. A client install ignores it.
    pub host_punt: Option<String>,
    pub rpm_group: Option<String>,
    pub floor: Option<Floor>,
    /// Flatpak client already on the box; uninstall has a leg to sweep.
    pub has_flatpak_client: bool,
    pub couch_box: bool,
    pub graphical_seat: bool,
    /// A session file under `/usr/share/{wayland-sessions,xsessions}`: a desktop can be
    /// logged into later. Without one, only gamescope can stand a session up.
    pub desktop_sessions: bool,
    pub sunshine_active: bool,
    pub current_channel: Option<Channel>,
    pub installed_pf: Vec<String>,
    /// Labels from `BINARIES` that are not on `PATH`.
    pub missing: Vec<String>,
    pub host_version: Option<String>,
    pub has_web_server: bool,
    pub has_omarchy_bin: bool,
    pub has_ujust: bool,
    pub in_input_group: bool,
    pub in_punktfunk_group: bool,
    pub has_input_group: bool,
    pub nvidia: Nvidia,
    pub firewall: Firewall,
    pub systemd_pid1: bool,
    pub user_manager: bool,
    pub web_unit_present: bool,
    /// A console login password already lives in the user's config dir; a re-run must not
    /// ask about one it would only overwrite.
    pub web_password_present: bool,
    /// `PUNKTFUNK_UI_BIND` as host.env already names it. A re-run defaults to this, so pressing
    /// Enter through the question can never move a console the operator already placed.
    #[serde(default)]
    pub web_bind: Option<String>,
    pub scripting_unit_disabled: bool,
    pub ip: Option<String>,
    pub user: String,
}

impl Facts {
    pub fn probe(paths: &BasePaths, run: &dyn CommandRunner, env: &Env) -> Result<Facts, Punt> {
        let text = paths.read(&paths.os_release).ok_or(Punt::NoOsRelease)?;
        let os = OsRelease::parse(&text);
        let (family, page, host_punt) = detect_family(&os, run)?;
        let omarchy = family == Family::Pacman && os.id == "omarchy";

        let (rpm_group, floor) = floors(&os, family);
        let user = env
            .get("USER")
            .map(str::to_string)
            .or_else(|| run.first_line("id", &["-un"]))
            .unwrap_or_default();
        let groups = run
            .first_line("id", &["-nG", &user])
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();

        let missing = BINARIES
            .iter()
            .filter(|(bin, _)| !run.which(bin))
            .map(|(_, label)| (*label).to_string())
            .collect();

        Ok(Facts {
            family,
            omarchy,
            docs_page: format!("{DOCS}/{page}"),
            host_punt: host_punt.map(|p| p.message()),
            rpm_group,
            floor,
            has_flatpak_client: run
                .probe("flatpak", &["info", FLATPAK_APP])
                .is_some_and(|o| o.ok()),
            couch_box: os.like("bazzite") || os.like("nobara"),
            graphical_seat: graphical_seat(env),
            desktop_sessions: desktop_sessions(paths),
            sunshine_active: sunshine_active(run),
            current_channel: crate::platform::backend(family).current_channel(paths, run),
            installed_pf: crate::platform::backend(family).installed_pf(run),
            missing,
            host_version: run.first_line("punktfunk-host", &["--version"]),
            has_web_server: run.which("punktfunk-web-server"),
            has_omarchy_bin: run.which("punktfunk-omarchy"),
            has_ujust: run.which("ujust"),
            in_input_group: groups.iter().any(|g| g == "input"),
            in_punktfunk_group: groups.iter().any(|g| g == "punktfunk"),
            has_input_group: run
                .probe("getent", &["group", "input"])
                .is_some_and(|o| o.ok()),
            nvidia: nvidia(paths, run),
            firewall: firewall(paths, run),
            systemd_pid1: paths.run.join("systemd/system").is_dir(),
            user_manager: run
                .probe("systemctl", &["--user", "show-environment"])
                .is_some_and(|o| o.ok()),
            web_unit_present: unit_files(run, "punktfunk-web.service")
                .lines()
                .any(|l| l.starts_with("punktfunk-web.service")),
            web_password_present: std::fs::metadata(paths.config.join("punktfunk/web-password"))
                .is_ok_and(|m| m.len() > 0),
            web_bind: env_line(&paths.host_env(), "PUNKTFUNK_UI_BIND"),
            scripting_unit_disabled: unit_files(run, "punktfunk-scripting.service")
                .contains("disabled"),
            ip: local_ip(run),
            user,
            os,
        })
    }

    pub fn fully_installed(&self) -> bool {
        self.missing.is_empty()
    }
}

/// The sh script's detection ladder, in the same order — `rpm-ostree`/`bootc` before the
/// debian and fedora tests, because Bazzite answers `like fedora` too.
/// The family, its docs page, and the punt a **host** install would hit here.
///
/// NixOS and SteamOS refuse outright — they own installers for both halves. An unknown distro
/// is a dead end only for the host: the client still has a user-scope flatpak (§5).
impl Family {
    /// What to install for `certutil`, named in the warning when it is missing.
    pub fn certutil_package(self) -> &'static str {
        match self {
            Family::Apt => "sudo apt install libnss3-tools",
            Family::Dnf | Family::Sysext => "sudo dnf install nss-tools",
            Family::Pacman | Family::Steamos => "sudo pacman -S nss",
            Family::Flatpak => "install nss tools",
        }
    }
}

type Detected = (Family, &'static str, Option<Punt>);

fn detect_family(os: &OsRelease, run: &dyn CommandRunner) -> Result<Detected, Punt> {
    if os.id == "nixos" {
        return Err(Punt::NixOs);
    }
    // SteamOS before the arch test it would otherwise match: `/usr` is read-only, so the host
    // is built on the device rather than installed. Derivatives that keep `ID_LIKE=steamos`
    // (HoloISO, ChimeraOS) take the same path.
    if os.id == "steamos" || os.like("steamos") {
        return Ok((Family::Steamos, "steamos-host", None));
    }
    if run.which("rpm-ostree") || run.which("bootc") || os.id == "bazzite" {
        return Ok((Family::Sysext, "bazzite", None));
    }
    if os.like("debian") || os.like("ubuntu") {
        let page = if os.id == "ubuntu" {
            "ubuntu"
        } else {
            "debian"
        };
        return Ok((Family::Apt, page, None));
    }
    if os.like("fedora") {
        return Ok((Family::Dnf, "fedora", None));
    }
    if os.like("arch") {
        let page = if os.id == "omarchy" {
            "omarchy"
        } else {
            "arch"
        };
        return Ok((Family::Pacman, page, None));
    }
    Ok((
        Family::Flatpak,
        "install",
        Some(Punt::Unsupported(os.pretty.clone())),
    ))
}

/// Fedora RPM group lives here because it shares the version-floor check.
pub fn floors(os: &OsRelease, family: Family) -> (Option<String>, Option<Floor>) {
    let floor = match os.id.as_str() {
        "debian" if os.major() < 13 => Some(Floor::Die(format!(
            "Debian {} is below the glibc floor — {}, or build from source: {DOCS}/developers/build-from-source",
            os.version_id,
            crate::platform::floor("debian")
        ))),
        "ubuntu" if (20..=25).contains(&os.major()) => Some(Floor::Confirm(format!(
            "Ubuntu {} installs the package but can't host — its desktop is too old to create a virtual display ({DOCS}/requirements#the-floor-for-a-working-host). Runs on {}.",
            os.version_id,
            crate::platform::floor("debian")
        ))),
        "linuxmint" if (20..=22).contains(&os.major()) => Some(Floor::Confirm(format!(
            "Linux Mint {} (Ubuntu 24.04 base) installs the package but can't host — {DOCS}/requirements#cinnamon-linux-mint-and-lmde. LMDE 7 and Mint 23 can.",
            os.version_id
        ))),
        _ => None,
    };
    if family != Family::Dnf {
        return (None, floor);
    }
    match os.major() {
        44 => (Some("fedora-44".into()), floor),
        // Fedora 43 ships the same RPM as Bazzite, so the group name is `bazzite`.
        43 => (Some("bazzite".into()), floor),
        _ => (
            None,
            Some(Floor::Die(format!(
                "no RPM group for Fedora {} yet — {DOCS}/developers/build-from-source",
                os.version_id
            ))),
        ),
    }
}

fn desktop_sessions(paths: &BasePaths) -> bool {
    ["usr/share/wayland-sessions", "usr/share/xsessions"]
        .iter()
        .any(|d| std::fs::read_dir(paths.etc_root.join(d)).is_ok_and(|mut it| it.next().is_some()))
}

fn graphical_seat(env: &Env) -> bool {
    env.get("DISPLAY").is_some()
        || env.get("WAYLAND_DISPLAY").is_some()
        || matches!(env.get("XDG_SESSION_TYPE"), Some("x11" | "wayland"))
}

/// Same split as `punktfunk-host detect-conflicts`: exit 1 is the only "yes". Any other
/// code is a host too old to know the subcommand, a half-install, or a crash — fall
/// through to the unit probe rather than opening the GameStream surface on a guess.
fn sunshine_active(run: &dyn CommandRunner) -> bool {
    if run.which("punktfunk-host")
        && let Some(out) = run.probe("punktfunk-host", &["detect-conflicts"])
    {
        match out.code {
            0 => return false,
            1 => return true,
            _ => {}
        }
    }
    for unit in ["sunshine.service", "apollo.service", "vibeshine.service"] {
        for args in [
            vec!["is-active", "--quiet", unit],
            vec!["is-enabled", "--quiet", unit],
            vec!["--user", "is-active", "--quiet", unit],
            vec!["--user", "is-enabled", "--quiet", unit],
        ] {
            if run.probe("systemctl", &args).is_some_and(|o| o.ok()) {
                return true;
            }
        }
    }
    false
}

fn nvidia(paths: &BasePaths, run: &dyn CommandRunner) -> Nvidia {
    let devices = paths.sys.join("bus/pci/devices");
    let Ok(entries) = std::fs::read_dir(&devices) else {
        return Nvidia::Absent;
    };
    let present = entries.filter_map(Result::ok).any(|e| {
        std::fs::read_to_string(e.path().join("vendor")).is_ok_and(|v| v.trim() == "0x10de")
    });
    if !present {
        return Nvidia::Absent;
    }
    if !run.which("nvidia-smi") {
        return Nvidia::NoDriver;
    }
    if run.probe("nvidia-smi", &[]).is_some_and(|o| o.ok()) {
        Nvidia::Ok
    } else {
        Nvidia::ModuleNotLoaded
    }
}

/// Packages never open ports; they install firewalld services / ufw profiles by name. Only
/// an active firewall is worth writing rules into.
fn firewall(paths: &BasePaths, run: &dyn CommandRunner) -> Firewall {
    if run.which("firewall-cmd")
        && run
            .probe("systemctl", &["is-active", "--quiet", "firewalld"])
            .is_some_and(|o| o.ok())
    {
        return Firewall::Firewalld;
    }
    let enabled = paths
        .read(&paths.etc("ufw/ufw.conf"))
        .is_some_and(|t| t.lines().any(|l| l.trim_end() == "ENABLED=yes"));
    if run.which("ufw") && enabled {
        return Firewall::Ufw;
    }
    Firewall::None
}

/// One `KEY=VALUE` out of an env file. Last line wins, comments and blanks do not count — the
/// same reading systemd gives the file, so what we report is what the unit will get.
fn env_line(path: &std::path::Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines()
        .filter_map(|l| l.trim().strip_prefix(&format!("{key}=")))
        .map(|v| v.trim().trim_matches('"').to_string())
        .rfind(|v| !v.is_empty())
}

fn unit_files(run: &dyn CommandRunner, unit: &str) -> String {
    run.probe("systemctl", &["--user", "list-unit-files", unit])
        .filter(|o| o.ok())
        .map(|o| o.stdout)
        .unwrap_or_default()
}

fn local_ip(run: &dyn CommandRunner) -> Option<String> {
    if let Some(line) = run.first_line("hostname", &["-I"])
        && let Some(first) = line.split_whitespace().next()
    {
        return Some(first.to_string());
    }
    let out = run.first_line("ip", &["-4", "route", "get", "1.1.1.1"])?;
    let mut it = out.split_whitespace();
    while let Some(word) = it.next() {
        if word == "src" {
            return it.next().map(str::to_string);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    // host.env is hand-edited, so the reading has to be systemd's: last assignment wins, a
    // comment is not one, and a blank value means unset (the same rule `PUNKTFUNK_MGMT_BIND`
    // takes in pf-host-config). Getting it wrong here moves a console on the next re-run.
    #[test]
    fn env_line_reads_the_file_the_way_the_unit_will() {
        let dir = std::env::temp_dir().join(format!("pf-env-line-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("host.env");
        let write = |body: &str| std::fs::write(&path, body).unwrap();

        write("PUNKTFUNK_UI_BIND=0.0.0.0\n");
        assert_eq!(
            super::env_line(&path, "PUNKTFUNK_UI_BIND").as_deref(),
            Some("0.0.0.0")
        );
        write("  PUNKTFUNK_UI_BIND=\"100.64.0.3\"\n");
        assert_eq!(
            super::env_line(&path, "PUNKTFUNK_UI_BIND").as_deref(),
            Some("100.64.0.3")
        );
        write("PUNKTFUNK_UI_BIND=0.0.0.0\nPUNKTFUNK_UI_BIND=127.0.0.1\n");
        assert_eq!(
            super::env_line(&path, "PUNKTFUNK_UI_BIND").as_deref(),
            Some("127.0.0.1")
        );
        write("# PUNKTFUNK_UI_BIND=0.0.0.0\n");
        assert_eq!(super::env_line(&path, "PUNKTFUNK_UI_BIND"), None);
        write("PUNKTFUNK_UI_BIND=\n");
        assert_eq!(super::env_line(&path, "PUNKTFUNK_UI_BIND"), None);
        assert_eq!(
            super::env_line(&dir.join("nope.env"), "PUNKTFUNK_UI_BIND"),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;
    use crate::seam::FakeRunner;

    fn os(id: &str, id_like: &str, version: &str) -> OsRelease {
        OsRelease {
            id: id.into(),
            id_like: id_like.into(),
            version_id: version.into(),
            pretty: id.into(),
        }
    }

    #[test]
    fn os_release_parses_quotes_and_falls_back_to_id_for_pretty() {
        let os = OsRelease::parse("ID=arch\nID_LIKE=\"arch\"\nVERSION_ID='40'\n");
        assert_eq!(os.id, "arch");
        assert_eq!(os.version_id, "40");
        assert_eq!(os.pretty, "arch");
        assert!(os.like("arch"));
        assert!(!os.like("debian"));
    }

    #[test]
    fn cachyos_is_detected_through_id_like() {
        let r = FakeRunner::new();
        let (family, page, _) = detect_family(&os("cachyos", "arch", ""), &r).unwrap();
        assert_eq!(family, Family::Pacman);
        assert_eq!(page, "arch");
    }

    // rpm-ostree and bootc are checked before the fedora test, or Bazzite lands on dnf.
    #[test]
    fn an_ostree_box_is_sysext_even_though_it_looks_like_fedora() {
        let r = FakeRunner::new().with_path("rpm-ostree");
        let (family, page, _) = detect_family(&os("bluefin", "fedora", "43"), &r).unwrap();
        assert_eq!(family, Family::Sysext);
        assert_eq!(page, "bazzite");
    }

    #[test]
    fn omarchy_is_a_pacman_flavour_with_its_own_docs_page() {
        let r = FakeRunner::new();
        let (family, page, _) = detect_family(&os("omarchy", "arch", ""), &r).unwrap();
        assert_eq!(family, Family::Pacman);
        assert_eq!(page, "omarchy");
    }

    #[test]
    fn nixos_punts_before_anything_else() {
        let r = FakeRunner::new();
        assert_eq!(detect_family(&os("nixos", "", ""), &r), Err(Punt::NixOs));
    }

    // SteamOS says `ID_LIKE=arch`, so it has to be tested before the arch family or a Deck
    // gets pacman commands its read-only /usr cannot run. A derivative that keeps
    // `ID_LIKE=steamos` lands on the same on-device build.
    #[test]
    fn steamos_and_its_derivatives_beat_the_arch_test() {
        let r = FakeRunner::new();
        let (family, page, punt) = detect_family(&os("steamos", "arch", ""), &r).unwrap();
        assert_eq!(family, Family::Steamos);
        assert_eq!(page, "steamos-host");
        assert!(punt.is_none());

        let (family, _, _) = detect_family(&os("holoiso", "steamos arch", ""), &r).unwrap();
        assert_eq!(family, Family::Steamos);
    }

    // Game Mode / HTPC images only. rpm-ostree, bootc, and ujust are not tells: Silverblue
    // and Bluefin ship all three and are workstations.
    #[test]
    fn couch_box_is_bazzite_and_nobara_only() {
        assert!(os("bazzite", "fedora", "43").like("bazzite"));
        assert!(!os("silverblue", "fedora", "43").like("bazzite"));
        assert!(!os("bluefin", "fedora", "43").like("nobara"));
    }

    #[test]
    fn debian_below_the_glibc_floor_dies_and_ubuntu_only_asks() {
        let (_, floor) = floors(&os("debian", "", "12"), Family::Apt);
        assert!(matches!(floor, Some(Floor::Die(_))));
        let (_, floor) = floors(&os("debian", "", "13"), Family::Apt);
        assert!(floor.is_none());
        let (_, floor) = floors(&os("ubuntu", "debian", "24.04"), Family::Apt);
        assert!(matches!(floor, Some(Floor::Confirm(_))));
        let (_, floor) = floors(&os("ubuntu", "debian", "26.04"), Family::Apt);
        assert!(floor.is_none());
    }

    #[test]
    fn the_fedora_group_map_covers_44_and_43_and_dies_otherwise() {
        assert_eq!(
            floors(&os("fedora", "", "44"), Family::Dnf).0.unwrap(),
            "fedora-44"
        );
        assert_eq!(
            floors(&os("fedora", "", "43"), Family::Dnf).0.unwrap(),
            "bazzite"
        );
        let (group, floor) = floors(&os("fedora", "", "45"), Family::Dnf);
        assert!(group.is_none());
        assert!(matches!(floor, Some(Floor::Die(_))));
    }

    #[test]
    fn detect_conflicts_answers_only_with_exit_one() {
        let yes = FakeRunner::new().answer("punktfunk-host detect-conflicts", 1, "");
        assert!(sunshine_active(&yes));
        let no = FakeRunner::new().answer("punktfunk-host detect-conflicts", 0, "");
        assert!(!sunshine_active(&no));
    }

    // A host too old to know the subcommand exits 127/2 — that is not an answer. The unit probe decides.
    #[test]
    fn an_unknown_detect_conflicts_code_falls_through_to_the_unit_probe() {
        let old = FakeRunner::new()
            .answer("punktfunk-host detect-conflicts", 2, "")
            .answer("systemctl is-active --quiet sunshine.service", 0, "");
        assert!(sunshine_active(&old));
        let quiet = FakeRunner::new()
            .answer("punktfunk-host detect-conflicts", 2, "")
            .with_path("systemctl");
        assert!(!sunshine_active(&quiet));
    }

    #[test]
    fn installed_pf_keeps_only_packages_apt_reports_as_installed() {
        let r = FakeRunner::new().answer(
            "dpkg-query -W -f=${Package} ${db:Status-Status}\n punktfunk*",
            0,
            "punktfunk-host installed\npunktfunk-web config-files\npunktfunk-client installed\n",
        );
        let found = crate::platform::backend(Family::Apt).installed_pf(&r);
        assert_eq!(found, ["punktfunk-host", "punktfunk-client"]);
    }
}
