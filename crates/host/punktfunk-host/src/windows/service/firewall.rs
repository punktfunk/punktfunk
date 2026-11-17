//! The inbound firewall rules `service install` adds, and the startup Public-network warning.

use super::*;

/// `netsh` `profile=` for inbound rules. Default Domain+Private; `allow_public` is all profiles.
/// Shared with the web-console rule in `install.rs`.
pub(crate) fn firewall_profile_arg(allow_public: bool) -> &'static str {
    if allow_public {
        "profile=any"
    } else {
        "profile=domain,private"
    }
}

/// Public-network firewall scope. Tri-state, like `--gamestream=on|off`:
/// - `--allow-public-network` or `=on` → opt-in (bare form kept for existing scripts)
/// - `=off` → opt-out
/// - absent → the previous install's marker (so a silent upgrade does not reset the checkbox)
///
/// A typo (`=of`) must not fall through to the marker: the marker may be `true`, and a mistyped
/// opt-out would leave Public open. No marker on a first install → Domain+Private.
pub(crate) fn allow_public_network(args: &[String]) -> Result<bool> {
    for a in args {
        if let Some(v) = a.strip_prefix("--allow-public-network") {
            return match v {
                "" | "=on" => Ok(true),
                "=off" => Ok(false),
                _ => bail!(
                    "--allow-public-network must be 'on' or 'off' (got '{}')",
                    v.trim_start_matches('=')
                ),
            };
        }
    }
    Ok(fw_public_marker().exists())
}

/// `netsh advfirewall firewall add rule` for one inbound allow.
///
/// A port-only `dir=in action=allow` admits any process that binds first (high ports need no
/// elevation) and suppresses the Windows prompt. Name `program` so the ports are ours only.
/// Keep `ports` too: both is tighter. `program: None` is the old any-program rule — a looser
/// rule still streams; no rule is a black screen.
pub(crate) fn fw_add_rule_args(
    name: &str,
    proto: &str,
    ports: Option<&str>,
    program: Option<&std::path::Path>,
    profile: &str,
) -> Vec<String> {
    let mut args: Vec<String> = ["advfirewall", "firewall", "add", "rule"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.push(format!("name={name}"));
    args.push("dir=in".into());
    args.push("action=allow".into());
    args.push(format!("protocol={proto}"));
    if let Some(p) = ports {
        args.push(format!("localport={p}"));
    }
    if let Some(exe) = program {
        args.push(format!("program={}", exe.display()));
    }
    args.push(profile.to_string());
    args
}

pub(crate) fn run_netsh(args: &[String]) -> bool {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_quiet("netsh", &borrowed)
}

/// The UDP rule's ports: native, each seat's native port (the box redirects a client to its
/// seat there), mDNS 5353.
fn udp_ports(native_port: u16) -> String {
    let seats = pf_seats::model::DEFAULT_NATIVE_PORTS.map(|p| p.to_string());
    format!("{native_port},{},5353", seats.join(","))
}

/// Inbound streaming + mgmt rules, and the optional planes' ([`sync_plane_rules`]). Best-effort;
/// never fails the install. Scoped by [`firewall_profile_arg`] and this executable
/// ([`fw_add_rule_args`]). The mgmt port is deliberate: `serve` binds mgmt/library to all
/// interfaces; off loopback, `require_auth` (`mgmt/auth.rs`) refuses the admin API and admits a
/// paired client only to `cert_may_access`, so opening it adds no admin surface.
pub(super) fn add_firewall_rules(allow_public: bool) {
    let profile = firewall_profile_arg(allow_public);
    // `service install` remove-then-adds on every upgrade, so a moved install cannot leave a
    // stale path.
    let exe = match std::env::current_exe() {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!(
                "warning: could not resolve the host executable path ({e}) — the rules below stay \
                 open to any program on those ports"
            );
            None
        }
    };
    // Mgmt/library (LAN: paired-cert allowlist) on the port host.env gives `serve`; `--mgmt-bind`
    // lands there first.
    let host_env = std::fs::read_to_string(host_env_path()).unwrap_or_default();
    let listen = pf_host_config::listen::ListenPlan::from_host_env(&host_env);
    let tcp = listen.mgmt.port().to_string();
    let udp = udp_ports(listen.native_port);
    let rules = [("TCP", "TCP", tcp.as_str()), ("UDP", "UDP", udp.as_str())];
    for (suffix, proto, ports) in rules {
        let name = format!("Punktfunk {suffix}");
        let ok = run_netsh(&fw_add_rule_args(
            &name,
            proto,
            Some(ports),
            exe.as_deref(),
            profile,
        ));
        if ok {
            let scope = match &exe {
                Some(p) => format!(" for {}", p.display()),
                None => String::new(),
            };
            println!("Firewall rule added: {name} ({ports}{scope}) [{profile}]");
        } else {
            eprintln!("warning: firewall rule '{name}' not added (add it manually if needed)");
        }
    }
    sync_plane_rules(exe.as_deref());
    // Print only when scoping actually happened: with no exe path the rules are still wide open.
    if exe.is_some() {
        println!(
            "Note: these rules are scoped to the punktfunk host executable, so they no longer open \
             those ports to every program on this machine. Another mDNS/GameStream application \
             that relied on punktfunk's rules to be reachable now needs a rule of its own."
        );
    }
    if !allow_public {
        println!(
            "Note: streaming ports are open on Private/Domain networks only. On a network Windows \
             classifies as Public, clients won't connect — set that network to Private, or reinstall \
             with the 'Allow connections on Public networks' option."
        );
    }
}

/// The any-UDP rule a host with a second video port needed. Video rides the native port, so
/// [`remove_firewall_rules`] only deletes it.
const FW_DATA_PLANE_RULE: &str = "Punktfunk UDP (data plane)";

pub(super) fn remove_firewall_rules() {
    let _ = run_quiet(
        "netsh",
        &[
            "advfirewall",
            "firewall",
            "delete",
            "rule",
            &format!("name={FW_DATA_PLANE_RULE}"),
        ],
    );
    for name in [
        "Punktfunk TCP",
        "Punktfunk UDP",
        FW_BROWSER_RULE,
        FW_GAMESTREAM_RULE,
    ] {
        // netsh matches rule names case-insensitively, so this also reaps the old lowercase names.
        let _ = run_quiet(
            "netsh",
            &[
                "advfirewall",
                "firewall",
                "delete",
                "rule",
                &format!("name={name}"),
            ],
        );
    }
}

/// The browser plane's UDP rule.
const FW_BROWSER_RULE: &str = "Punktfunk Browser";

/// GameStream's TCP and UDP rules, under one name.
const FW_GAMESTREAM_RULE: &str = "Punktfunk GameStream";

/// The rules the optional planes need while on: `(name, protocol, ports)`. The browser rule opens
/// each seat's browser port too, beside its native port: a seat's plane follows the box's switch.
fn plane_rules(
    browser: Option<u16>,
    gamestream: bool,
) -> Vec<(&'static str, &'static str, String)> {
    let mut rules = Vec::new();
    if let Some(port) = browser {
        let seats = pf_seats::model::DEFAULT_BROWSER_PORTS.map(|p| p.to_string());
        rules.push((
            FW_BROWSER_RULE,
            "UDP",
            format!("{port},{}", seats.join(",")),
        ));
    }
    if gamestream {
        rules.push((FW_GAMESTREAM_RULE, "TCP", "47984,47989,48010".into()));
        rules.push((FW_GAMESTREAM_RULE, "UDP", "47998-48010".into()));
    }
    rules
}

/// Make the browser plane's and GameStream's rules match what the next host serves: present
/// while on, scoped like the others, absent while off. The console's toggles apply at a host
/// restart, so the service runs this before each host start. Best-effort.
pub(super) fn sync_plane_rules(exe: Option<&std::path::Path>) {
    for name in [FW_BROWSER_RULE, FW_GAMESTREAM_RULE] {
        let rule = format!("name={name}");
        let _ = run_quiet(
            "netsh",
            &["advfirewall", "firewall", "delete", "rule", &rule],
        );
    }
    let host_env = std::fs::read_to_string(host_env_path()).unwrap_or_default();
    let config = pf_host_config::resolve_now().config;
    let browser =
        pf_host_config::listen::browser_port_from_host_env(&host_env, config.webtransport);
    let gamestream = pf_host_config::listen::gamestream_from_host_env(&host_env, config.gamestream);
    let profile = firewall_profile_arg(fw_public_marker().exists());
    for (name, proto, ports) in plane_rules(browser, gamestream) {
        if !run_netsh(&fw_add_rule_args(name, proto, Some(&ports), exe, profile)) {
            tracing::warn!(rule = name, ports, "firewall rule not added");
        }
    }
}

/// Presence means `--allow-public-network` was chosen; suppresses the startup Public warning.
pub(super) fn fw_public_marker() -> std::path::PathBuf {
    pf_paths::config_dir().join("fw-allow-public")
}

pub(super) fn set_fw_public_marker(allow_public: bool) {
    let path = fw_public_marker();
    if allow_public {
        let _ = std::fs::write(&path, b"1\n");
    } else {
        let _ = std::fs::remove_file(&path);
    }
}

/// Any active connection classified Public? `None` if `Get-NetConnectionProfile` cannot answer.
pub(super) fn active_network_is_public() -> Option<bool> {
    // Full System32 path: CreateProcess searches the launching EXE's directory first, so a
    // planted `powershell.exe` next to the host would run as SYSTEM.
    let ps = crate::install::sys32(pf_paths::POWERSHELL);
    let out = std::process::Command::new(&ps)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-NetConnectionProfile).NetworkCategory",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Some(s.lines().any(|l| l.trim().eq_ignore_ascii_case("Public")))
}

/// Warn when the network is Public and the operator did not opt in. Own thread: must not delay
/// the host.
pub(super) fn warn_if_public_network() {
    if fw_public_marker().exists() {
        return;
    }
    if active_network_is_public() == Some(true) {
        tracing::warn!(
            "this machine's current network is classified Public (an untrusted-network profile), so \
             punktfunk's streaming ports are firewalled off here and clients on this network can't \
             reach the host. Fix: set the network to Private (Windows Settings > Network > \
             properties) — or, only for a network you trust, reinstall with the 'Allow connections \
             on Public networks' option."
        );
    }
}

#[cfg(test)]
mod firewall_tests {
    use super::*;
    use pf_host_config::listen::ListenPlan;
    use std::path::Path;

    /// Fixed-port rules carry both `program=` and `localport=`. Dropping `program=` still streams;
    /// any unprivileged process can then bind those ports without a Windows prompt.
    #[test]
    fn fixed_port_rules_are_scoped_to_the_program_and_the_ports() {
        let exe = Path::new(r"C:\Program Files\Punktfunk\punktfunk-host.exe");
        let args = fw_add_rule_args(
            "Punktfunk UDP",
            "UDP",
            Some("47998-48010,9777,5353"),
            Some(exe),
            "profile=domain,private",
        );
        assert!(args.contains(&format!("program={}", exe.display())));
        assert!(args.contains(&"localport=47998-48010,9777,5353".to_string()));
        assert!(args.contains(&"dir=in".to_string()));
        assert!(args.contains(&"action=allow".to_string()));
        assert!(args.contains(&"profile=domain,private".to_string()));
        assert_eq!(&args[..4], &["advfirewall", "firewall", "add", "rule"]);
    }

    /// A client redirected to its seat lands on the seat's own port.
    #[test]
    fn the_udp_rule_opens_every_seat_port() {
        let rule = |host_env: &str| udp_ports(ListenPlan::from_host_env(host_env).native_port);
        assert_eq!(rule(""), "9777,9779,9780,9781,9782,5353");
        assert_eq!(
            rule("PUNKTFUNK_NATIVE_PORT=9800\n"),
            "9800,9779,9780,9781,9782,5353"
        );
    }

    /// GameStream's ports open only while GameStream is on.
    #[test]
    fn gamestream_ports_open_only_while_it_is_on() {
        let names = |gs| {
            plane_rules(None, gs)
                .into_iter()
                .map(|r| r.0)
                .collect::<Vec<_>>()
        };
        assert!(names(false).is_empty());
        assert_eq!(names(true), [FW_GAMESTREAM_RULE, FW_GAMESTREAM_RULE]);
        assert_eq!(
            plane_rules(Some(9778), false)[0].2,
            "9778,9783,9784,9785,9786"
        );
    }

    /// Missing executable → port-only rule, not no rule. A looser rule still streams; none is black.
    #[test]
    fn a_missing_program_falls_back_to_the_port_only_rule() {
        let args = fw_add_rule_args("Punktfunk TCP", "TCP", Some("47990"), None, "profile=any");
        assert!(!args.iter().any(|a| a.starts_with("program=")));
        assert!(args.contains(&"localport=47990".to_string()));
    }

    /// `--mgmt-bind` writes the line the mgmt rule reads back, whatever host.env already holds.
    #[test]
    fn the_mgmt_bind_flag_lands_where_the_rule_reads() {
        let mgmt_port = |host_env: &str| ListenPlan::from_host_env(host_env).mgmt.port();
        let set =
            |text: &str| mgmt_port(&with_env_line(text, "PUNKTFUNK_MGMT_BIND", "0.0.0.0:47991"));
        assert_eq!(set(""), 47991);
        assert_eq!(
            set("RUST_LOG=info\n# PUNKTFUNK_MGMT_BIND=0.0.0.0:1\n"),
            47991
        );
        assert_eq!(
            set("PUNKTFUNK_MGMT_BIND=0.0.0.0:1\nPUNKTFUNK_MGMT_BIND=0.0.0.0:2\n"),
            47991
        );
        assert_eq!(
            with_env_line("A=1\n# K=0\nK=2\n", "K", "3"),
            "A=1\n# K=0\nK=3\n"
        );
    }
}
