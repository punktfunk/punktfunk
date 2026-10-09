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

/// A setting `serve` reads: `flag` in host.env's `PUNKTFUNK_HOST_CMD`, else the last `key=` line,
/// the same order `parse_serve_args` applies.
pub(super) fn serve_setting<'a>(host_env: &'a str, flag: &str, key: &str) -> Option<&'a str> {
    let last = |name: &str| {
        host_env
            .lines()
            .rev()
            .filter_map(|l| l.trim().split_once('='))
            .find(|(k, _)| k.trim() == name)
            .map(|(_, v)| v.trim().trim_matches('"'))
    };
    let mut cmd = last("PUNKTFUNK_HOST_CMD").unwrap_or("").split_whitespace();
    cmd.find(|w| *w == flag)
        .and_then(|_| cmd.next())
        .or_else(|| last(key))
}

/// The mgmt port `serve` binds, else 47990. A blank or bad value keeps 47990; the host treats
/// blank as unset and refuses to start on a bad one.
pub(super) fn mgmt_port(host_env: &str) -> u16 {
    serve_setting(host_env, "--mgmt-bind", "PUNKTFUNK_MGMT_BIND")
        .and_then(|v| v.parse::<std::net::SocketAddr>().ok())
        .map_or(crate::mgmt::DEFAULT_PORT, |a| a.port())
}

/// The native QUIC port `serve` binds, else 9777, read the way [`mgmt_port`] reads its own.
pub(super) fn native_port(host_env: &str) -> u16 {
    serve_setting(host_env, "--native-port", "PUNKTFUNK_NATIVE_PORT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(9777)
}

/// Inbound streaming + mgmt rules. Best-effort; never fails the install. Scoped by
/// [`firewall_profile_arg`] and this executable ([`fw_add_rule_args`]). The mgmt port is
/// deliberate: `serve` binds mgmt/library to all interfaces; off-loopback
/// `mgmt::require_auth` is read-only to a paired client cert, so opening it adds no admin surface.
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
    // Mgmt/library (LAN read-only, paired-cert) and native on the ports host.env gives `serve`;
    // `--mgmt-bind` lands there first. GameStream 47984/47989/48010, 47998-48010; mDNS 5353.
    let host_env = std::fs::read_to_string(host_env_path()).unwrap_or_default();
    let tcp = format!("47984,47989,48010,{}", mgmt_port(&host_env));
    let udp = format!("47998-48010,{},5353", native_port(&host_env));
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

/// The any-UDP rule a host with a second video port needed. Video now rides the native port.
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
    for suffix in ["TCP", "UDP"] {
        // netsh matches rule names case-insensitively, so this also reaps the old lowercase names.
        let name = format!("Punktfunk {suffix}");
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
    let ps = crate::install::sys32(r"WindowsPowerShell\v1.0\powershell.exe");
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

    /// Missing executable → port-only rule, not no rule. A looser rule still streams; none is black.
    #[test]
    fn a_missing_program_falls_back_to_the_port_only_rule() {
        let args = fw_add_rule_args("Punktfunk TCP", "TCP", Some("47990"), None, "profile=any");
        assert!(!args.iter().any(|a| a.starts_with("program=")));
        assert!(args.contains(&"localport=47990".to_string()));
    }

    /// A host moved off Sunshine's 47990 opens the port it binds; clients learn it in-band.
    #[test]
    fn the_mgmt_rule_follows_the_host_env_bind() {
        assert_eq!(mgmt_port(""), 47990);
        assert_eq!(mgmt_port("PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\r\n"), 47991);
        assert_eq!(mgmt_port("# PUNKTFUNK_MGMT_BIND=0.0.0.0:48123\n"), 47990);
        assert_eq!(
            mgmt_port("PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\nPUNKTFUNK_MGMT_BIND=\n"),
            47990
        );
        assert_eq!(mgmt_port("PUNKTFUNK_MGMT_BIND=\"[::]:48123\"\n"), 48123);
        assert_eq!(mgmt_port("PUNKTFUNK_MGMT_BIND=nonsense\n"), 47990);
        let cmd = "PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\nPUNKTFUNK_HOST_CMD=serve --mgmt-bind 0.0.0.0:48123\n";
        assert_eq!(
            mgmt_port(cmd),
            48123,
            "the command line outranks the env line"
        );
    }

    /// A native port moved in host.env or on the host command is the one the UDP rule opens.
    #[test]
    fn the_native_rule_follows_the_serve_port() {
        assert_eq!(native_port(""), 9777);
        assert_eq!(native_port("PUNKTFUNK_NATIVE_PORT=9800\r\n"), 9800);
        assert_eq!(native_port("# PUNKTFUNK_NATIVE_PORT=9800\n"), 9777);
        assert_eq!(native_port("PUNKTFUNK_NATIVE_PORT=\n"), 9777);
        assert_eq!(
            native_port(
                "PUNKTFUNK_NATIVE_PORT=9800\nPUNKTFUNK_HOST_CMD=\"serve --native-port 9900\"\n"
            ),
            9900
        );
        assert_eq!(
            native_port("PUNKTFUNK_NATIVE_PORT=9800\nPUNKTFUNK_HOST_CMD=serve --gamestream\n"),
            9800
        );
    }

    /// `--mgmt-bind` writes the line the mgmt rule reads back, whatever host.env already holds.
    #[test]
    fn the_mgmt_bind_flag_lands_where_the_rule_reads() {
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
