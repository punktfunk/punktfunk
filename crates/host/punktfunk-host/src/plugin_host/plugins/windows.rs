//! The Windows plugin runner: the scheduled task, and the ACL policy on what LocalService may
//! read and write.
//!
//! The runner is a separate, unprivileged principal. It reads the scoped `plugin-token` and the
//! TLS pin, never `mgmt-token`, and it writes only the plugin/script units, their state and the
//! ingest drop — each grant is an explicit `icacls` call listed here rather than a directory it
//! inherits. That policy is the whole reason this file exists: it should be readable in one
//! place instead of found by grepping for a `cfg`.

use super::*;

/// `NT AUTHORITY\LocalService` in icacls SID form.
pub(super) const LOCAL_SERVICE_SID: &str = "*S-1-5-19";

/// Secrets the runner may read: the scoped `plugin-token`, the per-plugin tokens it hands each
/// sandboxed plugin, and the TLS-pin cert (`native-cert.pem` after the identity split, else
/// `cert.pem`). Never `mgmt-token`. Absent files are skipped, so listing both certs is safe.
const RUNNER_SECRET_FILES: [&str; 4] = [
    "plugin-token",
    "plugin-run/plugin-tokens.json",
    "native-cert.pem",
    "cert.pem",
];

/// Directory-bound inputs. The inheritable read grant covers grants files created by atomic
/// rename; the secret token also receives a direct ACE after each protected rewrite.
const RUNNER_INPUT_DIRS: [&str; 1] = [super::RUNNER_DATA_DIR];

/// Unit dirs the runner imports. Inheritable `(RX,WA)`: bun's loader opens unit
/// files with FILE_WRITE_ATTRIBUTES; plain `(RX)` is EPERM on every import. WA
/// can only touch timestamps/readonly bits — `windowsSddlUnsafeReason` treats it
/// as harmless.
const RUNNER_UNIT_DIRS: [&str; 2] = ["plugins", "scripts"];

/// Writable state: `<config_dir>\plugin-state`. Plugins persist under
/// `plugin-state\<name>`, so LocalService needs Modify here — code dirs are
/// (RX,WA), secrets are (R). Inheritable onto per-plugin subdirs. A plugin keeps
/// its settings here, so no other account holds an ACE.
const RUNNER_STATE_DIRS: [&str; 1] = ["plugin-state"];

/// Ingest inbox: `<config_dir>\ingest`. Inverse of `plugin-state`: `BUILTIN\Users`
/// gets Modify so an interactive-user app can drop `ingest\<plugin>\…` for the
/// LocalService runner to read. The one place in the config tree a local account
/// may write (trusted-single-user; the reader is LocalService).
const RUNNER_INGEST_DIRS: [&str; 1] = ["ingest"];

/// `BUILTIN\Users` (S-1-5-32-545) in icacls SID form — the ingest inbox's writer.
const USERS_SID: &str = "*S-1-5-32-545";

/// Present once [`strip_users_read_once`] has run on this install.
const USERS_STRIP_MARKER: &str = "users-read-stripped";

/// Installs before 0.44 made every config subdirectory with an explicit, inheritable
/// `Users:(RX)`; the root's own re-ACL at start does not reach a protected child. Walk the
/// children once. `ingest` keeps its `Users:(M)`, and `emulators\<id>` keeps the Modify the
/// player's session runs on, so that one is not walked.
pub(super) fn strip_users_read_once() {
    let cfg = pf_paths::config_dir();
    let marker = cfg.join(USERS_STRIP_MARKER);
    if marker.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&cfg) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !entry.path().is_dir() || name == "ingest" {
            continue;
        }
        let mut cmd = Command::new(icacls_path());
        cmd.arg(entry.path()).args(["/remove:g", USERS_SID]);
        if name != "emulators" {
            cmd.arg("/T");
        }
        let _ = cmd
            .args(["/C", "/Q"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    if let Err(e) = std::fs::write(&marker, b"") {
        tracing::warn!(path = %marker.display(), error = %e, "users-read strip marker not written");
    }
}

pub(super) fn enable() -> Result<()> {
    // Converge the principal before start: an older task may still be SYSTEM.
    // Idempotent; `-LogonType ServiceAccount` needs no stored password.
    powershell(&format!(
        "$p = New-ScheduledTaskPrincipal -UserId 'LocalService' -LogonType ServiceAccount; \
         Set-ScheduledTask -TaskName {TASK} -Principal $p -ErrorAction Stop | Out-Null"
    ))?;
    grant_runner_secret_reads();
    let _ = std::fs::write(pf_paths::config_dir().join(RUNNER_ACL_MARKER), b"");
    powershell(&format!(
        "Enable-ScheduledTask -TaskName {TASK} -ErrorAction Stop | Out-Null; \
         Start-ScheduledTask -TaskName {TASK} -ErrorAction Stop"
    ))?;
    println!("Plugin runner enabled and started ({TASK}, runs as LocalService).");
    Ok(())
}

pub(super) fn disable() -> Result<()> {
    powershell(&format!(
        "Stop-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
         Disable-ScheduledTask -TaskName {TASK} -ErrorAction Stop | Out-Null"
    ))?;
    revoke_runner_secret_reads();
    println!("Plugin runner stopped and disabled ({TASK}).");
    Ok(())
}

/// Present once the grants below are applied. `serve` applies them only while it is missing, so
/// the plugin tree is re-ACL'd once per install, not on every boot.
const RUNNER_ACL_MARKER: &str = "plugin-run/runner-acl";

/// A fresh install registers and starts the task without `plugins enable`, which leaves the unit
/// and state dirs unreadable to LocalService: every plugin import fails EPERM.
pub(super) fn converge_runner_acls(status: &RuntimeStatus) {
    let marker = pf_paths::config_dir().join(RUNNER_ACL_MARKER);
    if !status.installed || !status.enabled || marker.exists() {
        return;
    }
    grant_runner_secret_reads();
    if let Err(e) = std::fs::write(&marker, b"") {
        tracing::warn!(path = %marker.display(), error = %e, "runner grant marker not written");
    }
}

/// Grant LocalService read on runner inputs. The data directory uses an inheritable ACE so atomic
/// grant-file replacements stay readable; protected credential rewrites reapply a direct ACE.
/// The ingest inbox opens to `BUILTIN\Users` and is closed to seat accounts.
fn grant_runner_secret_reads() {
    let cfg = pf_paths::config_dir();
    for name in RUNNER_INPUT_DIRS {
        let dir = cfg.join(name);
        if !create_runner_dir(&dir) {
            continue;
        }
        if let Err(e) = grant_runner_data_dir(&dir) {
            eprintln!("warning: {e:#}");
        }
    }
    for name in RUNNER_SECRET_FILES {
        let path = cfg.join(name);
        if !path.exists() {
            println!(
                "note: {} does not exist yet (the host writes it on first serve). Start the \
                 host once, then run `punktfunk-host plugins enable` again so the runner can \
                 authenticate.",
                path.display()
            );
            continue;
        }
        if let Err(e) = grant_runner_secret_read(&path) {
            eprintln!(
                "warning: {e:#} - the plugin runner may fail to authenticate to the management \
                 API"
            );
        }
    }
    // Unit dirs: inheritable (RX,WA). Create now so later files inherit rather
    // than needing another `plugins enable`.
    for name in RUNNER_UNIT_DIRS {
        let dir = cfg.join(name);
        if !create_runner_dir(&dir) {
            continue;
        }
        let ok = Command::new(icacls_path())
            .arg(&dir)
            .args(["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(RX,WA)")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "warning: could not grant LocalService read on {} - the runner may fail to \
                 import plugins/scripts from it",
                dir.display()
            );
        }
    }
    for name in RUNNER_STATE_DIRS {
        let dir = cfg.join(name);
        if !create_runner_dir(&dir) {
            continue;
        }
        let ok = Command::new(icacls_path())
            .arg(&dir)
            .args(["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(M)")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "warning: could not grant LocalService write on {} - state-writing plugins \
                 (config/cache) may fail to persist",
                dir.display()
            );
        }
    }
    for name in RUNNER_INGEST_DIRS {
        let dir = cfg.join(name);
        if !create_runner_dir(&dir) {
            continue;
        }
        let ok = Command::new(icacls_path())
            .arg(&dir)
            .args(["/grant:r", &format!("{USERS_SID}:(OI)(CI)(M)")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "warning: could not open the ingest inbox {} for writes - a plugin fed by an \
                 interactive-user app (e.g. playnite) may see no data",
                dir.display()
            );
        }
    }
    deny_seats();
    // `{app}\scripting` is not under the config dir. Same (RX,WA) as the unit
    // dirs: bun opens the entry script with FILE_WRITE_ATTRIBUTES, and the
    // install tree only carries Users:(RX). WA cannot change content.
    if let Some(dir) = runner_bundle_dir() {
        let ok = Command::new(icacls_path())
            .arg(&dir)
            .args(["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(RX,WA)")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            eprintln!(
                "warning: could not grant LocalService read on {} - the plugin runner will not \
                 start (bun exits EPERM on its own entry script)",
                dir.display()
            );
        }
    }
}

/// Deny the seats group ([`pf_seats::windows::SEATS_GROUP`]) on the ingest inbox and on
/// `tray-token`. A seat account is in `BUILTIN\Users`, whose grants would let it replace the
/// owner's Playnite titles and read the box's summary; an explicit deny on the same object
/// outranks that allow. A token minted before the group existed is covered here, a later one at
/// its mint. A deny already there is left alone: `icacls /deny` adds another ACE on every call.
/// A box that never provisioned a seat has no group.
pub(super) fn deny_seats() {
    let Some(sid) = pf_seats::windows::seats_group_sid() else {
        return;
    };
    let cfg = pf_paths::config_dir();
    let inbox = RUNNER_INGEST_DIRS.map(|name| (cfg.join(name), "(OI)(CI)", "M"));
    let token = (cfg.join(crate::mgmt_token::TRAY_FILE), "", "R");
    for (path, inherit, rights) in inbox.into_iter().chain([token]) {
        if !path.exists() {
            continue;
        }
        let ace = format!(
            "\\{}:{inherit}(DENY)({rights})",
            pf_seats::windows::SEATS_GROUP
        );
        let listed = Command::new(icacls_path()).arg(&path).output();
        if listed.is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(&ace)) {
            continue;
        }
        let ok = Command::new(icacls_path())
            .arg(&path)
            .args(["/deny", &format!("*{sid}:{inherit}({rights})")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            tracing::warn!(path = %path.display(), "still open to seat accounts");
        }
    }
}

/// Restore access after `write_secret_file` replaces the token with a protected file. The
/// directory ACE also lets LocalService traverse into the dedicated runner-data directory.
pub(super) fn grant_runner_credential(path: &std::path::Path) -> Result<()> {
    let dir = path
        .parent()
        .context("resolve the runner credential directory")?;
    grant_runner_data_dir(dir)?;
    grant_runner_secret_read(path)
}

pub(super) fn grant_runner_data_dir(dir: &std::path::Path) -> Result<()> {
    let ok = Command::new(icacls_path())
        .arg(dir)
        .args(["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(RX)")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        bail!("grant LocalService read on {}", dir.display());
    }
    Ok(())
}

fn grant_runner_secret_read(path: &std::path::Path) -> Result<()> {
    let ok = Command::new(icacls_path())
        .arg(path)
        .args(["/grant:r", &format!("{LOCAL_SERVICE_SID}:(R)")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        bail!("grant LocalService read on {}", path.display());
    }
    Ok(())
}

/// Create a runner directory that the grants below re-ACL, refusing a reparse point.
///
/// `icacls` follows a junction, so a link planted here before the config dir was hardened would
/// move the grant — `BUILTIN\Users:(M)` for the ingest inbox — onto whatever it points at.
/// `create_private_dir` rejects one and locks the DACL first; the grant then adds its own ACE.
fn create_runner_dir(dir: &std::path::Path) -> bool {
    match pf_paths::create_private_dir(dir) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("warning: {} not created: {e}", dir.display());
            false
        }
    }
}

/// `None` if the exe path cannot be resolved; callers skip the grant rather than fail enable.
fn runner_bundle_dir() -> Option<std::path::PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.join("scripting"))
}

/// Drop the LocalService grants when the runner is switched off. `enable` re-grants.
fn revoke_runner_secret_reads() {
    let cfg = pf_paths::config_dir();
    let _ = std::fs::remove_file(cfg.join(RUNNER_ACL_MARKER));
    for name in RUNNER_SECRET_FILES
        .iter()
        .chain(RUNNER_INPUT_DIRS.iter())
        .chain(RUNNER_UNIT_DIRS.iter())
        .chain(RUNNER_STATE_DIRS.iter())
    {
        let path = cfg.join(name);
        if !path.exists() {
            continue;
        }
        let _ = Command::new(icacls_path())
            .arg(&path)
            .args(["/remove:g", LOCAL_SERVICE_SID])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    // Ingest was granted to Users, not LocalService. Removing that ACE leaves
    // SYSTEM/Administrators alone: nothing drops into the inbox until `enable`. The seats deny
    // goes with it.
    let seats = pf_seats::windows::seats_group_sid().map(|sid| format!("*{sid}"));
    for name in RUNNER_INGEST_DIRS {
        let path = cfg.join(name);
        if !path.exists() {
            continue;
        }
        let mut icacls = Command::new(icacls_path());
        icacls.arg(&path).args(["/remove:g", USERS_SID]);
        if let Some(seats) = &seats {
            icacls.args(["/remove:d", seats]);
        }
        let _ = icacls
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    // Bundle dir is not under `cfg`. Removing the ACE leaves inherited
    // Users:(RX) from Program Files — read-only, not inaccessible.
    if let Some(dir) = runner_bundle_dir().filter(|d| d.exists()) {
        let _ = Command::new(icacls_path())
            .arg(&dir)
            .args(["/remove:g", LOCAL_SERVICE_SID])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// System32 `icacls`, not PATH — same planted-binary rule as [`powershell_path`].
pub(super) fn icacls_path() -> String {
    crate::install::sys32("icacls.exe")
}

/// System32 powershell, not PATH. CreateProcess searches the launching EXE's
/// directory first, so a planted `powershell.exe` beside the host would run
/// with these privileges.
fn powershell_path() -> String {
    crate::install::sys32(pf_paths::POWERSHELL)
}

pub(super) fn powershell(command: &str) -> Result<()> {
    let status = Command::new(powershell_path())
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .status()
        .context("run powershell")?;
    if !status.success() {
        bail!(
            "the {TASK} scheduled task couldn't be changed — is punktfunk installed with the \
             scripting component, and is this prompt elevated?"
        );
    }
    Ok(())
}

pub(super) fn powershell_output(command: &str) -> Option<String> {
    let out = Command::new(powershell_path())
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

// ---- elevation --------------------------------------------------------------------------------

/// Refuse unelevated admin-only ops. Do not self-elevate via UAC: that opens a
/// new console that closes on exit, hiding bun's output.
pub(super) fn require_elevation(what: &str) -> Result<()> {
    if is_elevated() {
        return Ok(());
    }
    // ASCII only: the default Windows console codepage drops em-dashes and arrows.
    bail!(
        "{what} needs administrator rights (the plugins directory under %ProgramData%\\punktfunk \
         and the runner task are admin-owned).\n\nOpen an elevated prompt: Start -> type \
         \"PowerShell\" -> right-click -> Run as administrator, then run this command again."
    )
}

/// Effective local-Administrator membership via `CheckTokenMembership`.
///
/// Not `TokenElevation`: a restricted/SAFER token from an elevated one
/// (`runas /trustlevel:0x20000`) still reports `TokenIsElevated = 1` while
/// Administrators is deny-only.
fn is_elevated() -> bool {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::Security::{
        AllocateAndInitializeSid, CheckTokenMembership, FreeSid, PSID, SID_IDENTIFIER_AUTHORITY,
    };

    // BUILTIN\Administrators, S-1-5-32-544. Spelled out so this does not depend
    // on which windows crate module exports the RID constants.
    const NT_AUTHORITY: SID_IDENTIFIER_AUTHORITY = SID_IDENTIFIER_AUTHORITY {
        Value: [0, 0, 0, 0, 0, 5],
    };
    const BUILTIN_DOMAIN_RID: u32 = 32;
    const ALIAS_RID_ADMINS: u32 = 544;

    let mut admins = PSID::default();
    // SAFETY: AllocateAndInitializeSid is given a valid authority and exactly the 2 sub-authorities
    // its count argument declares (the remaining 6 are the API's required zero padding). On success
    // it yields a valid PSID that we pass to CheckTokenMembership and free on every path below;
    // `None` for the token means "the calling thread's effective token".
    unsafe {
        if AllocateAndInitializeSid(
            &NT_AUTHORITY,
            2,
            BUILTIN_DOMAIN_RID,
            ALIAS_RID_ADMINS,
            0,
            0,
            0,
            0,
            0,
            0,
            &mut admins,
        )
        .is_err()
        {
            return false;
        }
        let mut is_member = ::windows::core::BOOL::default();
        let ok = CheckTokenMembership(Some(HANDLE::default()), admins, &mut is_member).is_ok();
        FreeSid(admins);
        ok && is_member.as_bool()
    }
}

const TASK: &str = "PunktfunkScripting";

pub(super) fn usage_note() {
    eprintln!(
        "    On Windows, `add`/`remove`/`enable`/`disable` need an ELEVATED prompt (the plugins\n    \
         directory and the runner task are admin-owned)."
    );
}

/// Bundled bun needs the runner script path as a leading arg.
pub(super) fn runner_command() -> Result<(std::path::PathBuf, Vec<String>)> {
    let app = std::env::current_exe()
        .context("resolve current exe")?
        .parent()
        .context("resolve install dir")?
        .to_path_buf();
    let bun = app.join("bun").join("bun.exe");
    let runner = app.join("scripting").join("runner-cli.js");
    if !bun.exists() || !runner.exists() {
        bail!(
            "the plugin runner isn't installed (looked for {} and {}) — reinstall punktfunk \
             with the scripting component",
            bun.display(),
            runner.display()
        );
    }
    Ok((bun, vec![runner.to_string_lossy().into_owned()]))
}

/// The runner's account may reach one directory the operator owns. LocalService holds no ACE
/// inside a user profile, so without this a launcher installed there reads as not installed.
pub(super) fn grant(dir: &std::path::Path, write: bool) -> Result<()> {
    grant_to(LOCAL_SERVICE_SID, dir, write)
}

/// One grantee's ACE on one directory, `icacls /grant:r` so an earlier one is replaced, never
/// stacked. The grantee is the runner's account today and a plugin's package SID once each
/// runs in its own AppContainer. One directory only: a service account bypasses traverse
/// checks, so the locked parents above it stay shut.
pub(super) fn grant_to(sid: &str, dir: &std::path::Path, write: bool) -> Result<()> {
    grant_ace(sid, dir, if write { Ace::Modify } else { Ace::Read })
}

/// [`grant_to`] with the ACE spelled out: code dirs take `(RX,WA)`, which is neither.
fn grant_ace(sid: &str, dir: &std::path::Path, ace: Ace) -> Result<()> {
    // A typo must not report success — the ACE would land on a name nothing ever reads.
    if !dir.is_dir() {
        bail!(
            "'{}' is not a directory (grant the folder holding the launcher, not the .exe)",
            dir.display()
        );
    }
    let ok = Command::new(icacls_path())
        .arg(dir)
        .args(["/grant:r", &format!("{sid}:{}", ace.icacls())])
        .status()
        .context("run icacls")?
        .success();
    if !ok {
        bail!(
            "icacls left '{}' unchanged: a folder's permissions are changed by its OWNER or \
             an administrator - run this as the user who owns the folder, or from an elevated \
             prompt",
            dir.display()
        );
    }
    Ok(())
}

/// The inverse of [`grant`].
pub(super) fn revoke(dir: &std::path::Path) -> Result<()> {
    revoke_from(LOCAL_SERVICE_SID, dir)
}

/// Remove one grantee's ACE from one directory. A folder that is gone has nothing to remove.
pub(super) fn revoke_from(sid: &str, dir: &std::path::Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let ok = Command::new(icacls_path())
        .arg(dir)
        .args(["/remove:g", sid])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("run icacls")?
        .success();
    if !ok {
        bail!("remove the runner's ACE from {}", dir.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Only an `HKLM` key becomes a named key; `HKCU` and anything malformed name nothing.
    #[test]
    fn named_key_takes_hklm_only() {
        assert_eq!(
            super::named_key(r"HKLM\SOFTWARE\Valve\Steam").as_deref(),
            Some(r"MACHINE\SOFTWARE\Valve\Steam")
        );
        assert_eq!(
            super::named_key(r"hklm\SOFTWARE\WOW6432Node\Ubisoft").as_deref(),
            Some(r"MACHINE\SOFTWARE\WOW6432Node\Ubisoft")
        );
        for bad in [
            r"HKCU\Software\Valve\Steam",
            r"HKLM\",
            r"HKLM\SOFTWARE\\Valve",
            r"HKLM\SOFTWARE\..\SAM",
            "HKLM\\SOFTWARE\\a\u{7}b",
            r"SOFTWARE\Valve",
        ] {
            assert_eq!(super::named_key(bad), None, "{bad}");
        }
    }
}

pub(super) fn runtime_status() -> RuntimeStatus {
    let out = powershell_output(&format!(
        "$t = Get-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
         if ($null -eq $t) {{ 'missing' }} else {{ \"$($t.State)|$($t.Principal.UserId)\" }}"
    ));
    match out.as_deref().map(str::trim) {
        Some("missing") | None => RuntimeStatus {
            installed: false,
            enabled: false,
            running: false,
            unit: TASK,
            principal: None,
            detail: "reinstall punktfunk with the scripting component to get the plugin runner"
                .into(),
        },
        Some(raw) => {
            let (state, principal) = raw.split_once('|').unwrap_or((raw, ""));
            RuntimeStatus {
                installed: true,
                enabled: !state.eq_ignore_ascii_case("Disabled"),
                running: state.eq_ignore_ascii_case("Running"),
                unit: TASK,
                principal: (!principal.is_empty()).then(|| principal.to_string()),
                detail: String::new(),
            }
        }
    }
}

/// The host's word to the runner that the operator turned the sandbox off: `host.env`'s
/// `PUNKTFUNK_PLUGIN_SANDBOX`, which the runner's task never sees.
const SANDBOX_OFF_MARKER: &str = "sandbox-off";

/// Is `PUNKTFUNK_PLUGIN_SANDBOX` off in `host.env`? Read from the marker the host published,
/// so a CLI process reads the host's answer and not its own environment.
pub(super) fn runner_sandbox_off() -> bool {
    pf_paths::config_dir()
        .join(RUNNER_DATA_DIR)
        .join(SANDBOX_OFF_MARKER)
        .exists()
}

/// Write or remove [`SANDBOX_OFF_MARKER`] from this process's environment. The serving host
/// alone calls it: its environment is `host.env`.
pub(super) fn publish_sandbox_override() {
    let off = pf_host_config::env_on("PUNKTFUNK_PLUGIN_SANDBOX") == Some(false);
    let marker = pf_paths::config_dir()
        .join(RUNNER_DATA_DIR)
        .join(SANDBOX_OFF_MARKER);
    let result = if off {
        tracing::warn!("PUNKTFUNK_PLUGIN_SANDBOX=off — plugins run outside their AppContainers");
        std::fs::write(&marker, b"")
    } else {
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    };
    if let Err(e) = result {
        tracing::warn!(path = %marker.display(), error = %e, "sandbox marker not updated");
    }
}

/// The ACE a placed directory carries.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum Ace {
    /// `(RX)`: a declared read, or a read grant.
    Read,
    /// `(RX,WA)`: code bun loads — its loader opens files with `FILE_WRITE_ATTRIBUTES`.
    Code,
    /// `(M)`: a declared write, a write grant, or the plugin's own state.
    Modify,
    /// `(RX)` on this folder alone: the names in it, none of what they hold. `(RD)` alone
    /// does not open the folder for a listing.
    List,
}

impl Ace {
    fn icacls(self) -> &'static str {
        match self {
            Ace::Read => "(OI)(CI)(RX)",
            Ace::Code => "(OI)(CI)(RX,WA)",
            Ace::Modify => "(OI)(CI)(M)",
            Ace::List => "(RX)",
        }
    }
}

/// What this host placed, by grantee SID: directories with the ACE each carries, and registry
/// keys as `SetNamedSecurityInfoW` spells them. A strip reads this, never a manifest that may
/// have changed since the ACE was placed.
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PlacedAces {
    #[serde(default)]
    grantees: std::collections::BTreeMap<String, Placed>,
}

#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Placed {
    #[serde(default)]
    paths: std::collections::BTreeMap<String, Ace>,
    #[serde(default)]
    keys: std::collections::BTreeSet<String>,
}

/// Under `plugin-run`, beside the grants it mirrors.
const RUNNER_ACES_FILE: &str = "runner-aces.json";

static RECHECK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The next converge places every ACE again, not only the changed ones. The service asks at start:
/// it runs as SYSTEM, which may edit a folder like `WindowsApps` that an admin's CLI may not.
pub(super) fn recheck_next() {
    RECHECK.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Every directory a plugin declared or was granted carries an ACE for the runner's account,
/// read or Modify, and every `HKLM` key it declared a read; what a gone plugin placed is
/// stripped. With the sandbox on, the plugin's package gets the same plus its code, state and
/// inbox: a container passes only an ACE its account and its package both hold, so a root in a
/// user profile needs both. A root under the host's own profile is nobody's `~` and gets
/// nothing; a directory that does not exist yet waits for the next start. After
/// [`recheck_next`] it places every ACE again: an installer that rewrites a folder or key drops
/// the ACE while the file still lists it.
pub(super) fn converge_runner_roots(
    roots: &[access::RunnerRoot],
    home: &std::path::Path,
    manifests: &std::collections::BTreeMap<String, manifest::PluginManifest>,
) -> Result<bool> {
    let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
    let runner = Placed {
        paths: root_aces(roots, &home),
        keys: manifests
            .values()
            .flat_map(|m| m.registry.iter())
            .filter_map(|k| named_key(k))
            .collect(),
    };
    let mut want = PlacedAces {
        grantees: [(
            LOCAL_SERVICE_SID.trim_start_matches('*').to_string(),
            runner,
        )]
        .into(),
    };
    if !runner_sandbox_off() {
        let store = access::AccessStore::open(pf_paths::config_dir());
        for (id, m) in manifests {
            let sid = match crate::windows::app_container::package_sid(id) {
                Ok(sid) => sid,
                Err(e) => {
                    tracing::warn!(plugin = %id, error = %format!("{e:#}"), "no package SID");
                    continue;
                }
            };
            let mut paths = root_aces(&store.plugin_roots(id, m), &home);
            paths.extend(package_dirs(id));
            if m.per_account() {
                paths.extend(profiles_root());
            }
            let keys = m.registry.iter().filter_map(|k| named_key(k)).collect();
            want.grantees.insert(sid, Placed { paths, keys });
        }
    }
    let file = pf_paths::config_dir()
        .join(RUNNER_DATA_DIR)
        .join(RUNNER_ACES_FILE);
    let had: PlacedAces = std::fs::read_to_string(&file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let recheck = RECHECK.swap(false, std::sync::atomic::Ordering::Relaxed);
    if want == had && !recheck {
        return Ok(false);
    }
    let mut placed = had.clone();
    let empty = Placed::default();
    // Stale first: an ACE that outlives its plugin is the leak this file exists to close.
    for (sid, old) in &had.grantees {
        let new = want.grantees.get(sid).unwrap_or(&empty);
        let icacls_sid = format!("*{sid}");
        for path in old.paths.keys().filter(|p| !new.paths.contains_key(*p)) {
            match revoke_from(&icacls_sid, std::path::Path::new(path)) {
                Ok(()) => {
                    placed
                        .grantees
                        .entry(sid.clone())
                        .or_default()
                        .paths
                        .remove(path);
                }
                Err(e) => tracing::warn!(%sid, %path, error = %e, "runner ACE not removed"),
            }
        }
        for key in old.keys.difference(&new.keys) {
            match crate::windows::registry_ace::revoke(key, sid) {
                Ok(()) => {
                    placed
                        .grantees
                        .entry(sid.clone())
                        .or_default()
                        .keys
                        .remove(key);
                }
                Err(e) => {
                    tracing::warn!(%sid, %key, error = %format!("{e:#}"), "key ACE not removed")
                }
            }
        }
    }
    for (sid, new) in &want.grantees {
        let old = had.grantees.get(sid).unwrap_or(&empty);
        let icacls_sid = format!("*{sid}");
        for (path, ace) in &new.paths {
            if !recheck && old.paths.get(path) == Some(ace) {
                continue;
            }
            match grant_ace(&icacls_sid, std::path::Path::new(path), *ace) {
                Ok(()) => {
                    placed
                        .grantees
                        .entry(sid.clone())
                        .or_default()
                        .paths
                        .insert(path.clone(), *ace);
                }
                Err(e) => tracing::warn!(%sid, %path, error = %e, "runner ACE not placed"),
            }
        }
        let keys: Vec<&String> = if recheck {
            new.keys.iter().collect()
        } else {
            new.keys.difference(&old.keys).collect()
        };
        for key in keys {
            match crate::windows::registry_ace::grant_read(key, sid) {
                Ok(()) => {
                    placed
                        .grantees
                        .entry(sid.clone())
                        .or_default()
                        .keys
                        .insert(key.clone());
                }
                // Not recorded either way, so the next converge tries a key that appeared since.
                Err(e) if crate::windows::registry_ace::is_absent(&e) => {
                    tracing::debug!(%sid, %key, "key ACE waits for its key")
                }
                Err(e) => {
                    tracing::warn!(%sid, %key, error = %format!("{e:#}"), "key ACE not placed")
                }
            }
        }
    }
    placed
        .grantees
        .retain(|_, p| !p.paths.is_empty() || !p.keys.is_empty());
    if placed == had {
        return Ok(false);
    }
    pf_paths::replace_file(&file, serde_json::to_string_pretty(&placed)?.as_bytes())
        .with_context(|| format!("replace {}", file.display()))?;
    Ok(true)
}

/// The roots that get an ACE: existing directories outside the host's own profile.
fn root_aces(
    roots: &[access::RunnerRoot],
    home: &std::path::Path,
) -> std::collections::BTreeMap<String, Ace> {
    roots
        .iter()
        .filter(|r| !r.path.starts_with(home) && r.path.is_dir())
        .map(|r| {
            let ace = if r.write { Ace::Modify } else { Ace::Read };
            (r.path.to_string_lossy().into_owned(), ace)
        })
        .collect()
}

/// What a plugin's package needs beyond its roots: the runtime, the runner bundle and the
/// plugin tree it loads code from, its own state, and its inbox when it has one.
fn package_dirs(id: &str) -> std::collections::BTreeMap<String, Ace> {
    let cfg = pf_paths::config_dir();
    let mut dirs = std::collections::BTreeMap::new();
    let mut code = |dir: Option<std::path::PathBuf>| {
        if let Some(d) = dir.filter(|d| d.is_dir()) {
            dirs.insert(d.to_string_lossy().into_owned(), Ace::Code);
        }
    };
    code(
        runner_command()
            .ok()
            .and_then(|(bun, _)| bun.parent().map(Into::into)),
    );
    code(runner_bundle_dir());
    code(Some(cfg.join("plugins")));
    let state = cfg.join("plugin-state").join(id);
    if state.is_dir() || std::fs::create_dir_all(&state).is_ok() {
        dirs.insert(state.to_string_lossy().into_owned(), Ace::Modify);
    }
    let inbox = cfg.join("ingest").join(id);
    if inbox.is_dir() {
        dirs.insert(inbox.to_string_lossy().into_owned(), Ace::Read);
    }
    dirs
}

/// `%SystemDrive%\Users`, list only: a per-account source finds each profile's folder there
/// and asks for the one it reads, as the runner's account could before the container.
fn profiles_root() -> Option<(String, Ace)> {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let users = std::path::PathBuf::from(format!(r"{drive}\Users"));
    users
        .is_dir()
        .then(|| (users.to_string_lossy().into_owned(), Ace::List))
}

/// `HKLM\SOFTWARE\…` as `SetNamedSecurityInfoW` names it: `MACHINE\SOFTWARE\…`. An `HKCU` key
/// is one account's and gets no ACE for the service; anything else is not a key.
fn named_key(declared: &str) -> Option<String> {
    let rest = declared
        .get(..5)
        .filter(|p| p.eq_ignore_ascii_case("HKLM\\"))
        .map(|_| &declared[5..])?;
    let clean = !rest.is_empty()
        && rest.len() <= 255
        && !rest.chars().any(char::is_control)
        && rest
            .split('\\')
            .all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains(['/', '*', '?']));
    clean.then(|| format!("MACHINE\\{rest}"))
}

/// Stop then start: there is no `Restart-ScheduledTask`, and Start on a running task is a no-op.
pub(super) fn restart_runtime() -> Result<()> {
    powershell(&format!(
        "Stop-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
         Start-ScheduledTask -TaskName {TASK} -ErrorAction Stop"
    ))
}
