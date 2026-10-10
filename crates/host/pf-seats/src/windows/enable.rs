//! Turning seats on and off: the prerequisite checks, the seat display driver, the Remote
//! Desktop listener and its firewall scope, and the pinned RDP certificate.
//!
//! [`checks`] only reads. [`turn_on`] creates the Seats key and records there what it changed
//! (`ListenerWasOff`, `AllowRdpFromNetwork`, `RdpRulesOpened`); [`turn_off`] undoes exactly
//! that and deletes the
//! key last, so a failed restore can be retried. A refusal carries one plain sentence for the
//! operator; its cause goes to the log. The seat display driver stays in the driver store when
//! seats go off.

use super::util::{backend_error, WinResult};
use super::{keeper, seats_enabled, server_edition, session_wrapper, SEATS_KEY};
use crate::backend::BackendError;
use crate::ipc::Diagnostic;
use pf_paths::POWERSHELL;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY};
use winreg::RegKey;

const TERMINAL_SERVER_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Terminal Server";
const LISTENER_DENIED: &str = "fDenyTSConnections";
/// Seats key values: the listener was off before [`turn_on`], and the firewall choice.
const LISTENER_WAS_OFF: &str = "ListenerWasOff";
const RDP_FROM_NETWORK: &str = "AllowRdpFromNetwork";
/// The `Remote Desktop` rules [`turn_on`] enabled for the network, one name per line.
const RDP_RULES_OPENED: &str = "RdpRulesOpened";
/// The `Remote Desktop` firewall group by the resource id every Windows language shares.
const RDP_RULE_GROUP: &str = "@FirewallAPI.dll,-28752";
const LOOPBACK: &str = "127.0.0.0/8";
/// The seats package, with the driver binary its INF lists, in `{app}\staging\pfvdisplay`.
const PACKAGE: [&str; 3] = [
    "pf_vdisplay_seats.inf",
    "pf_vdisplay_seats.cat",
    "pf_vdisplay.dll",
];
/// The RDP listener takes a moment to come up after `fDenyTSConnections` flips.
const TRUST_ATTEMPTS: u32 = 3;
const TRUST_RETRY: Duration = Duration::from_secs(2);
/// Role, licensing and a GPU as three `0`/`1` flags. The licensing types 2 and 4 are Per
/// Device and Per User; an adapter named `Microsoft …` is the basic or remote display driver.
const PROBE: &str = "$ProgressPreference='SilentlyContinue'; \
    $ErrorActionPreference='SilentlyContinue'; \
    $role = [bool](Get-WindowsFeature RDS-RD-Server).Installed; \
    $mode = (Get-CimInstance -Namespace root/cimv2/TerminalServices \
    -ClassName Win32_TerminalServiceSetting).LicensingType; \
    $gpu = [bool](Get-CimInstance Win32_VideoController | \
    Where-Object { $_.Name -notlike 'Microsoft *' }); \
    '{0} {1} {2}' -f [int]$role, [int]($mode -in 2,4), [int]$gpu";

/// The prerequisites for seats, one diagnostic each: info when met, an error with the next move
/// when not. A desktop edition runs seats only through a Remote Desktop wrapper the operator
/// installed, a warning, and has no role or licensing to ask about.
pub(super) fn checks() -> Vec<Diagnostic> {
    let mut checks = Vec::new();
    let server = match server_edition() {
        Some(true) => {
            checks.push(Diagnostic::info(
                "windows_server",
                "This is Windows Server.",
            ));
            true
        }
        Some(false) if session_wrapper() => {
            checks.push(Diagnostic::warning(
                "windows_server",
                "A Remote Desktop wrapper lets this desktop edition of Windows run seats. Only Windows Server is supported.",
            ));
            false
        }
        Some(false) => {
            checks.push(Diagnostic::error(
                "windows_server",
                "Seats need Windows Server — this is a desktop edition of Windows.",
            ));
            return checks;
        }
        None => {
            checks.push(Diagnostic::error(
                "windows_server",
                "Couldn't tell which edition of Windows this is.",
            ));
            return checks;
        }
    };
    let Some([role, licensing, gpu]) = probe() else {
        checks.push(Diagnostic::error(
            "probe",
            "Couldn't read this machine's Remote Desktop and graphics settings.",
        ));
        return checks;
    };
    if server {
        checks.push(check(
            role,
            "rds_role",
            "The Remote Desktop Session Host role is installed.",
            "The Remote Desktop Session Host role isn't installed. Add it in Server Manager, then try again.",
        ));
        checks.push(check(
            licensing,
            "rds_licensing",
            "Remote Desktop licensing is set.",
            "Remote Desktop licensing isn't set. Choose Per Device or Per User in Server Manager, then try again.",
        ));
    }
    checks.push(check(
        gpu,
        "gpu",
        "A graphics card is installed.",
        "No graphics card was found. Seats need a GPU.",
    ));
    checks
}

/// Turn seats on. A failure after the Seats key exists on a machine that had none puts the
/// machine back, so `enabled` never reports a half-done switch.
pub(super) fn turn_on(host_path: &Path, allow_rdp_from_network: bool) -> WinResult<()> {
    let fresh = !seats_enabled();
    let done = apply(host_path, allow_rdp_from_network);
    if done.is_err() && fresh {
        let _ = turn_off();
    }
    done
}

/// Restore the listener and the firewall scope `turn_on` changed, then remove the Seats key.
/// Nothing to do without the key.
pub(super) fn turn_off() -> WinResult<()> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let Ok(seats) = hklm.open_subkey_with_flags(SEATS_KEY, KEY_READ | KEY_WOW64_64KEY) else {
        return Ok(());
    };
    close_rdp_rules(&seats)?;
    if seats.get_value::<u32, _>(RDP_FROM_NETWORK).ok() == Some(0) {
        firewall_scope("Any").map_err(|cause| {
            refuse(
                "rdp_firewall",
                "Couldn't restore Remote Desktop's firewall rules.",
                cause,
            )
        })?;
    }
    if seats.get_value::<u32, _>(LISTENER_WAS_OFF).ok() == Some(1) {
        deny_listener(true).map_err(|cause| {
            refuse(
                "rdp_listener",
                "Couldn't turn Remote Desktop back off.",
                cause,
            )
        })?;
    }
    drop(seats);
    hklm.delete_subkey_with_flags(SEATS_KEY, KEY_WOW64_64KEY)
        .map_err(|cause| refuse("seats_key", "Couldn't turn seats off.", cause))
}

/// Seats are on and the operator let the network reach Remote Desktop.
pub(super) fn rdp_from_network() -> bool {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(SEATS_KEY, KEY_READ | KEY_WOW64_64KEY)
        .and_then(|seats| seats.get_value::<u32, _>(RDP_FROM_NETWORK))
        .is_ok_and(|allowed| allowed == 1)
}

/// A refusal as the check list shows it.
pub(super) fn refusal(error: BackendError) -> Diagnostic {
    Diagnostic::error(error.code, error.message)
}

fn apply(host_path: &Path, allow_rdp_from_network: bool) -> WinResult<()> {
    let dir = package_dir(host_path);
    if PACKAGE.iter().any(|name| !dir.join(name).is_file()) {
        return Err(refuse(
            "driver_missing",
            "The seat display driver isn't on this machine. Reinstall punktfunk, then try again.",
            dir.display(),
        ));
    }
    let (seats, _) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey_with_flags(SEATS_KEY, KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY)
        .map_err(|cause| refuse("seats_key", "Couldn't turn seats on.", cause))?;
    install_driver(&dir)?;
    allow_listener(&seats)?;
    // A choice made earlier narrowed the rules; allowing the network now widens them again.
    let narrowed = seats.get_value::<u32, _>(RDP_FROM_NETWORK).ok() == Some(0);
    let scope = match (allow_rdp_from_network, narrowed) {
        (false, _) => Some(LOOPBACK),
        (true, true) => Some("Any"),
        (true, false) => None,
    };
    if let Some(scope) = scope {
        firewall_scope(scope).map_err(|cause| {
            refuse(
                "rdp_firewall",
                "Couldn't limit Remote Desktop to this machine.",
                cause,
            )
        })?;
    }
    if allow_rdp_from_network {
        open_rdp_rules(&seats)?;
    } else {
        close_rdp_rules(&seats)?;
    }
    seats
        .set_value(RDP_FROM_NETWORK, &u32::from(allow_rdp_from_network))
        .map_err(|cause| refuse("seats_key", "Couldn't turn seats on.", cause))?;
    trust_rdp_certificate(host_path)
}

/// `{app}\staging\pfvdisplay`: the packages setup leaves beside the host for the console.
fn package_dir(host_path: &Path) -> PathBuf {
    host_path.with_file_name("staging").join("pfvdisplay")
}

/// Add the seats package to the driver store. The terminal-services stack makes the devnode
/// when a seat's session starts, so none is created here. 3010 is a driver added that a
/// restart would load.
fn install_driver(dir: &Path) -> WinResult<()> {
    let inf = dir.join(PACKAGE[0]);
    match run(
        "pnputil.exe",
        &["/add-driver", &inf.to_string_lossy(), "/install"],
    ) {
        Some(0 | 3010) => Ok(()),
        code => Err(refuse(
            "driver_install",
            "Windows couldn't install the seat display driver.",
            format!("pnputil exited {code:?}"),
        )),
    }
}

/// Turn the Remote Desktop listener on, noting under the Seats key that it was off.
fn allow_listener(seats: &RegKey) -> WinResult<()> {
    let listener =
        |cause: std::io::Error| refuse("rdp_listener", "Couldn't turn on Remote Desktop.", cause);
    let terminal_server = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(
            TERMINAL_SERVER_KEY,
            KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY,
        )
        .map_err(listener)?;
    if terminal_server.get_value::<u32, _>(LISTENER_DENIED).ok() == Some(1) {
        // The note comes first: a crash between the two writes still restores the listener.
        seats
            .set_value(LISTENER_WAS_OFF, &1_u32)
            .map_err(listener)?;
        deny_listener(false).map_err(listener)?;
    }
    Ok(())
}

fn deny_listener(deny: bool) -> std::io::Result<()> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(TERMINAL_SERVER_KEY, KEY_SET_VALUE | KEY_WOW64_64KEY)?
        .set_value(LISTENER_DENIED, &u32::from(deny))
}

/// Scope the `Remote Desktop` rule group to `remote`: an address range or `Any`.
fn firewall_scope(remote: &str) -> Result<(), String> {
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         Set-NetFirewallRule -Group '{RDP_RULE_GROUP}' -RemoteAddress {remote}"
    );
    match run(POWERSHELL, &powershell_args(&script)) {
        Some(0) => Ok(()),
        code => Err(format!("Set-NetFirewallRule exited {code:?}")),
    }
}

/// Enable the `Remote Desktop` rules that are off, for an operator who lets the network reach
/// Remote Desktop. Turning the listener on by registry enables none. The names are noted first,
/// so a crash between the two still closes them.
fn open_rdp_rules(seats: &RegKey) -> WinResult<()> {
    let fail = |cause: String| {
        refuse(
            "rdp_firewall",
            "Couldn't open Remote Desktop to the network.",
            cause,
        )
    };
    let off = powershell_lines(&format!(
        "$ErrorActionPreference='Stop'; \
         Get-NetFirewallRule -Group '{RDP_RULE_GROUP}' | \
         Where-Object {{ $_.Enabled -eq 'False' }} | ForEach-Object {{ $_.Name }}"
    ))
    .map_err(fail)?;
    if off.is_empty() {
        return Ok(());
    }
    let mut opened = rules_opened(seats);
    opened.extend(off.iter().cloned());
    opened.sort();
    opened.dedup();
    seats
        .set_value(RDP_RULES_OPENED, &opened.join("\n"))
        .map_err(|cause| fail(cause.to_string()))?;
    powershell_lines(&format!(
        "$ErrorActionPreference='Stop'; Enable-NetFirewallRule -Name {}",
        ps_list(&off)
    ))
    .map(drop)
    .map_err(fail)
}

/// Disable the rules [`open_rdp_rules`] enabled, then forget them. A rule since removed is
/// skipped.
fn close_rdp_rules(seats: &RegKey) -> WinResult<()> {
    let opened = rules_opened(seats);
    if opened.is_empty() {
        return Ok(());
    }
    let fail = |cause: String| {
        refuse(
            "rdp_firewall",
            "Couldn't close Remote Desktop to the network.",
            cause,
        )
    };
    powershell_lines(&format!(
        "$ErrorActionPreference='Stop'; \
         Get-NetFirewallRule -Name {} -ErrorAction SilentlyContinue | Disable-NetFirewallRule",
        ps_list(&opened)
    ))
    .map_err(fail)?;
    match seats.delete_value(RDP_RULES_OPENED) {
        Err(cause) if cause.kind() != std::io::ErrorKind::NotFound => Err(fail(cause.to_string())),
        _ => Ok(()),
    }
}

fn rules_opened(seats: &RegKey) -> Vec<String> {
    seats
        .get_value::<String, _>(RDP_RULES_OPENED)
        .map(|names| names.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// `names` as a PowerShell string array, each single-quoted.
fn ps_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("'{}'", name.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(",")
}

/// A PowerShell script's non-empty output lines. `Err` when it fails or doesn't launch.
fn powershell_lines(script: &str) -> Result<Vec<String>, String> {
    let output = Command::new(pf_paths::system32(POWERSHELL))
        .args(powershell_args(script))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!("powershell exited {:?}", output.status.code()));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Record the leaf TermService presents. The keeper observes it, since the handshake needs
/// IronRDP, which the service doesn't link.
fn trust_rdp_certificate(host_path: &Path) -> WinResult<()> {
    let keeper = keeper::keeper_path(host_path);
    if !keeper.is_file() {
        return Err(refuse(
            "keeper_missing",
            "A punktfunk file is missing. Reinstall punktfunk, then try again.",
            keeper.display(),
        ));
    }
    let mut cause = String::new();
    for attempt in 0..TRUST_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(TRUST_RETRY);
        }
        match Command::new(&keeper)
            .arg("trust")
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) if output.status.success() => return Ok(()),
            Ok(output) => cause = String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            Err(error) => cause = error.to_string(),
        }
    }
    Err(refuse(
        "rdp_trust",
        "Couldn't record this machine's Remote Desktop certificate. Check that Remote Desktop is running, then try again.",
        cause,
    ))
}

fn probe() -> Option<[bool; 3]> {
    let output = Command::new(pf_paths::system32(POWERSHELL))
        .args(powershell_args(PROBE))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_probe(&String::from_utf8_lossy(&output.stdout))
}

/// The last non-empty line: three `0`/`1` flags.
fn parse_probe(text: &str) -> Option<[bool; 3]> {
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let flags: Vec<bool> = line.split_whitespace().map(|flag| flag == "1").collect();
    flags.try_into().ok()
}

fn powershell_args(script: &str) -> [&str; 6] {
    [
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script,
    ]
}

/// A System32 tool's exit code, output discarded. `None` when it did not launch.
fn run(tool: &str, args: &[&str]) -> Option<i32> {
    Command::new(pf_paths::system32(tool))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?
        .code()
}

fn check(ok: bool, code: &str, met: &str, unmet: &str) -> Diagnostic {
    if ok {
        Diagnostic::info(code, met)
    } else {
        Diagnostic::error(code, unmet)
    }
}

/// A refusal the operator reads: `sentence` is the answer, `cause` goes to the log.
fn refuse(code: &str, sentence: &str, cause: impl std::fmt::Display) -> BackendError {
    tracing::warn!(code, %cause, "seat change refused");
    backend_error(code, sentence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_answers_with_three_flags_on_its_last_line() {
        assert_eq!(parse_probe("1 0 1\r\n"), Some([true, false, true]));
        assert_eq!(parse_probe("noise\r\n0 0 0\r\n\r\n"), Some([false; 3]));
        assert_eq!(parse_probe("1 1"), None);
        assert_eq!(parse_probe(""), None);
    }

    #[test]
    fn rule_names_reach_powershell_quoted() {
        let names = [
            "RemoteDesktop-UserMode-In-TCP".to_string(),
            "it's".to_string(),
        ];
        assert_eq!(ps_list(&names), "'RemoteDesktop-UserMode-In-TCP','it''s'");
    }
}
