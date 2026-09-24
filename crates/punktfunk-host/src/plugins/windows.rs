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
/// (RX,WA), secrets are (R). Inheritable onto per-plugin subdirs. Users stay
/// read-only (config-dir default).
const RUNNER_STATE_DIRS: [&str; 1] = ["plugin-state"];

/// Ingest inbox: `<config_dir>\ingest`. Inverse of `plugin-state`: `BUILTIN\Users`
/// gets Modify so an interactive-user app can drop `ingest\<plugin>\…` for the
/// LocalService runner to read. The rest of the config tree stays Users-read-only.
/// Any local user can drop a file here (trusted-single-user; the reader is LocalService).
const RUNNER_INGEST_DIRS: [&str; 1] = ["ingest"];

/// `BUILTIN\Users` (S-1-5-32-545) in icacls SID form — the ingest inbox's writer.
const USERS_SID: &str = "*S-1-5-32-545";

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
    // the inherited Users:RX, so the dir reverts to read-only.
    for name in RUNNER_INGEST_DIRS {
        let path = cfg.join(name);
        if !path.exists() {
            continue;
        }
        let _ = Command::new(icacls_path())
            .arg(&path)
            .args(["/remove:g", USERS_SID])
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
    crate::install::sys32(r"WindowsPowerShell\v1.0\powershell.exe")
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

/// The ACE a grant carries: `(RX)` for read, `(M)` for write, both inheritable.
/// Pure so a test pins the string without a real `icacls`.
fn grant_permission(write: bool) -> &'static str {
    if write {
        "(OI)(CI)(M)"
    } else {
        "(OI)(CI)(RX)"
    }
}

/// Grant the runner access to one directory the operator owns, via `icacls /grant:r` so an
/// existing ACE is replaced rather than stacked.
///
/// The Windows runner is `NT AUTHORITY\LocalService`, which holds no ACE anywhere inside a user
/// profile — so a launcher installed there is invisible to every scanner plugin, and reads exactly
/// like one that is not installed. This is that grant, without the operator hand-writing a SID.
///
/// It stays one directory: every service account holds "bypass traverse checking", so the locked
/// parents above the target are never access-checked and the rest of the profile stays shut.
pub(super) fn grant(dir: &std::path::Path, write: bool) -> Result<()> {
    // A typo must not report success — the ACE would land on a name nothing ever reads.
    if !dir.is_dir() {
        bail!(
            "'{}' is not a directory (grant the folder holding the launcher, not the .exe)",
            dir.display()
        );
    }
    let ok = Command::new(icacls_path())
        .arg(dir)
        .args([
            "/grant:r",
            &format!("{LOCAL_SERVICE_SID}:{}", grant_permission(write)),
        ])
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

/// Remove the runner's ACE from one directory: the inverse of [`grant`]. A folder that is gone
/// has nothing left to remove.
pub(super) fn revoke(dir: &std::path::Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let ok = Command::new(icacls_path())
        .arg(dir)
        .args(["/remove:g", LOCAL_SERVICE_SID])
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
    /// Read grants are read-only ACEs; a write grant is the one that carries Modify.
    #[test]
    fn grant_permission_splits_read_from_write() {
        assert_eq!(super::grant_permission(false), "(OI)(CI)(RX)");
        assert_eq!(super::grant_permission(true), "(OI)(CI)(M)");
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

/// Windows has no per-plugin sandbox to turn off; the diagnostic that asks is Linux's.
pub(super) fn runner_sandbox_off() -> bool {
    false
}

/// Nothing to converge: the task sees the whole disk, and a grant is an ACL ([`grant`]).
pub(super) fn converge_runner_roots(
    _roots: &[super::access::RunnerRoot],
    _home: &std::path::Path,
) -> Result<bool> {
    Ok(false)
}

/// Stop then start: there is no `Restart-ScheduledTask`, and Start on a running task is a no-op.
pub(super) fn restart_runtime() -> Result<()> {
    powershell(&format!(
        "Stop-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
         Start-ScheduledTask -TaskName {TASK} -ErrorAction Stop"
    ))
}
