//! The Windows plugin runner: the scheduled task, and the ACL policy on what LocalService may
//! read and write.
//!
//! The runner is a separate, unprivileged principal. It reads the scoped `plugin-token` and the
//! TLS pin, never `mgmt-token`, and it writes only the plugin/script units, their state and the
//! ingest drop — each grant is an explicit `icacls` call listed here rather than a directory it
//! inherits. That policy is the whole reason this file exists: it should be readable in one
//! place instead of found by grepping for a `cfg`.

use super::ace_plan::{self, Aces};
use super::*;
use crate::windows::folder_ace;

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
        let mut args = vec!["/remove:g", USERS_SID];
        if name != "emulators" {
            args.push("/T");
        }
        args.extend(["/C", "/Q"]);
        icacls(&entry.path(), &args);
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
/// The ingest inbox opens to `BUILTIN\Users` and is closed to seat accounts. Warnings stay ASCII:
/// `plugins enable` prints them on a console whose codepage drops an em dash.
fn grant_runner_secret_reads() {
    let cfg = pf_paths::config_dir();
    for name in RUNNER_INPUT_DIRS {
        let dir = cfg.join(name);
        if !create_runner_dir(&dir) {
            continue;
        }
        if let Err(e) = grant_runner_data_dir(&dir) {
            tracing::warn!(error = %format!("{e:#}"), "runner data directory grant failed");
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
            tracing::warn!(
                error = %format!("{e:#}"),
                "runner secret grant failed - the plugin runner may not authenticate"
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
        let ok = icacls(
            &dir,
            &["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(RX,WA)")],
        );
        if !ok {
            tracing::warn!(
                dir = %dir.display(),
                "LocalService read not granted - the runner may not import plugins or scripts"
            );
        }
    }
    for name in RUNNER_STATE_DIRS {
        let dir = cfg.join(name);
        if !create_runner_dir(&dir) {
            continue;
        }
        let ok = icacls(
            &dir,
            &["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(M)")],
        );
        if !ok {
            tracing::warn!(
                dir = %dir.display(),
                "LocalService write not granted - plugins may not persist their state"
            );
        }
    }
    for name in RUNNER_INGEST_DIRS {
        let dir = cfg.join(name);
        if !create_runner_dir(&dir) {
            continue;
        }
        let ok = icacls(&dir, &["/grant:r", &format!("{USERS_SID}:(OI)(CI)(M)")]);
        if !ok {
            tracing::warn!(
                dir = %dir.display(),
                "ingest inbox not opened for writes - a plugin fed by a desktop app may see no data"
            );
        }
    }
    deny_seats();
    // `{app}\scripting` is not under the config dir. Same (RX,WA) as the unit
    // dirs: bun opens the entry script with FILE_WRITE_ATTRIBUTES, and the
    // install tree only carries Users:(RX). WA cannot change content.
    if let Some(dir) = runner_bundle_dir() {
        let ok = icacls(
            &dir,
            &["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(RX,WA)")],
        );
        if !ok {
            tracing::warn!(
                dir = %dir.display(),
                "LocalService read not granted on the runner bundle - the plugin runner will not start"
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
        let ok = icacls(&path, &["/deny", &format!("*{sid}:{inherit}({rights})")]);
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
    let ok = icacls(
        dir,
        &["/grant:r", &format!("{LOCAL_SERVICE_SID}:(OI)(CI)(RX)")],
    );
    if !ok {
        bail!("grant LocalService read on {}", dir.display());
    }
    Ok(())
}

fn grant_runner_secret_read(path: &std::path::Path) -> Result<()> {
    let ok = icacls(path, &["/grant:r", &format!("{LOCAL_SERVICE_SID}:(R)")]);
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
            tracing::warn!(dir = %dir.display(), error = %e, "runner directory not created");
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
        icacls(&path, &["/remove:g", LOCAL_SERVICE_SID]);
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
        let mut args = vec!["/remove:g", USERS_SID];
        if let Some(seats) = &seats {
            args.extend(["/remove:d", seats.as_str()]);
        }
        icacls(&path, &args);
    }
    // Bundle dir is not under `cfg`. Removing the ACE leaves inherited
    // Users:(RX) from Program Files — read-only, not inaccessible.
    if let Some(dir) = runner_bundle_dir().filter(|d| d.exists()) {
        icacls(&dir, &["/remove:g", LOCAL_SERVICE_SID]);
    }
}

/// System32 `icacls`, not PATH — same planted-binary rule as [`powershell_path`].
pub(super) fn icacls_path() -> String {
    crate::install::sys32("icacls.exe")
}

/// `icacls <path> <args>` with its output dropped. `true` when it ran and reported success.
fn icacls(path: &std::path::Path, args: &[&str]) -> bool {
    let path = path.to_string_lossy();
    let mut all = vec![path.as_ref()];
    all.extend_from_slice(args);
    crate::install::run_quiet("icacls", &all)
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

/// The runner's account, and `plugin`'s files capability with the sandbox on, may reach one
/// directory the operator owns: both ACEs in one walk of its tree, which the next converge
/// adopts rather than walking again. LocalService holds no ACE inside a user profile, so
/// without this a launcher installed there reads as not installed.
pub(super) fn grant(
    plugin: Option<&str>,
    dir: &std::path::Path,
    write: bool,
    files: bool,
) -> Result<()> {
    let ace = Ace::of(write, files, false);
    let sids = decision_sids(plugin);
    let aces: Vec<(String, Ace)> = sids.iter().map(|sid| (sid.clone(), ace)).collect();
    grant_aces(dir, &aces, plugin)?;
    note(dir, &sids, Some(ace));
    Ok(())
}

/// The inverse of [`grant`] for a folder no plugin holds any more, in one walk.
pub(super) fn revoke(plugin: &str, dir: &std::path::Path, files: bool) -> Result<()> {
    let sids = decision_sids(Some(plugin));
    let ace = Ace::of(false, files, false);
    let aces: Vec<(String, Ace)> = sids.iter().map(|sid| (sid.clone(), ace)).collect();
    revoke_sids(dir, &aces, Some(plugin))?;
    note(dir, &sids, None);
    Ok(())
}

/// The runner's SID, and `plugin`'s files capability with the sandbox on, without icacls' `*`.
fn decision_sids(plugin: Option<&str>) -> Vec<String> {
    let mut sids = vec![LOCAL_SERVICE_SID.trim_start_matches('*').to_string()];
    if let Some(id) = plugin.filter(|_| !runner_sandbox_off()) {
        sids.push(crate::windows::app_container::files_capability_sid(id));
    }
    sids
}

/// SIDs by how their ACE is placed: a folder's own files, a link, and the rest, which share one
/// walk of the tree.
fn by_reach(aces: &[(String, Ace)]) -> (Vec<&str>, Vec<&str>, Vec<&(String, Ace)>) {
    let (mut own, mut links, mut tree) = (Vec::new(), Vec::new(), Vec::new());
    for pair in aces {
        match pair.1.reach() {
            Reach::Files => own.push(pair.0.as_str()),
            Reach::Link => links.push(pair.0.as_str()),
            Reach::Tree => tree.push(pair),
        }
    }
    (own, links, tree)
}

/// Each `(SID, ACE)` on one directory, in a single `icacls /grant:r` for what walks the tree: an
/// earlier ACE for a SID is replaced, never stacked, and the tree below is walked once however
/// many SIDs share it. One directory only: a service account bypasses traverse checks, so the
/// locked parents above it stay shut. `plugin` names who the walk is listed for.
fn grant_aces(dir: &std::path::Path, aces: &[(String, Ace)], plugin: Option<&str>) -> Result<()> {
    // A typo must not report success — the ACE would land on a name nothing ever reads.
    if !dir.is_dir() {
        bail!(
            "'{}' is not a directory (grant the folder holding the launcher, not the .exe)",
            dir.display()
        );
    }
    let owner = |detail: String| {
        anyhow::anyhow!(
            "'{}' is unchanged: a folder's permissions are changed by its OWNER or an \
             administrator - run this as the user who owns the folder, or from an elevated \
             prompt{detail}",
            dir.display()
        )
    };
    let (own, links, tree) = by_reach(aces);
    if !own.is_empty() {
        folder_ace::place(dir, &own).map_err(|e| owner(format!(": {e:#}")))?;
    }
    if !links.is_empty() {
        folder_ace::place_link(dir, &links).map_err(|e| owner(format!(": {e:#}")))?;
    }
    if tree.is_empty() {
        return Ok(());
    }
    #[cfg(test)]
    WALKS.with(|w| w.set(w.get() + 1));
    let _walk = walks::begin(plugin, &dir.to_string_lossy(), false);
    let ok = Command::new(icacls_path())
        .arg(dir)
        .arg("/grant:r")
        .args(
            tree.iter()
                .map(|(sid, ace)| format!("*{sid}:{}", ace.icacls())),
        )
        .stdout(std::process::Stdio::null())
        .status()
        .context("run icacls")?
        .success();
    if !ok {
        return Err(owner(String::new()));
    }
    Ok(())
}

/// Every ACE of each `(SID, kind)` off one directory: what walks the tree in one walk. A folder
/// that is gone has nothing to remove; a link whose target is gone is still a link.
fn revoke_sids(dir: &std::path::Path, aces: &[(String, Ace)], plugin: Option<&str>) -> Result<()> {
    if std::fs::symlink_metadata(dir).is_err() {
        return Ok(());
    }
    let (own, links, tree) = by_reach(aces);
    if !own.is_empty() {
        folder_ace::take(dir, &own)
            .with_context(|| format!("remove the runner's ACE from {}", dir.display()))?;
    }
    if !links.is_empty() {
        folder_ace::take_link(dir, &links)
            .with_context(|| format!("remove the runner's ACE from {}", dir.display()))?;
    }
    if tree.is_empty() {
        return Ok(());
    }
    #[cfg(test)]
    WALKS.with(|w| w.set(w.get() + 1));
    let _walk = walks::begin(plugin, &dir.to_string_lossy(), true);
    let ok = Command::new(icacls_path())
        .arg(dir)
        .arg("/remove:g")
        .args(tree.iter().map(|(sid, _)| format!("*{sid}")))
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
thread_local! {
    /// Folder ACL changes on this thread, each one walk of a folder's tree.
    static WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// A converge on this thread walked a folder itself since the last [`take_walked`].
    static CONVERGE_WALKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether a converge on this thread placed or removed a folder ACE itself since the last ask.
pub(super) fn take_walked() -> bool {
    CONVERGE_WALKED.with(|w| w.replace(false))
}

/// What a grant or revoke already did to a folder, for the next converge to record rather than
/// walk again. `ace` is `None` for a removal; the path is spelled as the converge spells roots.
struct Noted {
    path: String,
    sid: String,
    ace: Option<Ace>,
    at: std::time::Instant,
}

static NOTED: std::sync::Mutex<Vec<Noted>> = std::sync::Mutex::new(Vec::new());

/// A note no converge used in this long is dropped: by then the converge has its own record.
const NOTE_TTL: std::time::Duration = std::time::Duration::from_secs(600);

fn note(dir: &std::path::Path, sids: &[String], ace: Option<Ace>) {
    let path = dir
        .canonicalize()
        .unwrap_or_else(|_| dir.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let mut noted = NOTED.lock().unwrap_or_else(|poison| poison.into_inner());
    noted.retain(|n| !(n.path == path && sids.contains(&n.sid)));
    noted.extend(sids.iter().map(|sid| Noted {
        path: path.clone(),
        sid: sid.clone(),
        ace,
        at: std::time::Instant::now(),
    }));
}

/// Whether `path`'s own DACL already gives `sid` what `ace` grants: one descriptor read, where
/// placing it again would walk the whole tree. A link is read as itself, not followed.
fn held(path: &str, sid: &str, ace: Ace) -> bool {
    use ::windows::Win32::Security::Authorization::SE_FILE_OBJECT;
    let rights = if ace == Ace::Link {
        folder_ace::link_rights(std::path::Path::new(path), sid)
    } else {
        crate::windows::registry_ace::rights(path, SE_FILE_OBJECT, sid)
    };
    rights.is_ok_and(|mask| mask & ace.mask() == ace.mask())
}

#[cfg(test)]
mod tests {
    /// An `HKLM` key names itself once, an `HKCU` key one per loaded account, anything
    /// malformed nothing.
    #[test]
    fn named_keys_take_the_machine_and_each_account() {
        let sids = [
            "S-1-5-21-1-2-3-1001".to_string(),
            "S-1-12-1-4-5".to_string(),
        ];
        let named = |k| super::named_keys_for(k, &sids);
        assert_eq!(
            named(r"HKLM\SOFTWARE\Valve\Steam"),
            [r"MACHINE\SOFTWARE\Valve\Steam"]
        );
        assert_eq!(
            named(r"hklm\SOFTWARE\WOW6432Node\Ubisoft"),
            [r"MACHINE\SOFTWARE\WOW6432Node\Ubisoft"]
        );
        assert_eq!(
            named(r"HKCU\System\GameConfigStore"),
            [
                r"USERS\S-1-5-21-1-2-3-1001\System\GameConfigStore",
                r"USERS\S-1-12-1-4-5\System\GameConfigStore",
            ]
        );
        for bad in [
            r"HKLM\",
            r"HKCU\Software\..\..\S-1-5-18",
            r"HKLM\SOFTWARE\\Valve",
            r"HKLM\SOFTWARE\..\SAM",
            "HKLM\\SOFTWARE\\a\u{7}b",
            r"HKU\S-1-5-18\Software",
            r"SOFTWARE\Valve",
        ] {
            assert!(named(bad).is_empty(), "{bad}");
        }
    }

    /// A folder's SIDs share one walk; what a grant noted, or the folder still carries on a
    /// recheck, costs none; an ACE an installer stripped is placed again. Writes ACLs on a
    /// scratch tree, so it runs on a Windows box with `--ignored`.
    #[test]
    #[ignore = "edits ACLs with icacls; run on a Windows box"]
    fn a_folder_takes_one_walk_and_a_recheck_none() {
        use super::{converge_folders, held, note, Ace, Placed, PlacedAces, WALKS};
        let tmp = tempfile::tempdir().unwrap();
        for d in 0..20 {
            let sub = tmp.path().join(format!("d{d}"));
            std::fs::create_dir_all(&sub).unwrap();
            for f in 0..20 {
                std::fs::write(sub.join(format!("f{f}")), b"x").unwrap();
            }
        }
        let dir = tmp.path().canonicalize().unwrap();
        let path = dir.to_string_lossy().into_owned();
        let deep = dir.join("d19").join("f19").to_string_lossy().into_owned();
        let runner = super::LOCAL_SERVICE_SID.trim_start_matches('*').to_string();
        // Any well-formed capability SID: an ACE needs no container that holds it.
        let pkg = "S-1-15-3-1024-1-2-3-4-5-6-7-8".to_string();
        let walks = || WALKS.with(std::cell::Cell::get);
        let aces = |ace: Ace| PlacedAces {
            grantees: [&runner, &pkg]
                .into_iter()
                .map(|sid| {
                    let paths = [(path.clone(), ace)].into();
                    let keys = Default::default();
                    (sid.clone(), Placed { paths, keys })
                })
                .collect(),
        };
        let none = PlacedAces::default();
        let want = aces(Ace::Read);
        assert!(!held(&path, &pkg, Ace::Read));

        let start = walks();
        let mut placed = none.clone();
        converge_folders(&want, &none, &mut placed, false);
        assert_eq!(walks() - start, 1, "both SIDs in one walk");
        assert!(placed == want);
        assert!(held(&path, &pkg, Ace::Read) && held(&path, &runner, Ace::Read));
        assert!(held(&deep, &pkg, Ace::Read), "the tree below inherited it");
        assert!(!held(&path, &pkg, Ace::Modify));

        let start = walks();
        let mut again = want.clone();
        converge_folders(&want, &want, &mut again, true);
        assert_eq!(
            walks() - start,
            0,
            "a recheck reads the folder, it does not walk it"
        );
        assert!(again == want);

        // A grant already placed them: the converge records without a walk.
        let start = walks();
        note(&dir, &[runner.clone(), pkg.clone()], Some(Ace::Read));
        let mut adopted = none.clone();
        converge_folders(&want, &none, &mut adopted, false);
        assert_eq!(walks() - start, 0);
        assert!(adopted == want);

        // An installer stripped the capability's ACE: the recheck sees it and walks once.
        let strip =
            |sid: &String| super::revoke_sids(&dir, &[(sid.clone(), Ace::Read)], None).unwrap();
        strip(&pkg);
        let start = walks();
        let mut healed = want.clone();
        converge_folders(&want, &want, &mut healed, true);
        assert_eq!(walks() - start, 1);
        assert!(held(&path, &pkg, Ace::Read));

        // A folder on the refused list costs a verify no walk, stripped or not.
        strip(&pkg);
        let refused = || super::REFUSED.lock().unwrap();
        refused().insert((pkg.clone(), path.clone()));
        let start = walks();
        let mut skipped = want.clone();
        converge_folders(&want, &want, &mut skipped, true);
        assert_eq!(walks() - start, 0);
        assert!(!held(&path, &pkg, Ace::Read));
        refused().clear();
        converge_folders(&want, &want, &mut skipped, true);
        assert!(held(&path, &pkg, Ace::Read));

        // Both come off in one walk, and a noted removal costs none.
        let start = walks();
        let mut gone = want.clone();
        converge_folders(&none, &want, &mut gone, false);
        assert_eq!(walks() - start, 1);
        assert!(!held(&path, &pkg, Ace::Read));
        let start = walks();
        note(&dir, &[runner.clone(), pkg.clone()], None);
        let mut dropped = want.clone();
        converge_folders(&none, &want, &mut dropped, false);
        assert_eq!(walks() - start, 0);
        assert!(dropped.grantees.values().all(|p| p.paths.is_empty()));
    }

    /// A folder's own ACE reaches its files and stops; a private folder inside keeps its DACL
    /// through a tree grant. Writes ACLs on a scratch tree, so it runs with `--ignored`.
    #[test]
    #[ignore = "edits ACLs; run on a Windows box"]
    fn a_files_ace_stops_at_the_folder_and_a_tree_ace_at_protection() {
        use super::{converge_folders, held, Ace, Placed, PlacedAces};
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let spell = |p: std::path::PathBuf| p.to_string_lossy().into_owned();
        let (path, own) = (spell(dir.clone()), spell(dir.join("appmanifest_1.acf")));
        let (sub, deep) = (dir.join("common"), dir.join("common").join("f"));
        let locked = dir.join("locked");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(&own, b"x").unwrap();
        std::fs::write(&deep, b"x").unwrap();
        let cut = std::process::Command::new(super::icacls_path())
            .arg(&locked)
            .args(["/inheritance:d", "/Q"])
            .output()
            .unwrap();
        assert!(cut.status.success());
        let (sub, deep, locked) = (spell(sub), spell(deep), spell(locked));
        let pkg = "S-1-15-3-1024-1-2-3-4-5-6-7-9".to_string();
        let aces = |ace: Ace| PlacedAces {
            grantees: [(
                pkg.clone(),
                Placed {
                    paths: [(path.clone(), ace)].into(),
                    keys: Default::default(),
                },
            )]
            .into(),
        };
        let none = PlacedAces::default();
        let files = aces(Ace::Files);
        let mut placed = none.clone();
        converge_folders(&files, &none, &mut placed, false);
        assert!(held(&path, &pkg, Ace::Read) && held(&own, &pkg, Ace::Read));
        let late = spell(dir.join("appmanifest_2.acf"));
        std::fs::write(&late, b"x").unwrap();
        assert!(held(&late, &pkg, Ace::Read), "a new file inherits it");
        assert!(!held(&sub, &pkg, Ace::Read) && !held(&deep, &pkg, Ace::Read));

        // Widened to the tree: everything below takes it, except what is protected.
        let tree = aces(Ace::Read);
        let mut widened = files.clone();
        converge_folders(&tree, &files, &mut widened, false);
        assert!(held(&deep, &pkg, Ace::Read));
        assert!(!held(&locked, &pkg, Ace::Read));

        // Narrowed back, the tree loses it again.
        let mut narrowed = tree.clone();
        converge_folders(&files, &tree, &mut narrowed, false);
        assert!(held(&own, &pkg, Ace::Read) && !held(&deep, &pkg, Ace::Read));

        let mut gone = files.clone();
        converge_folders(&none, &files, &mut gone, false);
        assert!(!held(&path, &pkg, Ace::Read) && !held(&own, &pkg, Ace::Read));
        assert!(!held(&late, &pkg, Ace::Read), "what it inherited goes too");
    }

    /// A link's ACE lands on the link object, not on what it points at, and comes off alone.
    /// Makes a junction in a scratch tree, so `--ignored`.
    #[test]
    #[ignore = "edits ACLs; run on a Windows box"]
    fn a_link_ace_lands_on_the_link_not_its_target() {
        use super::{held, Ace};
        use crate::windows::folder_ace;
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = tmp.path().join("link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
        let pkg = "S-1-15-3-1024-1-2-3-4-5-6-7-10";
        let spell = |p: &std::path::Path| p.to_string_lossy().into_owned();
        folder_ace::place_link(&link, &[pkg]).unwrap();
        assert!(held(&spell(&link), pkg, Ace::Link));
        assert!(
            !held(&spell(&target), pkg, Ace::Read),
            "the target is untouched"
        );
        folder_ace::take_link(&link, &[pkg]).unwrap();
        assert!(!held(&spell(&link), pkg, Ace::Link));
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
    /// `(OI)(NP)(RX)`: this folder and the files directly in it, and no folder below. Placed
    /// with no walk; a file made here later inherits it.
    Files,
    /// `(RX)` on a junction or symbolic link itself, never followed: what the runner opens
    /// through on its way to a root. Placed by handle; a link has nothing below it.
    Link,
}

/// How an ACE is placed, which is also how far it reaches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// icacls, one walk of the tree.
    Tree,
    /// `SetFileSecurityW` on the folder and each file in it.
    Files,
    /// A handle on the link object.
    Link,
}

impl Ace {
    fn of(write: bool, files: bool, link: bool) -> Self {
        match (write, files, link) {
            (_, _, true) => Ace::Link,
            (true, _, false) => Ace::Modify,
            (false, true, false) => Ace::Files,
            (false, false, false) => Ace::Read,
        }
    }

    fn reach(self) -> Reach {
        match self {
            Ace::Read | Ace::Code | Ace::Modify | Ace::List => Reach::Tree,
            Ace::Files => Reach::Files,
            Ace::Link => Reach::Link,
        }
    }

    /// The ACE as icacls spells it. A link never reaches icacls, see [`by_reach`].
    fn icacls(self) -> &'static str {
        match self {
            Ace::Read => "(OI)(CI)(RX)",
            Ace::Code => "(OI)(CI)(RX,WA)",
            Ace::Modify => "(OI)(CI)(M)",
            Ace::List | Ace::Link => "(RX)",
            Ace::Files => "(OI)(NP)(RX)",
        }
    }

    /// The access mask icacls writes for [`Ace::icacls`]: `RX` is FILE_GENERIC_READ |
    /// FILE_GENERIC_EXECUTE, `WA` adds FILE_WRITE_ATTRIBUTES, `M` is Modify.
    fn mask(self) -> u32 {
        match self {
            Ace::Read | Ace::List | Ace::Files | Ace::Link => folder_ace::READ_MASK,
            Ace::Code => 0x0012_01A9,
            Ace::Modify => 0x0013_01BF,
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
static VERIFY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The `(SID, path)` grants Windows refused this run, or that a placing left unseen on the folder.
/// Neither is walked again before a recheck.
static REFUSED: std::sync::Mutex<std::collections::BTreeSet<(String, String)>> =
    std::sync::Mutex::new(std::collections::BTreeSet::new());

/// The next converge checks every folder ACE on the folder itself and places any it lacks, tries
/// a refused folder again, and places every key ACE again. The service asks at start: it runs as
/// SYSTEM, which may edit a folder like `WindowsApps` that an admin's CLI may not.
pub(super) fn recheck_next() {
    RECHECK.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// The next converge checks every folder ACE on the folder itself and places any it lacks: one
/// descriptor read per ACE, cheap enough to run every minute.
pub(super) fn verify_next() {
    VERIFY.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Every directory a plugin declared or was granted carries an ACE for the runner's account,
/// read or Modify, and every `HKLM` key it declared a read; what a gone plugin placed is
/// stripped. With the sandbox on, the plugin's files capability gets the same plus its code,
/// state and inbox: a container passes only an ACE its account and its capability both hold, so
/// a root in a user profile needs both. An ACE an older host placed for the package SID is in
/// the record and comes off here. A root under the host's own profile is nobody's `~` and gets
/// nothing; a directory that does not exist yet waits for the next start. The record alone
/// can't see an ACE that went from its folder, so [`verify_next`] and [`recheck_next`] read the
/// folders: a plugin without the ACE on its own code can't start.
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
            .flat_map(|k| named_keys(k))
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
            let sid = crate::windows::app_container::files_capability_sid(id);
            let mut paths = root_aces(&store.plugin_roots(id, m), &home);
            paths.extend(package_dirs(id));
            if m.per_account() {
                paths.extend(profiles_root());
            }
            let keys = m.registry.iter().flat_map(|k| named_keys(k)).collect();
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
    if recheck {
        REFUSED
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
    }
    let verify = VERIFY.swap(false, std::sync::atomic::Ordering::Relaxed) || recheck;
    if want == had && !verify {
        return Ok(false);
    }
    let mut placed = had.clone();
    let empty = Placed::default();
    converge_folders(&want, &had, &mut placed, verify);
    // Stale keys first: an ACE that outlives its plugin is the leak this file exists to close.
    for (sid, old) in &had.grantees {
        let new = want.grantees.get(sid).unwrap_or(&empty);
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
                // A signed-out account's hive is unloaded and keeps its ACE: kept on record so
                // the next converge after sign-in takes it, or holds it, as it should.
                Err(e) if crate::windows::registry_ace::is_absent(&e) => {
                    tracing::debug!(%sid, %key, "key ACE waits for its hive")
                }
                Err(e) => {
                    tracing::warn!(%sid, %key, error = %format!("{e:#}"), "key ACE not removed")
                }
            }
        }
    }
    for (sid, new) in &want.grantees {
        let old = had.grantees.get(sid).unwrap_or(&empty);
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

/// Move every folder ACE in `placed` from `had` to `want`, one walk per folder and verb: an
/// inheritable ACE makes Windows rewrite every file below, so a folder's SIDs share the walk.
/// What a grant or revoke already did is recorded without one. With `verify`, a recorded ACE
/// counts only if the folder's own DACL still carries it.
fn converge_folders(want: &PlacedAces, had: &PlacedAces, placed: &mut PlacedAces, verify: bool) {
    let paths = |p: &PlacedAces| -> Aces<Ace> {
        p.grantees
            .iter()
            .map(|(sid, g)| (sid.clone(), g.paths.clone()))
            .collect()
    };
    let (want_paths, had_paths) = (paths(want), paths(had));
    let noted = std::mem::take(&mut *NOTED.lock().unwrap_or_else(|poison| poison.into_inner()));
    let noted_ace = |sid: &str, path: &str| {
        noted
            .iter()
            .find(|n| n.sid == sid && n.path == path)
            .map(|n| n.ace)
    };
    let plan = ace_plan::plan(
        &want_paths,
        &had_paths,
        |sid, path, ace| {
            if noted_ace(sid, path) == Some(Some(ace)) {
                return true;
            }
            let same = had_paths.get(sid).and_then(|p| p.get(path)) == Some(&ace);
            same && (!verify || held(path, sid, ace))
        },
        |sid, path| noted_ace(sid, path) == Some(None),
    );
    // A note this pass had no use for may belong to a grant whose record lands after it.
    let used = |n: &Noted| match n.ace {
        Some(ace) => plan
            .kept
            .iter()
            .any(|(s, p, a)| *s == n.sid && *p == n.path && *a == ace),
        None => plan
            .dropped
            .iter()
            .any(|(s, p)| *s == n.sid && *p == n.path),
    };
    let left: Vec<Noted> = noted
        .into_iter()
        .filter(|n| !used(n) && n.at.elapsed() < NOTE_TTL)
        .collect();
    NOTED
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .extend(left);

    for (sid, path, ace) in &plan.kept {
        record(placed, sid, path, Some(*ace));
    }
    for (sid, path) in &plan.dropped {
        record(placed, sid, path, None);
    }
    let had_ace = |sid: &str, path: &str| had_paths.get(sid).and_then(|p| p.get(path)).copied();
    let mut refused = REFUSED.lock().unwrap_or_else(|poison| poison.into_inner());
    for step in plan.steps {
        let dir = std::path::Path::new(&step.path);
        let was = |sid: &String| had_ace(sid, &step.path).map(|ace| (sid.clone(), ace));
        // A folder's own files, a link and a tree are different ACEs: a change of kind undoes
        // the old.
        let changed = step
            .place
            .iter()
            .filter_map(|(sid, ace)| was(sid).filter(|(_, old)| old.reach() != ace.reach()));
        let gone: Vec<(String, Ace)> = step.remove.iter().filter_map(was).chain(changed).collect();
        if !gone.is_empty() {
            CONVERGE_WALKED.with(|w| w.set(true));
            match revoke_sids(dir, &gone, None) {
                Ok(()) => {
                    for (sid, _) in &gone {
                        record(placed, sid, &step.path, None);
                    }
                }
                Err(e) => tracing::warn!(path = %step.path, error = %e, "runner ACE not removed"),
            }
        }
        // A folder Windows won't let the service re-ACL stays wanted and unplaced, so every
        // pass comes back here: one try and one line per run, more only on a recheck.
        let place: Vec<(String, Ace)> = step
            .place
            .into_iter()
            .filter(|(sid, _)| !refused.contains(&(sid.clone(), step.path.clone())))
            .collect();
        if place.is_empty() {
            continue;
        }
        CONVERGE_WALKED.with(|w| w.set(true));
        match grant_aces(dir, &place, None) {
            Ok(()) => {
                for (sid, ace) in &place {
                    let key = (sid.clone(), step.path.clone());
                    // Unseen after placing, it would walk this tree on every minute's verify.
                    if verify && !held(&step.path, sid, *ace) {
                        refused.insert(key);
                    } else {
                        refused.remove(&key);
                    }
                    if had_paths.get(sid).and_then(|p| p.get(&step.path)) == Some(ace) {
                        tracing::warn!(%sid, path = %step.path, "runner ACE had gone, placed again");
                    }
                    record(placed, sid, &step.path, Some(*ace));
                }
            }
            Err(e) => {
                let mut fresh = false;
                for (sid, _) in &place {
                    fresh |= refused.insert((sid.clone(), step.path.clone()));
                }
                if fresh {
                    tracing::warn!(path = %step.path, error = %e, "runner ACE not placed");
                }
            }
        }
    }
}

fn record(placed: &mut PlacedAces, sid: &str, path: &str, ace: Option<Ace>) {
    let paths = &mut placed.grantees.entry(sid.to_string()).or_default().paths;
    match ace {
        Some(ace) => {
            paths.insert(path.to_string(), ace);
        }
        None => {
            paths.remove(path);
        }
    }
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
            (
                r.path.to_string_lossy().into_owned(),
                Ace::of(r.write, r.files, r.link),
            )
        })
        .collect()
}

/// What a plugin's container needs beyond its roots: the runtime, the runner bundle and the
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

/// A declared key as `SetNamedSecurityInfoW` names it: `HKLM\…` once as `MACHINE\…`, and
/// `HKCU\…` as `USERS\<sid>\…` under every account whose hive is loaded, as `~` names every
/// profile. Anything else names no key.
fn named_keys(declared: &str) -> Vec<String> {
    named_keys_for(declared, &user_sids())
}

fn named_keys_for(declared: &str, sids: &[String]) -> Vec<String> {
    if let Some(rest) = key_under(declared, "HKLM\\") {
        return vec![format!("MACHINE\\{rest}")];
    }
    match key_under(declared, "HKCU\\") {
        Some(rest) => sids.iter().map(|s| format!("USERS\\{s}\\{rest}")).collect(),
        None => Vec::new(),
    }
}

/// The path below `hive` (`HKLM\`), when `declared` is a clean key there.
fn key_under<'a>(declared: &'a str, hive: &str) -> Option<&'a str> {
    let rest = declared
        .get(..hive.len())
        .filter(|p| p.eq_ignore_ascii_case(hive))
        .map(|_| &declared[hive.len()..])?;
    let clean = !rest.is_empty()
        && rest.len() <= 255
        && !rest.chars().any(char::is_control)
        && rest
            .split('\\')
            .all(|c| !c.is_empty() && c != "." && c != ".." && !c.contains(['/', '*', '?']));
    clean.then_some(rest)
}

/// The loaded hives of real accounts: local and domain (`S-1-5-21-…`) and Entra ID
/// (`S-1-12-1-…`). Service and `_Classes` hives hold no launcher state.
fn user_sids() -> Vec<String> {
    use winreg::{enums::HKEY_USERS, RegKey};
    RegKey::predef(HKEY_USERS)
        .enum_keys()
        .flatten()
        .filter(|s| s.starts_with("S-1-5-21-") || s.starts_with("S-1-12-1-"))
        .filter(|s| !s.ends_with("_Classes"))
        .collect()
}

/// Stop then start: there is no `Restart-ScheduledTask`, and Start on a running task is a no-op.
pub(super) fn restart_runtime() -> Result<()> {
    powershell(&format!(
        "Stop-ScheduledTask -TaskName {TASK} -ErrorAction SilentlyContinue; \
         Start-ScheduledTask -TaskName {TASK} -ErrorAction Stop"
    ))
}
