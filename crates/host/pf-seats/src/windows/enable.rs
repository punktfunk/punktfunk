//! Turning seats on and off: the prerequisite checks, the seat display driver, the Remote
//! Desktop listener and its firewall scope, the seat listener, and the pinned RDP certificate.
//!
//! [`checks`] only reads. [`turn_on`] creates the Seats key and records there what it changed
//! (`ListenerWasOff`, `AllowRdpFromNetwork`, `RdpRulesOpened`, `RdpRuleScopes`,
//! `RdpRulesNarrowed`); [`turn_off`] undoes exactly that and deletes the key last, so a failed
//! restore can be retried. Both read the key into [`Recorded`], take their steps from
//! [`crate::seat_switch`] and [`perform`] them in order. A refusal carries one plain sentence for
//! the operator; its cause goes to the log. The seat display driver stays in the driver store
//! when seats go off.

use super::util::{backend_error, WinResult};
use super::{keeper, seats_enabled, server_edition, session_wrapper, ENABLING, SEATS_KEY};
use crate::backend::BackendError;
use crate::doctor::DriverState;
use crate::ipc::Diagnostic;
use crate::seat_switch::{self, Recorded, Step};
use pf_paths::POWERSHELL;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError, TryLockError};
use std::time::{Duration, Instant};
use winreg::enums::{RegDisposition, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, KEY_WOW64_64KEY};
use winreg::RegKey;

/// The role's state while it is installed and waits on a restart, as [`PROBE`] names it.
const RESTART_PENDING: &str = "RestartPending";
/// The row for it: an error so seats don't turn on, shown as the next step.
const RESTART: &str = "Remote Desktop Session Host is installed. Restart the PC, then press Set up Remote Desktop again to finish.";
const TERMINAL_SERVER_KEY: &str = r"SYSTEM\CurrentControlSet\Control\Terminal Server";
const LISTENER_DENIED: &str = "fDenyTSConnections";
/// Seats key values: the listener was off before [`turn_on`], and the firewall choice.
const LISTENER_WAS_OFF: &str = "ListenerWasOff";
const RDP_FROM_NETWORK: &str = "AllowRdpFromNetwork";
/// The `Remote Desktop` rules [`turn_on`] enabled for the network, one name per line.
const RDP_RULES_OPENED: &str = "RdpRulesOpened";
/// Each `Remote Desktop` rule's `RemoteAddress` before [`turn_on`] narrowed it, `name|address`
/// per line, for [`turn_off`] to write back.
const RDP_RULE_SCOPES: &str = "RdpRuleScopes";
/// Set before [`turn_on`] narrows the rules, deleted once [`restore_scopes`] put them back.
const RDP_RULES_NARROWED: &str = "RdpRulesNarrowed";
/// The `Remote Desktop` firewall group by the resource id every Windows language shares.
const RDP_RULE_GROUP: &str = "@FirewallAPI.dll,-28752";
/// `Remote Desktop Services`, the Session Host role's group, and its two rules on the
/// listener's port. The group's other rules manage the role and stay as they are.
const RDS_RULE_GROUP: &str = "@FirewallAPI.dll,-28791";
const RDS_LISTENER_RULES: &str =
    "'RemoteDesktopServices-UserMode-In-TCP','RemoteDesktopServices-UserMode-In-UDP'";
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
/// How long a tool may run before it counts as hung and is killed. A permit of the pipe is
/// pinned meanwhile. Windows answers its own state slowly for a minute after a restart, and a
/// role install runs for minutes.
const QUERY_LIMIT: Duration = Duration::from_secs(180);
/// What the doctor's two PowerShell queries may take together. The client stops waiting on the
/// doctor after five minutes, and the keeper's look and the local reads follow the queries.
pub(super) const DOCTOR_BUDGET: Duration = Duration::from_secs(240);
const INSTALL_LIMIT: Duration = Duration::from_secs(900);
const TOOL_LIMIT: Duration = Duration::from_secs(300);
/// The keeper's own handshake gives up after 15 s.
const KEEPER_LIMIT: Duration = Duration::from_secs(60);
/// The role's install state, then licensing and a GPU as `0`/`1` flags, on the last line. A role
/// the cmdlet can't read is `Unknown`, after the cmdlet's error. `Available`, a role not
/// installed, is the state enum's zero and false in PowerShell, so the state is tested against
/// null. The licensing types 2 and 4 are Per Device and Per User. A GPU sits on PCI and isn't
/// `Microsoft …`, the basic display driver: punktfunk's own display (`ROOT\…`) and a seat's
/// (`SWD\…`) don't count. An installed role reads `RestartPending` until the restart that
/// starts it: Windows reports `Installed` at once and stays in administration mode until then.
const PROBE: &str = "$ProgressPreference='SilentlyContinue'; \
    $ErrorActionPreference='SilentlyContinue'; \
    $role = 'Unknown'; \
    try { $state = (Get-WindowsFeature RDS-RD-Server -ErrorAction Stop).InstallState; \
    if ($null -ne $state) { $role = $state } } catch { $_.Exception.Message }; \
    $ts = Get-CimInstance -Namespace root/cimv2/TerminalServices \
    -ClassName Win32_TerminalServiceSetting; \
    $mode = $ts.LicensingType; \
    if ([string]$role -eq 'Installed' -and $ts -and $ts.TerminalServerMode -ne 1) \
    { $role = 'RestartPending' }; \
    $gpu = [bool](Get-CimInstance Win32_VideoController | \
    Where-Object { $_.PNPDeviceID -like 'PCI\\*' -and $_.Name -notlike 'Microsoft *' }); \
    '{0} {1} {2}' -f $role, [int]($mode -in 2,4), [int]$gpu";

/// The prerequisites for seats, one diagnostic each: info when met, an error with the next move
/// when not. A desktop edition runs seats only through a Remote Desktop wrapper the operator
/// installed, a warning, and has no role or licensing to ask about.
pub(super) fn checks() -> Vec<Diagnostic> {
    checks_until(Instant::now() + QUERY_LIMIT)
}

/// [`checks`], its query given until `deadline`. A query the time doesn't reach reads as one that
/// failed: the probe row says the settings couldn't be read.
pub(super) fn checks_until(deadline: Instant) -> Vec<Diagnostic> {
    let mut checks = Vec::new();
    let server = match server_edition() {
        Some(true) => {
            checks.push(
                Diagnostic::info("windows_server", "This is Windows Server.")
                    .worded("seatcheck_windows_server_ok"),
            );
            true
        }
        Some(false) if session_wrapper() => {
            checks.push(Diagnostic::warning(
                "windows_server",
                "A Remote Desktop wrapper lets this desktop edition of Windows run concurrent seats. Only Windows Server is supported for that. To take turns instead, add an Own account profile.",
            ).worded("seatcheck_windows_server_wrapper"));
            false
        }
        Some(false) => {
            checks.push(Diagnostic::error(
                "windows_server",
                "Playing at the same time needs Windows Server — this is a desktop edition of Windows. To take turns, add an Own account profile instead.",
            ).worded("seatcheck_windows_server_desktop"));
            return checks;
        }
        None => {
            checks.push(
                Diagnostic::error(
                    "windows_server",
                    "Couldn't tell which edition of Windows this is.",
                )
                .worded("seatcheck_edition_unknown"),
            );
            return checks;
        }
    };
    let Some((role, [licensing, gpu])) = probe(deadline) else {
        checks.push(
            Diagnostic::error(
                "probe",
                "Couldn't read this machine's Remote Desktop and graphics settings.",
            )
            .worded("seatcheck_probe"),
        );
        return checks;
    };
    if server {
        checks.push(role_check(&role));
        // Licensing takes a mode only once the restart has started the role.
        if role != RESTART_PENDING {
            checks.push(check(
                licensing,
                "rds_licensing",
                ["seatcheck_rds_licensing_ok", "seatcheck_rds_licensing_missing"],
                "Remote Desktop licensing is set.",
                "Remote Desktop licensing isn't set. Press Set up Remote Desktop in Profiles → Seats to set it to Per User.",
            ));
        }
    }
    checks.push(check(
        gpu,
        "gpu",
        ["seatcheck_gpu_ok", "seatcheck_gpu_missing"],
        "A graphics card is installed.",
        "No graphics card was found. Seats need a GPU.",
    ));
    checks
}

/// Install the Session Host role and set licensing to Per User, where Windows Server lacks them.
/// The last line is the role's outcome, then licensing's: `kept`, `installed` or `restart`, and
/// `kept`, `set`, `skipped` (the role waits on a restart) or a `ChangeMode` return value. A role
/// installed earlier and not restarted since is `restart` again.
const PREPARE: &str = "$ProgressPreference='SilentlyContinue'; $ErrorActionPreference='Stop'; \
    $role = 'kept'; \
    if ((Get-WindowsFeature RDS-RD-Server).InstallState -ne 'Installed') { \
    $r = Install-WindowsFeature RDS-RD-Server; \
    if (-not $r.Success) { throw 'Install-WindowsFeature did not succeed' }; \
    $role = if ([string]$r.RestartNeeded -eq 'No') { 'installed' } else { 'restart' } } \
    elseif ((Get-CimInstance -Namespace root/cimv2/TerminalServices \
    -ClassName Win32_TerminalServiceSetting).TerminalServerMode -ne 1) { $role = 'restart' }; \
    $lic = 'skipped'; \
    if ($role -ne 'restart') { \
    $s = Get-CimInstance -Namespace root/cimv2/TerminalServices -ClassName Win32_TerminalServiceSetting; \
    if ($s.LicensingType -in 2,4) { $lic = 'kept' } else { \
    $c = $s | Invoke-CimMethod -MethodName ChangeMode -Arguments @{LicensingType=4}; \
    $lic = if ($c.ReturnValue -eq 0) { 'set' } else { [string]$c.ReturnValue } } }; \
    '{0} {1}' -f $role, $lic";

/// What [`checks`] refuses and Windows Server can set up itself: the Session Host role, then a
/// licensing mode. One row per change or refusal; nothing on a desktop edition.
pub(super) fn prepare() -> Vec<Diagnostic> {
    if server_edition() != Some(true) {
        return Vec::new();
    }
    let lines = match powershell_lines(PREPARE, INSTALL_LIMIT) {
        Ok(lines) => lines,
        Err(cause) => {
            tracing::warn!(%cause, "remote desktop setup did not complete");
            return vec![Diagnostic::error(
                "rds_setup",
                "Windows couldn't set up Remote Desktop. Add Remote Desktop Session Host under Remote Desktop Services in Server Manager, restart, then try again.",
            )
            .worded("seatcheck_rds_setup")];
        }
    };
    tracing::info!(output = ?lines, "remote desktop setup");
    let last = lines.last().map(String::as_str).unwrap_or_default();
    let (role, licensing) = last.split_once(' ').unwrap_or((last, ""));
    let mut rows = Vec::new();
    match role {
        "installed" => rows.push(
            Diagnostic::info(
                "rds_role_installed",
                "Remote Desktop Session Host is now installed.",
            )
            .worded("seatcheck_rds_role_installed"),
        ),
        "restart" => rows.push(Diagnostic::error("restart", RESTART).worded("seatcheck_restart")),
        _ => {}
    }
    match licensing {
        "set" => rows.push(
            Diagnostic::info(
                "rds_licensing_set",
                "Remote Desktop licensing is now Per User.",
            )
            .worded("seatcheck_rds_licensing_set"),
        ),
        "kept" | "skipped" => {}
        code => tracing::warn!(code, "licensing mode change refused"),
    }
    rows
}

/// Held while seats turn on or off, so [`keep_rdp_local`] never narrows rules a switch is
/// widening on the way.
static SWITCHING: Mutex<()> = Mutex::new(());

/// Turn seats on. A failure where seats were off puts the machine back, and a crash leaves the
/// Seats key marked [`ENABLING`], so `enabled` never reports a half-done switch.
pub(super) fn turn_on(host_path: &Path, allow_rdp_from_network: bool) -> WinResult<()> {
    let _switching = SWITCHING.lock().unwrap_or_else(PoisonError::into_inner);
    let fresh = !seats_enabled();
    let done = apply(host_path, allow_rdp_from_network);
    if done.is_err() && fresh {
        let _ = off();
    }
    done
}

/// Undo what `turn_on` recorded ([`seat_switch::off`]), then remove the Seats key.
pub(super) fn turn_off() -> WinResult<()> {
    let _switching = SWITCHING.lock().unwrap_or_else(PoisonError::into_inner);
    off()
}

/// [`turn_off`] under its caller's hold. Nothing to do without the key. Each restore forgets its
/// value as it finishes, so the key is opened for writing.
fn off() -> WinResult<()> {
    let Ok(seats) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(SEATS_KEY, KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY)
    else {
        return Ok(());
    };
    // No step of seats off reads the host path.
    perform_all(&seat_switch::off(&recorded(&seats)), seats, Path::new(""))
}

/// What the Seats key records of the changes a turn-on made.
fn recorded(seats: &RegKey) -> Recorded {
    Recorded {
        enabling: seats.get_value::<u32, _>(ENABLING).is_ok(),
        listener_was_off: seats.get_value::<u32, _>(LISTENER_WAS_OFF).ok() == Some(1),
        choice: seats.get_value::<u32, _>(RDP_FROM_NETWORK).ok(),
        rules_opened: !rules_opened(seats).is_empty(),
        scopes_noted: seats.get_value::<String, _>(RDP_RULE_SCOPES).is_ok(),
        narrowed_noted: seats.get_value::<u32, _>(RDP_RULES_NARROWED).is_ok(),
    }
}

/// [`perform`] each of `steps` in order and stop at the first refusal. The key is closed before
/// [`Step::DeleteKey`] deletes it.
fn perform_all(steps: &[Step], seats: RegKey, host_path: &Path) -> WinResult<()> {
    for &step in steps {
        if step == Step::DeleteKey {
            drop(seats);
            return delete_key();
        }
        perform(step, &seats, host_path)?;
    }
    Ok(())
}

/// One step of a switch, against the open Seats key. A refusal names what didn't happen.
fn perform(step: Step, seats: &RegKey, host_path: &Path) -> WinResult<()> {
    let key_on = |cause: std::io::Error| {
        refuse("seats_key", "Couldn't turn seats on.", cause).said("seats_key_on")
    };
    let limit = |cause: String| {
        refuse(
            "rdp_firewall",
            "Couldn't limit Remote Desktop to this machine.",
            cause,
        )
        .said("rdp_firewall_limit")
    };
    match step {
        Step::MarkEnabling => seats.set_value(ENABLING, &1_u32).map_err(key_on),
        Step::InstallDriver => install_driver(&package_dir(host_path), true),
        Step::RecordScopes => record_scopes(seats),
        Step::NoteNarrowed => seats
            .set_value(RDP_RULES_NARROWED, &1_u32)
            .map_err(|cause| limit(cause.to_string())),
        Step::NarrowScopes => firewall_scope(LOOPBACK).map_err(limit),
        Step::RestoreScopes => restore_scopes(seats),
        Step::OpenRules => open_rdp_rules(seats),
        Step::CloseRules => close_rdp_rules(seats),
        Step::WriteChoice(allow) => seats
            .set_value(RDP_FROM_NETWORK, &u32::from(allow))
            .map_err(key_on),
        Step::AllowListener => allow_listener(seats),
        Step::EnsureSeatListener => {
            if let Err(cause) = super::seat_listener::ensure(host_path) {
                tracing::warn!(%cause, "seat listener not ready — seats start without pad audio");
            }
            Ok(())
        }
        Step::TrustCertificate => trust_rdp_certificate(host_path),
        Step::Finish => match seats.delete_value(ENABLING) {
            Err(cause) if cause.kind() != std::io::ErrorKind::NotFound => Err(key_on(cause)),
            _ => Ok(()),
        },
        Step::DenyListener => deny_listener(true).map_err(|cause| {
            refuse(
                "rdp_listener",
                "Couldn't turn Remote Desktop back off.",
                cause,
            )
            .said("rdp_listener_off")
        }),
        Step::RemoveSeatListener => {
            super::seat_listener::remove();
            Ok(())
        }
        Step::RestoreShortcuts => super::steam::unwrap_box_shortcuts().map_err(|cause| {
            refuse("steam_shortcuts", "Couldn't turn seats off.", cause).said("seats_key_off")
        }),
        Step::RestoreRegistration => {
            super::steam::restore_registration();
            Ok(())
        }
        Step::DeleteKey => delete_key(),
    }
}

fn delete_key() -> WinResult<()> {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .delete_subkey_with_flags(SEATS_KEY, KEY_WOW64_64KEY)
        .map_err(|cause| {
            refuse("seats_key", "Couldn't turn seats off.", cause).said("seats_key_off")
        })
}

/// Whether the network reaches Remote Desktop: the operator's choice while seats are on. While
/// they are off it is what the machine does today, so turning seats on keeps a server that is
/// managed over Remote Desktop reachable unless the operator unticks it.
pub(super) fn rdp_from_network() -> bool {
    match RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(SEATS_KEY, KEY_READ | KEY_WOW64_64KEY)
    {
        Ok(seats) => seats.get_value::<u32, _>(RDP_FROM_NETWORK).ok() == Some(1),
        Err(_) => rdp_open_today(),
    }
}

/// Where Windows Firewall keeps its local rules, one `|`-separated string each.
const FIREWALL_RULES: &str =
    r"SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules";
/// Where Group Policy puts its rules, in the same form.
const POLICY_RULES: &str = r"SOFTWARE\Policies\Microsoft\WindowsFirewall\FirewallRules";

/// The listener is on and a firewall rule lets any address reach it.
fn rdp_open_today() -> bool {
    let listening = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(TERMINAL_SERVER_KEY, KEY_READ | KEY_WOW64_64KEY)
        .and_then(|key| key.get_value::<u32, _>(LISTENER_DENIED))
        .is_ok_and(|denied| denied == 0);
    listening && rdp_rule_open()
}

/// A local firewall rule of the Remote Desktop groups lets any address reach Remote Desktop.
/// Read from the registry: the dialog asks every time it opens, and PowerShell's firewall
/// cmdlets take seconds, a minute after a restart.
fn rdp_rule_open() -> bool {
    firewall_rules(FIREWALL_RULES)
        .iter()
        .any(|rule| open_rdp_rule(rule))
}

/// The rules under `store`, one `|`-separated string each; none when it can't be read.
fn firewall_rules(store: &str) -> Vec<String> {
    let Ok(rules) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(store, KEY_READ | KEY_WOW64_64KEY)
    else {
        return Vec::new();
    };
    let names: Vec<String> = rules
        .enum_values()
        .flatten()
        .map(|(name, _)| name)
        .collect();
    names
        .iter()
        .filter_map(|name| rules.get_value::<String, _>(name).ok())
        .collect()
}

/// The operator keeps Remote Desktop to this PC: the Seats key's choice.
pub(super) fn rdp_local_chosen() -> bool {
    RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(SEATS_KEY, KEY_READ | KEY_WOW64_64KEY)
        .is_ok_and(|seats| seats.get_value::<u32, _>(RDP_FROM_NETWORK).ok() == Some(0))
}

/// A local or Group Policy firewall rule, in any group, lets an address other than this PC's
/// reach Remote Desktop ([`seat_switch::reaches_rdp_from_network`]). For the doctor.
pub(super) fn rdp_reachable() -> bool {
    [FIREWALL_RULES, POLICY_RULES]
        .iter()
        .flat_map(|store| firewall_rules(store))
        .any(|rule| seat_switch::reaches_rdp_from_network(&rule))
}

/// Narrow Remote Desktop again where the operator keeps it on this PC and a rule of its groups
/// is open to the network: the Session Host role, Windows updates and a restore of the defaults
/// add or reset them. A registry read decides whether PowerShell runs, so the service repeats
/// this on its tick. A look while seats turn on or off is skipped.
pub(super) fn keep_rdp_local() {
    let _switching = match SWITCHING.try_lock() {
        Ok(held) => held,
        Err(TryLockError::Poisoned(held)) => held.into_inner(),
        Err(TryLockError::WouldBlock) => return,
    };
    if rdp_local_chosen() && rdp_rule_open() {
        match firewall_scope(LOOPBACK) {
            Ok(()) => tracing::info!("remote desktop narrowed to this pc again"),
            Err(cause) => tracing::warn!(%cause, "remote desktop not narrowed to this pc"),
        }
    }
}

/// An enabled inbound rule of either Remote Desktop group on the listener's port, with no
/// remote address named: `RA4=`/`RA6=` fields are how a rule limits who may connect.
fn open_rdp_rule(rule: &str) -> bool {
    let fields: Vec<&str> = rule.split('|').collect();
    let has = |field: &str| fields.iter().any(|f| f.eq_ignore_ascii_case(field));
    [RDP_RULE_GROUP, RDS_RULE_GROUP]
        .iter()
        .any(|group| has(&format!("EmbedCtxt={group}")))
        && has("Active=TRUE")
        && has("Dir=In")
        && has("LPort=3389")
        && !fields
            .iter()
            .any(|f| f.starts_with("RA4=") || f.starts_with("RA6="))
}

/// Days left without a licence server, then how many are named, as `days|servers`. The days
/// are empty when Windows doesn't answer.
const LICENSING: &str = "$ErrorActionPreference='SilentlyContinue'; \
    $s = Get-CimInstance -Namespace root/cimv2/TerminalServices \
    -ClassName Win32_TerminalServiceSetting; \
    $days = ''; $g = $s | Invoke-CimMethod -MethodName GetGracePeriodDays; \
    if ($g -and $g.ReturnValue -eq 0) { $days = [string]$g.DaysLeft }; \
    $servers = @(($s | Invoke-CimMethod -MethodName GetSpecifiedLicenseServerList).SpecifiedLSList | \
    Where-Object { $_ }).Count; \
    '{0}|{1}' -f $days, $servers";

/// How long Remote Desktop still runs without a licence server, for the doctor, until `deadline`.
/// Windows Server signs seats in for 120 days without one. `None` when Windows doesn't say; a
/// query that fails or runs out of time is a warning.
pub(super) fn licensing_row(deadline: Instant) -> Option<Diagnostic> {
    let lines = time_left(deadline)
        .ok_or_else(|| "the doctor's time is up".to_owned())
        .and_then(|limit| powershell_lines(LICENSING, limit));
    let lines = match lines {
        Ok(lines) => lines,
        Err(cause) => {
            tracing::warn!(%cause, "remote desktop licensing not read");
            return Some(
                Diagnostic::warning(
                    "rds_grace",
                    "Couldn't read how long Remote Desktop runs without a license server. Run the check again in a few minutes.",
                )
                .worded("seatcheck_rds_grace_unread"),
            );
        }
    };
    let (days, servers) = lines.last()?.split_once('|')?;
    grace_row(
        days.parse().ok(),
        servers.parse::<u32>().is_ok_and(|n| n > 0),
    )
}

/// A licence server ends the countdown. Without one the row warns for the last 30 days and
/// refuses at zero, when Windows stops signing seats in.
fn grace_row(days_left: Option<u32>, licence_server: bool) -> Option<Diagnostic> {
    const CODE: &str = "rds_grace";
    if licence_server {
        return Some(
            Diagnostic::info(
                CODE,
                "Remote Desktop gets its licenses from a license server.",
            )
            .worded("seatcheck_rds_grace_server"),
        );
    }
    let more = |days: u32| match days {
        1 => "1 more day".to_string(),
        days => format!("{days} more days"),
    };
    Some(match days_left? {
        0 => Diagnostic::error(
            CODE,
            "Remote Desktop's days without a license server are over, so seats can't sign in. \
             Set up a license server with CALs.",
        )
        .worded("seatcheck_rds_grace_over"),
        days @ 1..=30 => Diagnostic::warning(
            CODE,
            format!(
                "Remote Desktop runs {} without a license server. After that \
                 seats can't sign in: set one up with CALs.",
                more(days)
            ),
        )
        .worded("seatcheck_rds_grace_ending")
        .param("count", days),
        days => Diagnostic::info(
            CODE,
            format!(
                "Remote Desktop runs {} without a license server.",
                more(days)
            ),
        )
        .worded("seatcheck_rds_grace_days")
        .param("count", days),
    })
}

/// A refusal as the check list shows it.
pub(super) fn refusal(error: BackendError) -> Diagnostic {
    let mut row = Diagnostic::error(error.code, error.message);
    row.id = error.said.map(|code| format!("apierr_{code}"));
    row.params = error.params;
    row
}

/// Create or open the Seats key and perform [`seat_switch::on`] for what it records. A new key
/// reads as off until the last step; a finished one stays on while it is redone.
fn apply(host_path: &Path, allow_rdp_from_network: bool) -> WinResult<()> {
    let dir = package_dir(host_path);
    if PACKAGE.iter().any(|name| !dir.join(name).is_file()) {
        return Err(refuse(
            "driver_missing",
            "The seat display driver isn't on this machine. Reinstall Punktfunk, then try again.",
            dir.display(),
        )
        .said("seat_display_driver"));
    }
    let (seats, disposition) = RegKey::predef(HKEY_LOCAL_MACHINE)
        .create_subkey_with_flags(SEATS_KEY, KEY_READ | KEY_SET_VALUE | KEY_WOW64_64KEY)
        .map_err(|cause| {
            refuse("seats_key", "Couldn't turn seats on.", cause).said("seats_key_on")
        })?;
    let recorded = (disposition != RegDisposition::REG_CREATED_NEW_KEY).then(|| recorded(&seats));
    perform_all(
        &seat_switch::on(recorded, allow_rdp_from_network),
        seats,
        host_path,
    )
}

/// `{app}\staging\pfvdisplay`: the packages setup leaves beside the host for the console.
fn package_dir(host_path: &Path) -> PathBuf {
    host_path.with_file_name("staging").join("pfvdisplay")
}

pub(super) fn driver_state(host_path: &Path) -> DriverState {
    let Some(staged) = driver_ver(&package_dir(host_path).join(PACKAGE[0])) else {
        return DriverState::Unstaged;
    };
    let stored = store_versions();
    if stored.contains(&staged) {
        DriverState::Current
    } else if stored.is_empty() {
        DriverState::Missing
    } else {
        DriverState::Outdated
    }
}

/// Put the staged seats package in the driver store when the store lacks that version: an
/// uninstall deletes it, and an upgrade stages a new one only beside the host. Seats running
/// now keep theirs until their next session.
pub(super) fn ensure_driver(host_path: &Path) -> WinResult<()> {
    let state = driver_state(host_path);
    if state == DriverState::Current {
        return Ok(());
    }
    tracing::info!(
        ?state,
        "seat display driver not current in the driver store — installing"
    );
    let dir = package_dir(host_path);
    if PACKAGE.iter().any(|name| !dir.join(name).is_file()) {
        return Err(refuse(
            "seat_display_driver",
            "The seat display driver isn't on this machine. Reinstall Punktfunk, then try again.",
            dir.display(),
        )
        .said("seat_display_driver"));
    }
    install_driver(&dir, false).map_err(|error| BackendError {
        code: "seat_display_driver".into(),
        ..error
    })
}

/// The `DriverVer` value of an INF, such as `10/12/2026,9.9.1012.257`, spaces dropped.
fn driver_ver(inf: &Path) -> Option<String> {
    let bytes = std::fs::read(inf).ok()?;
    let text = match bytes.as_slice() {
        [0xFF, 0xFE, rest @ ..] => String::from_utf16_lossy(
            &rest
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        ),
        _ => String::from_utf8_lossy(&bytes).into_owned(),
    };
    inf_driver_ver(&text)
}

fn inf_driver_ver(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        key.trim().eq_ignore_ascii_case("DriverVer").then(|| {
            let value = value.split(';').next().unwrap_or_default();
            value.chars().filter(|c| !c.is_whitespace()).collect()
        })
    })
}

/// The `DriverVer` of every seats package in the driver store, one folder each.
fn store_versions() -> Vec<String> {
    let repository = pf_paths::system32(r"DriverStore\FileRepository");
    let Ok(entries) = std::fs::read_dir(repository) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("pf_vdisplay_seats.inf_")
        })
        .filter_map(|entry| driver_ver(&entry.path().join(PACKAGE[0])))
        .collect()
}

/// Whether Windows' own Remote Desktop display driver is dated after ours: the newer date wins
/// the seat's display, and a seat on Microsoft's never streams. `None` when either is unreadable.
pub(super) fn inbox_display_driver_newer(host_path: &Path) -> Option<bool> {
    let ours = driver_ver(&package_dir(host_path).join(PACKAGE[0]))?;
    let windows = driver_ver(Path::new(&pf_paths::windows_dir(r"INF\rdpidd.inf")))?;
    Some(ver_date(&windows)? >= ver_date(&ours)?)
}

/// `mm/dd/yyyy,…` as `(yyyy, mm, dd)`, so dates compare in order.
fn ver_date(driver_ver: &str) -> Option<(u32, u32, u32)> {
    let date = driver_ver.split(',').next()?;
    let mut parts = date.split('/').map(|part| part.parse::<u32>().ok());
    let (month, day, year) = (parts.next()??, parts.next()??, parts.next()??);
    Some((year, month, day))
}

/// Add the seats package to the driver store. The terminal-services stack makes the devnode
/// when a seat's session starts and picks the newest package then, so none is created here.
/// `install` also moves live devnodes onto it, which restarts a running seat's display: only
/// turning seats on asks for that. 3010 is a driver added that a restart would load.
fn install_driver(dir: &Path, install: bool) -> WinResult<()> {
    let inf = dir.join(PACKAGE[0]);
    let inf = inf.to_string_lossy();
    let args: &[&str] = if install {
        &["/add-driver", &inf, "/install"]
    } else {
        &["/add-driver", &inf]
    };
    match run("pnputil.exe", args) {
        Some(0 | 3010) => Ok(()),
        code => Err(refuse(
            "driver_install",
            "Windows couldn't install the seat display driver.",
            format!("pnputil exited {code:?}"),
        )
        .said("driver_install")),
    }
}

/// Turn the Remote Desktop listener on, noting under the Seats key that it was off.
fn allow_listener(seats: &RegKey) -> WinResult<()> {
    let listener = |cause: std::io::Error| {
        refuse("rdp_listener", "Couldn't turn on Remote Desktop.", cause).said("rdp_listener_on")
    };
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

/// Scope the `Remote Desktop` rule group to `remote`: an address range or `Any`. The Session
/// Host role's listener rules go first, where a machine has them: the last statement sets the
/// exit code, and a lookup that finds no rule fails it.
fn firewall_scope(remote: &str) -> Result<(), String> {
    let script = format!(
        "$ErrorActionPreference='Stop'; \
         Get-NetFirewallRule -Name {RDS_LISTENER_RULES} -ErrorAction SilentlyContinue | \
         Set-NetFirewallRule -RemoteAddress {remote}; \
         Set-NetFirewallRule -Group '{RDP_RULE_GROUP}' -RemoteAddress {remote}"
    );
    match run(POWERSHELL, &powershell_args(&script)) {
        Some(0) => Ok(()),
        code => Err(format!("Set-NetFirewallRule exited {code:?}")),
    }
}

/// The rules [`firewall_scope`] sets, as `name|address,address` lines: what each reaches today.
fn scopes_script() -> String {
    format!(
        "$ErrorActionPreference='Stop'; \
         @(Get-NetFirewallRule -Name {RDS_LISTENER_RULES} -ErrorAction SilentlyContinue) + \
         @(Get-NetFirewallRule -Group '{RDP_RULE_GROUP}') | \
         ForEach-Object {{ '{{0}}|{{1}}' -f $_.Name, \
         (($_ | Get-NetFirewallAddressFilter).RemoteAddress -join ',') }}; exit 0"
    )
}

/// Note each rule's scope, so [`restore_scopes`] writes back what the operator had, even when
/// they narrow the rules in a later turn-on.
fn record_scopes(seats: &RegKey) -> WinResult<()> {
    let fail = |cause: String| {
        refuse(
            "rdp_firewall",
            "Couldn't read Remote Desktop's firewall rules.",
            cause,
        )
        .said("rdp_firewall_read")
    };
    let scopes = powershell_lines(&scopes_script(), QUERY_LIMIT).map_err(fail)?;
    seats
        .set_value(RDP_RULE_SCOPES, &scopes.join("\n"))
        .map_err(|cause| fail(cause.to_string()))
}

/// The `(rule, addresses)` pairs [`record_scopes`] wrote. A line with no address is dropped.
fn parse_scopes(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once('|'))
        .filter(|(name, remote)| !name.is_empty() && !remote.is_empty())
        .map(|(name, remote)| (name.to_owned(), remote.to_owned()))
        .collect()
}

/// Set each recorded rule back to its scope, then drop the note that they are narrowed. A rule
/// the record lacks, one Windows added since, opens to `Any` as Windows creates it; with no
/// record, which an older key lacks, every rule does.
fn restore_scopes(seats: &RegKey) -> WinResult<()> {
    let fail = |cause: String| {
        refuse(
            "rdp_firewall",
            "Couldn't restore Remote Desktop's firewall rules.",
            cause,
        )
        .said("rdp_firewall_restore")
    };
    let kept = parse_scopes(
        &seats
            .get_value::<String, _>(RDP_RULE_SCOPES)
            .unwrap_or_default(),
    );
    powershell_lines(&restore_script(&kept), QUERY_LIMIT).map_err(fail)?;
    match seats.delete_value(RDP_RULES_NARROWED) {
        Err(cause) if cause.kind() != std::io::ErrorKind::NotFound => Err(fail(cause.to_string())),
        _ => Ok(()),
    }
}

fn restore_script(kept: &[(String, String)]) -> String {
    let names: Vec<String> = kept.iter().map(|(name, _)| name.clone()).collect();
    let mut script = String::from("$ErrorActionPreference='Stop'; ");
    for (name, remote) in kept {
        let remote: Vec<String> = remote.split(',').map(str::to_owned).collect();
        script += &format!(
            "Get-NetFirewallRule -Name {} -ErrorAction SilentlyContinue | \
             Set-NetFirewallRule -RemoteAddress {}; ",
            ps_list(std::slice::from_ref(name)),
            ps_list(&remote)
        );
    }
    script += &format!(
        "@(Get-NetFirewallRule -Name {RDS_LISTENER_RULES} -ErrorAction SilentlyContinue) + \
         @(Get-NetFirewallRule -Group '{RDP_RULE_GROUP}' -ErrorAction SilentlyContinue) | \
         Where-Object {{ @({}) -notcontains $_.Name }} | \
         Set-NetFirewallRule -RemoteAddress Any; exit 0",
        ps_list(&names)
    );
    script
}

/// Enable the `Remote Desktop` rules that are off, for an operator who lets the network reach
/// Remote Desktop: the inbound ones on the listener's port, as [`open_rdp_rule`] reads them. The
/// group's other rules, such as Shadow, stay as they are. Turning the listener on by registry
/// enables none. The names are noted first, so a crash between the two still closes them.
fn open_rdp_rules(seats: &RegKey) -> WinResult<()> {
    let fail = |cause: String| {
        refuse(
            "rdp_firewall",
            "Couldn't open Remote Desktop to the network.",
            cause,
        )
        .said("rdp_firewall_open")
    };
    let off = powershell_lines(
        &format!(
            "$ErrorActionPreference='Stop'; \
             Get-NetFirewallRule -Group '{RDP_RULE_GROUP}' | \
             Where-Object {{ $_.Enabled -eq 'False' -and $_.Direction -eq 'Inbound' }} | \
             Where-Object {{ ($_ | Get-NetFirewallPortFilter).LocalPort -contains '3389' }} | \
             ForEach-Object {{ $_.Name }}"
        ),
        QUERY_LIMIT,
    )
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
    powershell_lines(
        &format!(
            "$ErrorActionPreference='Stop'; Enable-NetFirewallRule -Name {}",
            ps_list(&off)
        ),
        QUERY_LIMIT,
    )
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
        .said("rdp_firewall_close")
    };
    powershell_lines(
        &format!(
            "$ErrorActionPreference='Stop'; \
             Get-NetFirewallRule -Name {} -ErrorAction SilentlyContinue | Disable-NetFirewallRule",
            ps_list(&opened)
        ),
        QUERY_LIMIT,
    )
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

/// A PowerShell script's non-empty output lines. `Err` when it fails, doesn't launch or runs
/// past `limit`.
fn powershell_lines(script: &str, limit: Duration) -> Result<Vec<String>, String> {
    let output = pf_paths::run_within(
        Command::new(pf_paths::system32(POWERSHELL))
            .args(powershell_args(script))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
        limit,
    )
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

/// Record the leaf TermService presents, in place of an earlier one: turning seats on is the
/// operator's yes to the certificate as it is now. The keeper observes it, since the handshake
/// needs IronRDP, which the service doesn't link.
fn trust_rdp_certificate(host_path: &Path) -> WinResult<()> {
    let keeper = keeper::keeper_path(host_path);
    if !keeper.is_file() {
        return Err(refuse(
            "keeper_missing",
            "A Punktfunk file is missing. Reinstall Punktfunk, then try again.",
            keeper.display(),
        )
        .said("keeper_missing"));
    }
    let mut cause = String::new();
    for attempt in 0..TRUST_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(TRUST_RETRY);
        }
        match pf_paths::run_within(
            Command::new(&keeper)
                .args(["trust", "--replace"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
            KEEPER_LIMIT,
        ) {
            Ok(output) if output.status.success() => return Ok(()),
            // STATUS_DLL_NOT_FOUND: a fresh Windows Server has no Visual C++ runtime.
            Ok(output) if output.status.code() == Some(0xC000_0135_u32 as i32) => {
                return Err(refuse(
                    "vc_runtime",
                    "The Microsoft Visual C++ runtime is missing. Install the Visual C++ 2015–2022 Redistributable (x64) from Microsoft, then try again.",
                    keeper.display(),
                )
                .said("vc_runtime"));
            }
            Ok(output) => cause = String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            Err(error) => cause = error.to_string(),
        }
    }
    Err(refuse(
        "rdp_trust",
        "Couldn't record this machine's Remote Desktop certificate. Check that Remote Desktop is running, then try again.",
        cause,
    )
    .said("rdp_trust"))
}

/// What a query may take: up to [`QUERY_LIMIT`], and none once `deadline` has passed.
fn time_left(deadline: Instant) -> Option<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    (!left.is_zero()).then_some(left.min(QUERY_LIMIT))
}

fn probe(deadline: Instant) -> Option<(String, [bool; 2])> {
    let output = pf_paths::run_within(
        Command::new(pf_paths::system32(POWERSHELL))
            .args(powershell_args(PROBE))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()),
        time_left(deadline)?,
    )
    .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    tracing::debug!(output = %text.trim(), "seat prerequisite probe");
    parse_probe(&text)
}

/// The last non-empty line: the role's install state, then two `0`/`1` flags.
fn parse_probe(text: &str) -> Option<(String, [bool; 2])> {
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let mut fields = line.split_whitespace();
    let role = fields.next()?.to_string();
    let flags: Vec<bool> = fields.map(|flag| flag == "1").collect();
    Some((role, flags.try_into().ok()?))
}

/// The Session Host role by its `Get-WindowsFeature` install state. A role the probe can't read
/// warns instead of refusing, so a broken cmdlet never blocks a machine that has the role.
fn role_check(state: &str) -> Diagnostic {
    const CODE: &str = "rds_role";
    match state {
        "Installed" => Diagnostic::info(CODE, "Remote Desktop Session Host is installed.")
            .worded("seatcheck_rds_role_ok"),
        RESTART_PENDING => Diagnostic::error("restart", RESTART).worded("seatcheck_restart"),
        "Unknown" => Diagnostic::warning(
            CODE,
            "Couldn't confirm that Remote Desktop Session Host is installed. If it isn't, press Set up Remote Desktop in Profiles → Seats.",
        )
        .worded("seatcheck_rds_role_unknown"),
        _ => Diagnostic::error(
            CODE,
            "Remote Desktop Session Host isn't installed. Press Set up Remote Desktop in Profiles → Seats to install it; Windows then needs a restart.",
        )
        .worded("seatcheck_rds_role_missing"),
    }
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

/// A System32 tool's exit code, output discarded. `None` when it did not launch or ran past
/// [`TOOL_LIMIT`].
pub(super) fn run(tool: &str, args: &[&str]) -> Option<i32> {
    pf_paths::run_within(
        Command::new(pf_paths::system32(tool))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
        TOOL_LIMIT,
    )
    .ok()?
    .status
    .code()
}

/// `ids` are the message ids of `met` and `unmet`.
fn check(ok: bool, code: &str, ids: [&str; 2], met: &str, unmet: &str) -> Diagnostic {
    if ok {
        Diagnostic::info(code, met).worded(ids[0])
    } else {
        Diagnostic::error(code, unmet).worded(ids[1])
    }
}

/// A refusal the operator reads: `sentence` is the answer, `cause` goes to the log. The caller
/// names the sentence's message with [`BackendError::said`].
fn refuse(code: &str, sentence: &str, cause: impl std::fmt::Display) -> BackendError {
    tracing::warn!(code, %cause, "seat change refused");
    backend_error(code, sentence)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::DiagnosticLevel;

    /// A query gets what its deadline leaves, never more than the limit, and none after it.
    #[test]
    fn a_query_gets_what_the_deadline_leaves() {
        let now = Instant::now();
        assert_eq!(time_left(now), None);
        assert_eq!(
            time_left(now + Duration::from_secs(3600)),
            Some(QUERY_LIMIT)
        );
        let short = time_left(now + Duration::from_secs(30)).unwrap();
        assert!(short > Duration::from_secs(20) && short <= Duration::from_secs(30));
    }

    #[test]
    fn only_an_unscoped_enabled_listener_rule_counts_as_open() {
        let rule = |active: &str, extra: &str| {
            format!(
                "v2.33|Action=Allow|Active={active}|Dir=In|Protocol=6|LPort=3389|\
                 App=%SystemRoot%\\system32\\svchost.exe|Svc=termservice|\
                 Name=@FirewallAPI.dll,-28775|{extra}EmbedCtxt=@FirewallAPI.dll,-28752|"
            )
        };
        assert!(open_rdp_rule(&rule("TRUE", "")));
        assert!(!open_rdp_rule(&rule("FALSE", "")));
        assert!(!open_rdp_rule(&rule("TRUE", "RA4=127.0.0.0/255.0.0.0|")));
        // The Session Host role's rule opens the same port from its own group.
        assert!(open_rdp_rule(
            "v2.33|Action=Allow|Active=TRUE|Dir=In|Protocol=6|LPort=3389|Svc=termservice|\
             Name=@FirewallAPI.dll,-28805|EmbedCtxt=@FirewallAPI.dll,-28791|"
        ));
        // The shadow rule names no port, and the web-socket rules sit in another group.
        assert!(!open_rdp_rule(
            "v2.33|Action=Allow|Active=TRUE|Dir=In|Protocol=6|App=RdpSa.exe|\
             EmbedCtxt=@FirewallAPI.dll,-28752|"
        ));
        assert!(!open_rdp_rule(
            "v2.33|Action=Allow|Active=TRUE|Dir=In|Protocol=6|LPort=3389|\
             EmbedCtxt=@FirewallAPI.dll,-28782|"
        ));
    }

    #[test]
    fn the_grace_row_warns_late_and_refuses_at_zero() {
        let level = |days, server| grace_row(days, server).map(|row| row.level);
        assert_eq!(level(Some(81), false), Some(DiagnosticLevel::Info));
        assert_eq!(level(Some(30), false), Some(DiagnosticLevel::Warning));
        assert_eq!(level(Some(0), false), Some(DiagnosticLevel::Error));
        assert_eq!(level(Some(0), true), Some(DiagnosticLevel::Info));
        assert_eq!(level(None, false), None);
    }

    #[test]
    fn a_role_waiting_on_a_restart_asks_for_it() {
        let row = role_check(RESTART_PENDING);
        assert_eq!(
            (row.code.as_str(), row.level),
            ("restart", DiagnosticLevel::Error)
        );
        let waiting = (RESTART_PENDING.to_string(), [false, true]);
        assert_eq!(parse_probe("RestartPending 0 1\r\n"), Some(waiting));
    }

    #[test]
    fn the_probe_answers_on_its_last_line() {
        let installed = ("Installed".to_string(), [false, true]);
        assert_eq!(parse_probe("Installed 0 1\r\n"), Some(installed));
        let unread = ("Unknown".to_string(), [true; 2]);
        assert_eq!(
            parse_probe("Access is denied.\r\nUnknown 1 1\r\n\r\n"),
            Some(unread)
        );
        assert_eq!(parse_probe("Installed 1"), None);
        assert_eq!(parse_probe(""), None);
    }

    #[test]
    fn an_infs_driver_ver_reads_without_spaces_or_comment() {
        let inf = "[Version]\r\nSignature=\"$Windows NT$\"\r\nDriverVer = 10/12/2026, 9.9.1012.257 ; build\r\n";
        assert_eq!(
            inf_driver_ver(inf).as_deref(),
            Some("10/12/2026,9.9.1012.257")
        );
        assert_eq!(inf_driver_ver("[Version]\r\n"), None);
    }

    /// The newer date wins the seat's display, so dates compare year first.
    #[test]
    fn driver_dates_compare_year_first() {
        let ours = ver_date("10/12/2026,9.9.1012.257").unwrap();
        let windows = ver_date("06/21/2006,10.0.26100.7309").unwrap();
        assert!(windows < ours);
        assert!(ver_date("12/31/2025,1.0") < ver_date("01/01/2026,1.0"));
        assert_eq!(ver_date("not a date"), None);
    }

    #[test]
    fn only_an_absent_role_reads_as_not_installed() {
        assert_eq!(role_check("Installed").level, DiagnosticLevel::Info);
        assert_eq!(role_check("Unknown").level, DiagnosticLevel::Warning);
        assert_eq!(role_check("Available").level, DiagnosticLevel::Error);
        assert_eq!(role_check("Removed").level, DiagnosticLevel::Error);
    }

    #[test]
    fn a_rule_goes_back_to_the_scope_it_had_and_an_unknown_one_opens() {
        let kept = parse_scopes(
            "RemoteDesktop-UserMode-In-TCP|10.0.5.0/255.255.255.0,LocalSubnet\n\
             {guid-1}|Any\nno-address|\n|orphan\nnot a record",
        );
        assert_eq!(
            kept,
            [
                (
                    "RemoteDesktop-UserMode-In-TCP".to_owned(),
                    "10.0.5.0/255.255.255.0,LocalSubnet".to_owned()
                ),
                ("{guid-1}".to_owned(), "Any".to_owned()),
            ]
        );
        let script = restore_script(&kept);
        assert!(script.contains(
            "-Name 'RemoteDesktop-UserMode-In-TCP' -ErrorAction SilentlyContinue | \
             Set-NetFirewallRule -RemoteAddress '10.0.5.0/255.255.255.0','LocalSubnet';"
        ));
        assert!(script.contains("@('RemoteDesktop-UserMode-In-TCP','{guid-1}') -notcontains"));
        // With no record every rule opens, as before the scopes were kept.
        assert!(restore_script(&[]).contains("@() -notcontains $_.Name"));
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
