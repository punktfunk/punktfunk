//! Host config-dir and owner-private file helpers.
//!
//! Leaf crate so `pf-media`, `pf-vdisplay`, and the orchestrator share these
//! without depending on `gamestream`. Std + `tracing`; on Windows, `pf-paths-win` for ACLs.
//!
//! [`config_dir`] is XDG / `%ProgramData%`, overridable with `PUNKTFUNK_CONFIG_DIR`.
//! [`seat_home`] is where a profile's Steam from Own Steam stays until it is erased.
//! [`create_private_dir`] / [`write_secret_file`] apply 0700 / 0600 on Unix and a
//! SYSTEM/Administrators DACL on Windows. [`replace_file`] / [`replace_secret_file`] /
//! [`replace_users_readable_file`] are the one temp-and-rename writer for stores; the last
//! is for the two files every local account may read, `mgmt-endpoint` and `tray-token`.
//! [`system32`] is how a privileged process names a Windows system tool; [`run_within`] runs
//! one that may hang, and [`remove_device`] runs another. [`seat`] is the Windows multi-seat
//! marker. [`tray_autostart`] is the tray's per-user autostart entry on Linux.
#![forbid(unsafe_code)]

use std::path::PathBuf;

pub mod restart_deletes;
pub mod seat;
#[cfg(not(windows))]
pub mod tray_autostart;

/// `<runtime>/punktfunk-gamescope-ei` (per-user 0700), or `/tmp/…` when there is
/// none. `runtime` is the live session's (`pf_host_config::session::runtime_dir`):
/// `pf-vdisplay` writes the file and `pf-inject` reads it, so both name the same one.
#[cfg(target_os = "linux")]
pub fn gamescope_ei_socket_file(runtime: Option<&str>) -> PathBuf {
    gamescope_ei_relay(runtime, "punktfunk-gamescope-ei")
}

/// `<runtime>/punktfunk-gamescope-{id}-ei`. Isolated bare-spawn
/// sessions (`design/gamescope-multiuser.md`) must not overwrite each
/// other's relay; `id` is `pf-vdisplay`'s `SessionIsolation`.
#[cfg(target_os = "linux")]
pub fn gamescope_ei_socket_file_for(runtime: Option<&str>, id: &str) -> PathBuf {
    gamescope_ei_relay(runtime, &format!("punktfunk-gamescope-{id}-ei"))
}

#[cfg(target_os = "linux")]
fn gamescope_ei_relay(runtime: Option<&str>, name: &str) -> PathBuf {
    match runtime.filter(|s| !s.is_empty()) {
        Some(rt) => PathBuf::from(rt).join(name),
        None => PathBuf::from("/tmp").join(name),
    }
}

/// `$XDG_DATA_HOME/punktfunk/seats` — the Steam homes Own Steam profiles kept. Path only.
#[cfg(target_os = "linux")]
pub fn seats_dir() -> PathBuf {
    data_dir().join("seats")
}

/// `$XDG_DATA_HOME/punktfunk/seats/<id>` — the Steam home a profile kept from playing as Own
/// Steam, which removing the profile with `erase` deletes. `id` is the profile id. Path only.
#[cfg(target_os = "linux")]
pub fn seat_home(id: &str) -> PathBuf {
    seats_dir().join(id)
}

/// `…/seats/<id>.json` — the pre-warm record an Own Steam profile left beside its home. Path
/// only; nothing reads it, and erasing the profile deletes it.
#[cfg(target_os = "linux")]
pub fn seat_record(id: &str) -> PathBuf {
    seats_dir().join(format!("{id}.json"))
}

/// `$XDG_DATA_HOME/punktfunk`, else `~/.local/share/punktfunk`. Separate from
/// [`config_dir`]: it holds installs (a seat's Steam, managed emulators), and the plugin
/// runner shares nothing under the config dir.
/// Whether this process's binary is no longer the file at its path: a package update swaps the
/// inode, a sysext refresh swaps the mount, a deleted path is gone. NixOS never touches a store
/// path, so there the system profile naming another build counts. Only a restart runs the new.
#[cfg(target_os = "linux")]
pub fn exe_replaced() -> bool {
    let link = std::fs::read_link("/proc/self/exe").unwrap_or_default();
    !link.as_os_str().is_empty()
        && (std::fs::metadata("/proc/self/exe").is_ok_and(|run| replaced_on_disk(&link, &run))
            || profile_moved(&link, std::path::Path::new("/run/current-system/sw/bin")))
}

/// Whether `exe`, a Nix store path, is no longer what `profile_bin` names under its file name.
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn profile_moved(exe: &std::path::Path, profile_bin: &std::path::Path) -> bool {
    let Some(name) = exe.file_name().filter(|_| exe.starts_with("/nix/store")) else {
        return false;
    };
    std::fs::canonicalize(profile_bin.join(name)).is_ok_and(|now| now != exe)
}

/// Whether the file at `path` is no longer the `running` binary ([`exe_replaced`]).
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn replaced_on_disk(path: &std::path::Path, running: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    !std::fs::metadata(path)
        .is_ok_and(|now| (now.dev(), now.ino()) == (running.dev(), running.ino()))
}

#[cfg(not(target_os = "windows"))]
pub fn data_dir() -> PathBuf {
    xdg_home("XDG_DATA_HOME", ".local/share").join("punktfunk")
}

/// `$XDG_STATE_HOME/punktfunk`, else `~/.local/state/punktfunk`: what must outlive a crash and
/// a reboot, such as the audio defaults a session claimed.
#[cfg(not(target_os = "windows"))]
pub fn state_dir() -> PathBuf {
    xdg_home("XDG_STATE_HOME", ".local/state").join("punktfunk")
}

/// `$var` when it holds an absolute path, else `$HOME/<fallback>`, else `.`. The XDG
/// base-dir spec ignores an empty or relative value. A caller that must not land in the
/// cwd checks `is_absolute()`. Ungated: string work only.
pub fn xdg_home(var: &str, fallback: &str) -> PathBuf {
    xdg_home_from(std::env::var_os(var), std::env::var_os("HOME"), fallback)
}

fn xdg_home_from(
    value: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    fallback: &str,
) -> PathBuf {
    value
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(fallback)))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Host identity, pairing, mgmt token, library.
///
/// Windows uses `%ProgramData%` so the SYSTEM service and the interactive
/// user share one dir that survives logout. Elsewhere `$XDG_CONFIG_HOME`, ignored
/// when empty or relative. `PUNKTFUNK_CONFIG_DIR` overrides.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PUNKTFUNK_CONFIG_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("ProgramData")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(not(target_os = "windows"))]
    let base = xdg_home("XDG_CONFIG_HOME", ".config");
    base.join("punktfunk")
}

/// `punktfunk-host serve` writes `PUNKTFUNK_MGMT_URL=https://127.0.0.1:<port>`
/// on start. The tray cannot read `host.env` on Windows (SYSTEM/Administrators
/// DACL); this file is Users-readable so a `PUNKTFUNK_MGMT_BIND` move still
/// reaches loopback consumers. `None` if absent or unparsable.
pub fn published_mgmt_port() -> Option<u16> {
    published_mgmt_port_in(&config_dir())
}

/// Takes the directory so tests do not call `set_var` (`unsafe`; forbidden here).
pub fn published_mgmt_port_in(dir: &std::path::Path) -> Option<u16> {
    let raw = std::fs::read_to_string(dir.join("mgmt-endpoint")).ok()?;
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let value = line.split_once('=').map_or(line, |(_, v)| v).trim();
    value
        .trim_end_matches('/')
        .rsplit_once(':')
        .and_then(|(_, port)| port.parse().ok())
}

/// `host.env`'s `KEY=VALUE` grammar, as the Windows service loads it into its environment.
pub mod env_file {
    /// Each entry in file order: lines trimmed, `#` comments and lines without `=` skipped,
    /// split on the first `=`, both sides trimmed, surrounding quotes stripped.
    pub fn parse(text: &str) -> impl Iterator<Item = (&str, &str)> {
        text.lines().filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            (!key.is_empty()).then(|| (key, value.trim().trim_matches('"')))
        })
    }

    /// The value `key` ends up with: a later line wins.
    pub fn get<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        parse(text)
            .filter(|(k, _)| *k == key)
            .last()
            .map(|(_, v)| v)
    }
}

/// The mode [`create_private_dir`] gives `dir`: 0700, except the door's box directory
/// (`config`), which every seat user walks to its own entry below it. Its files are 0600 and
/// its subdirectories 0700, so execute-only shows names and no more.
#[cfg(unix)]
fn private_dir_mode(door: bool, dir: &std::path::Path, config: &std::path::Path) -> u32 {
    if door && dir == config {
        0o711
    } else {
        0o700
    }
}

/// Create `dir` if missing and make it owner-private, tightening one that exists: 0700 on Unix,
/// owner Administrators and SYSTEM/Administrators only on Windows ([`harden_dir`]). Windows
/// refuses a reparse point ([`reject_reparse_point`]): hardening a junction would harden the
/// attacker-chosen target while the link stays theirs. Never the shared temp root: Windows
/// would lock its owner out, so a test keeps its files in a subdir.
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    debug_assert_ne!(dir, std::env::temp_dir(), "hardening the shared temp root");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mode = private_dir_mode(seat::role().is_door(), dir, &config_dir());
        let r = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(mode)
            .create(dir);
        // `recursive` does not re-chmod an existing dir.
        if dir.exists() {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode));
        }
        r
    }
    #[cfg(windows)]
    {
        harden_dir(dir)
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// What the first hardening of an existing directory in a process does, from what it read
/// before changing anything.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, PartialEq)]
enum FirstPass {
    /// Another account owns it, or its owner is unreadable. An owner keeps any `WRITE_DAC`
    /// handle it opened before a lock, so the folder is not locked in place.
    Refuse,
    /// Its parent let others write in it: lock it, then set aside every entry they own.
    SetAside,
    /// It was admin-only: lock it and re-own what lies below to Administrators.
    Reown,
}

/// [`FirstPass`] from the owner check (`Some(true)`: SYSTEM, Administrators or
/// TrustedInstaller) and whether an inherited ACE let another account write in the folder.
#[cfg_attr(not(windows), allow(dead_code))]
fn first_pass(admin_owned: Option<bool>, inherits_open: bool) -> FirstPass {
    match (admin_owned, inherits_open) {
        (Some(true), true) => FirstPass::SetAside,
        (Some(true), false) => FirstPass::Reown,
        _ => FirstPass::Refuse,
    }
}

/// Directories whose first hardening in this process completed.
#[cfg(windows)]
static HARDENED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// [`create_private_dir`] on Windows. Held across a first pass, [`HARDENED`]'s lock keeps a
/// second caller from using a folder that is locked but not yet cleared; the re-own walk runs
/// after it, since an admin-only folder holds nothing to clear. A failed first pass is retried.
#[cfg(windows)]
fn harden_dir(dir: &std::path::Path) -> std::io::Result<()> {
    reject_reparse_point(dir)?;
    let mut hardened = HARDENED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if hardened.iter().any(|done| done == dir) {
        drop(hardened);
        return relock(dir);
    }
    let reown = first_hardening(dir)?;
    hardened.push(dir.to_path_buf());
    drop(hardened);
    if reown {
        reown_tree(dir);
    }
    Ok(())
}

/// A later pass: the first one proved `dir`, so a lock that fails leaves it as it was.
#[cfg(windows)]
fn relock(dir: &std::path::Path) -> std::io::Result<()> {
    if create_locked(dir)? {
        return Ok(());
    }
    let _hold = hold_plain_directory(dir)?;
    if let Err(e) = pf_paths_win::lock_admin_only(dir) {
        tracing::warn!(dir = %dir.display(), error = %e, "config directory lock not applied");
    }
    Ok(())
}

/// Read `dir` before changing it, then lock it and deal with what it holds ([`FirstPass`]).
/// `true` when its tree is still to be re-owned. A folder that was open and does not end
/// admin-only, or keeps an entry another account owns, is an error: its contents are unproven.
/// It holds a new marker ([`pf_paths_win::mark_unproven`]) from its lock until then, so the
/// next pass sets aside again instead of re-owning what the failed one left.
#[cfg(windows)]
fn first_hardening(dir: &std::path::Path) -> std::io::Result<bool> {
    if create_locked(dir)? {
        return Ok(false);
    }
    let _hold = hold_plain_directory(dir)?;
    let lock_failed = |e: std::io::Error| {
        std::io::Error::new(
            e.kind(),
            format!("lock {} to SYSTEM and Administrators: {e}", dir.display()),
        )
    };
    match first_pass(
        pf_paths_win::is_admin_owned(dir),
        pf_paths_win::inherits_open_acl(dir) || pf_paths_win::is_unproven(dir),
    ) {
        FirstPass::Refuse => Err(std::io::Error::other(format!(
            "{} is owned by an account outside SYSTEM and Administrators",
            dir.display()
        ))),
        FirstPass::SetAside => {
            pf_paths_win::lock_admin_only(dir).map_err(lock_failed)?;
            let marker = pf_paths_win::mark_unproven(dir)?;
            pf_paths_win::ensure_admin_only_source(dir).map_err(lock_failed)?;
            pf_paths_win::quarantine_planted_tree(dir).map_err(|e| {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "set aside another account's entry in {}: {e}",
                        dir.display()
                    ),
                )
            })?;
            pf_paths_win::mark_proven(dir, &marker)?;
            Ok(false)
        }
        FirstPass::Reown => {
            if let Err(e) = pf_paths_win::lock_admin_only(dir) {
                tracing::warn!(dir = %dir.display(), error = %e, "config directory lock not applied");
            }
            Ok(true)
        }
    }
}

/// Create `dir` admin-only from its first instant; missing parents are plain. `true` when this
/// call made it: born locked, it holds nothing to prove.
#[cfg(windows)]
fn create_locked(dir: &std::path::Path) -> std::io::Result<bool> {
    if dir.is_dir() {
        return Ok(false);
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match pf_paths_win::create_dir_with(dir, pf_paths_win::ADMIN_ONLY) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(false),
        Err(e) => Err(e),
    }
}

/// `icacls /setowner Administrators /T` over `dir`: an admin-only folder's entries were written by
/// SYSTEM, an administrator or the account of a grant. Best effort.
#[cfg(windows)]
fn reown_tree(dir: &std::path::Path) {
    let _ = icacls_status(
        std::process::Command::new(system32("icacls.exe"))
            .arg(dir.as_os_str())
            .args(["/setowner", "*S-1-5-32-544", "/T", "/C", "/Q"]) // BUILTIN\Administrators
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    );
}

#[cfg(windows)]
fn reject_reparse_point(path: &std::path::Path) -> std::io::Result<()> {
    // FILE_ATTRIBUTE_REPARSE_POINT. Hard-coded: this crate stays pure-std.
    const REPARSE: u32 = 0x400;
    match std::fs::symlink_metadata(path) {
        Ok(md) => {
            use std::os::windows::fs::MetadataExt;
            if md.file_attributes() & REPARSE != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "{} is a reparse point (junction/symlink) — refusing to use it as a \
                         security-sensitive path",
                        path.display()
                    ),
                ));
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
fn handle_is_reparse(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::windows::fs::MetadataExt;
    const REPARSE: u32 = 0x400;
    Ok(file.metadata()?.file_attributes() & REPARSE != 0)
}

/// Hold the directory open with no delete-share across the path-based ACL update.
#[cfg(windows)]
fn hold_plain_directory(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const SHARE_READ_WRITE: u32 = 0x1 | 0x2;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_READ_WRITE)
        .custom_flags(BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
        .open(path)?;
    if handle_is_reparse(&file)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a reparse point", path.display()),
        ));
    }
    Ok(file)
}

/// Re-own to Administrators then re-ACL. A planted file's creator keeps
/// `WRITE_DAC`, so ACL-only would let them put access back.
#[cfg(windows)]
pub fn restrict_existing_secret_file(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    let icacls = system32("icacls.exe");
    let _ = icacls_status(
        std::process::Command::new(&icacls)
            .arg(path.as_os_str())
            .args(["/setowner", "*S-1-5-32-544"]) // BUILTIN\Administrators
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    );
    if let Err(e) = restrict_to_system_admins(path) {
        tracing::warn!(path = %path.display(), error = %e, "icacls hardening did not succeed");
    }
}

/// No-op: POSIX modes are set at create, and a user-owned home dir is not `%ProgramData%`.
#[cfg(not(windows))]
pub fn restrict_existing_secret_file(_path: &std::path::Path) {}

/// `%SystemRoot%\System32\<rel>`, else under `%WINDIR%`, else `C:\Windows`.
///
/// `CreateProcess` searches the exe's directory and the cwd before `PATH`, and the callers run
/// elevated or as SYSTEM, so a system tool is never spawned by bare name. `rel` may carry a
/// subdirectory ([`POWERSHELL`]). Ungated: string work only.
pub fn system32(rel: &str) -> String {
    format!(r"{}\System32\{rel}", system_root())
}

/// `%SystemRoot%\SysWOW64\<rel>`: where 32-bit processes find what [`system32`] names.
pub fn syswow64(rel: &str) -> String {
    format!(r"{}\SysWOW64\{rel}", system_root())
}

/// `%SystemRoot%\<rel>`, with [`system32`]'s fallbacks.
pub fn windows_dir(rel: &str) -> String {
    format!(r"{}\{rel}", system_root())
}

fn system_root() -> String {
    std::env::var("SystemRoot")
        .or_else(|_| std::env::var("WINDIR"))
        .unwrap_or_else(|_| r"C:\Windows".to_string())
}

/// Windows PowerShell under System32, for [`system32`].
pub const POWERSHELL: &str = r"WindowsPowerShell\v1.0\powershell.exe";

/// Run `command` to its end, or kill it after `limit` and fail with `TimedOut`. Stdout and
/// stderr come back when the caller piped them. A system tool can hang, and
/// `Command::output` waits on it forever.
pub fn run_within(
    command: &mut std::process::Command,
    limit: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread::{sleep, spawn, JoinHandle};
    use std::time::{Duration, Instant};
    /// How long a killed process may take to go, and how long a pipe may stay open after its
    /// writer exited. A process stuck in the kernel outlives its kill; a grandchild keeps a pipe.
    const GRACE: Duration = Duration::from_secs(2);
    type Drain = (Arc<Mutex<Vec<u8>>>, JoinHandle<()>);
    fn drain(pipe: Option<impl Read + Send + 'static>) -> Option<Drain> {
        pipe.map(|mut pipe| {
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&bytes);
            let reader = spawn(move || {
                let mut chunk = [0_u8; 4096];
                while let Ok(n @ 1..) = pipe.read(&mut chunk) {
                    let mut sink = sink.lock().unwrap_or_else(PoisonError::into_inner);
                    sink.extend_from_slice(&chunk[..n]);
                }
            });
            (bytes, reader)
        })
    }
    fn wait(
        child: &mut std::process::Child,
        limit: Duration,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            sleep(Duration::from_millis(10));
        }
    }
    let mut child = command.spawn()?;
    let (stdout, stderr) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let Some(status) = wait(&mut child, limit)? else {
        let _ = child.kill();
        let _ = wait(&mut child, GRACE);
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("no exit within {} s", limit.as_secs()),
        ));
    };
    // A pipe a grandchild still holds open is not waited on: the reader's bytes so far are all.
    let bytes = |drain: Option<Drain>| {
        let Some((bytes, reader)) = drain else {
            return Vec::new();
        };
        let end = Instant::now() + GRACE;
        while !reader.is_finished() && Instant::now() < end {
            sleep(Duration::from_millis(10));
        }
        let mut bytes = bytes.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut *bytes)
    };
    Ok(std::process::Output {
        status,
        stdout: bytes(stdout),
        stderr: bytes(stderr),
    })
}

/// An `icacls` run is over within this, or it is hung: a first hardening walks a tree.
#[cfg(windows)]
const ICACLS_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

/// `command`'s exit status, within [`ICACLS_LIMIT`]. Its output is discarded by the caller.
#[cfg(windows)]
fn icacls_status(command: &mut std::process::Command) -> std::io::Result<std::process::ExitStatus> {
    run_within(command, ICACLS_LIMIT).map(|output| output.status)
}

/// `pnputil /remove-device` by absolute path: an uninstaller must not depend on `%PATH%`.
/// `Err` carries pnputil's exit status and message, or why it did not run.
#[cfg(windows)]
pub fn remove_device(instance_id: &str) -> std::io::Result<()> {
    let o = std::process::Command::new(system32("pnputil.exe"))
        .args(["/remove-device", instance_id])
        .output()
        .map_err(|e| std::io::Error::new(e.kind(), format!("run pnputil: {e}")))?;
    if o.status.success() {
        return Ok(());
    }
    // Whichever stream pnputil wrote its reason to.
    let msg = if o.stderr.is_empty() {
        &o.stdout
    } else {
        &o.stderr
    };
    Err(std::io::Error::other(format!(
        "pnputil /remove-device {}: {}",
        o.status,
        String::from_utf8_lossy(msg).trim()
    )))
}

/// Unix: create and re-chmod 0600 so it is never group/world-readable.
/// Windows: `OpenOptions` cannot pass `SECURITY_ATTRIBUTES` and this crate
/// forbids `unsafe`, so the file is created empty, `icacls`'d, then written.
/// The DACL step is fatal; a failure unlinks the still-empty file. Windows
/// checks access only at open, so the file is held unshared until its own DACL
/// stands: a reader admitted by the inherited one cannot keep a handle for the
/// bytes. `icacls` opens for the DACL alone, which sharing does not gate.
/// The bytes reach the disk before return, so a rename after it never publishes an empty file.
pub fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    // Never write a secret through a link: the bytes would land on the attacker's target.
    #[cfg(windows)]
    reject_reparse_point(path)?;
    // Unlink, then create-new. A planted file another account still holds open (no
    // share-delete) refuses the unlink, so the write fails closed instead of landing the
    // fresh secret in a file that account reads through its handle.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        opts.share_mode(0).custom_flags(OPEN_REPARSE_POINT);
    }
    let mut f = opts.open(path)?;
    #[cfg(windows)]
    if handle_is_reparse(&f)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{} is a reparse point", path.display()),
        ));
    }
    #[cfg(windows)]
    if let Err(e) = restrict_to_system_admins(path) {
        drop(f);
        // Callers treat a non-empty file as "secret present"; do not leave a 0-byte stub.
        let _ = std::fs::remove_file(path);
        return Err(e);
    }
    f.write_all(contents)?;
    f.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Replace `path` with `contents` through a synced sibling temp and a rename: a reader or a
/// power cut sees the old file or the new one, never half of either. Default permissions; the
/// parent is made with `create_dir_all`.
pub fn replace_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    replace(path, contents, Temp::Plain)
}

/// [`replace_file`] for an owner-only file: the parent is a [`create_private_dir`] and the temp
/// a [`write_secret_file`], whose mode or DACL the rename carries to `path`.
pub fn replace_secret_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    replace(path, contents, Temp::Secret)
}

/// [`replace_secret_file`] whose result every local account may read. Windows adds
/// `BUILTIN\Users:(R)` to the temp before the rename, so a reader sees the old file or the
/// readable new one, never a locked one. Unix stays owner-only: the host's user is the reader.
pub fn replace_users_readable_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    replace(path, contents, Temp::UsersReadable(None))
}

/// [`replace_users_readable_file`] that the accounts of `deny` (an icacls principal, `*<SID>`)
/// may not read. Windows denies them on the temp beside the grant, so none of them ever sees
/// the published file. Unix ignores `deny`: the file is owner-only there.
pub fn replace_users_readable_file_denying(
    path: &std::path::Path,
    contents: &[u8],
    deny: &str,
) -> std::io::Result<()> {
    replace(path, contents, Temp::UsersReadable(Some(deny)))
}

/// How [`replace`] makes its temp, and so the published file.
#[derive(Clone, Copy, PartialEq)]
enum Temp<'a> {
    Plain,
    Secret,
    UsersReadable(Option<&'a str>),
}

/// The temp is removed on every error. A crash between write and rename leaves it behind:
/// nothing reaps `*.tmp`, since another writer may own an in-flight one.
fn replace(path: &std::path::Path, contents: &[u8], mode: Temp) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        if mode == Temp::Plain {
            std::fs::create_dir_all(dir)?;
        } else {
            create_private_dir(dir)?;
        }
    }
    let tmp = TmpFile(Some(unique_tmp_path(path)));
    if mode == Temp::Plain {
        use std::io::Write;
        let mut f = std::fs::File::create(tmp.path())?;
        f.write_all(contents)?;
        f.sync_all()?;
    } else {
        write_secret_file(tmp.path(), contents)?;
    }
    #[cfg(windows)]
    if let Temp::UsersReadable(deny) = mode {
        grant_users_read(tmp.path(), deny)?;
    }
    std::fs::rename(tmp.path(), path)?;
    tmp.published();
    Ok(())
}

/// `BUILTIN\Users:(R)` beside the SYSTEM/Administrators ACEs [`write_secret_file`] set, and a
/// read deny for `deny`, which outranks the grant.
#[cfg(windows)]
fn grant_users_read(path: &std::path::Path, deny: Option<&str>) -> std::io::Result<()> {
    let mut icacls = std::process::Command::new(system32("icacls.exe"));
    icacls
        .arg(path.as_os_str())
        .args(["/grant:r", "*S-1-5-32-545:(R)"]);
    if let Some(principal) = deny {
        icacls.args(["/deny", &format!("{principal}:(R)")]);
    }
    let status = icacls_status(
        icacls
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    )?;
    if status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "icacls grant Users read on {} ({status})",
        path.display()
    )))
}

/// `<name>.<pid>.<n>.tmp`. The pid keeps the CLI and the service apart; `n` keeps threads
/// apart, since [`write_secret_file`] unlinks whatever already holds the name.
fn unique_tmp_path(path: &std::path::Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.{n}.tmp", std::process::id()));
    path.with_file_name(name)
}

/// Owns a temp until [`Self::published`]; dropping it earlier deletes the file.
struct TmpFile(Option<PathBuf>);

impl TmpFile {
    fn path(&self) -> &std::path::Path {
        self.0.as_deref().expect("disarmed only by consuming self")
    }
    /// The rename landed: the path is the real file now.
    fn published(mut self) {
        self.0 = None;
    }
}

impl Drop for TmpFile {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// OWNER RIGHTS is the creating account (SYSTEM service or a manual run).
/// Failure is returned: [`write_secret_file`] treats it as fatal (only
/// control over the bytes about to be written); [`restrict_existing_secret_file`]
/// only warns.
#[cfg(windows)]
fn restrict_to_system_admins(path: &std::path::Path) -> std::io::Result<()> {
    let icacls = system32("icacls.exe");
    let status = icacls_status(
        std::process::Command::new(icacls)
            .arg(path.as_os_str())
            .args([
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-18:(F)", // NT AUTHORITY\SYSTEM
                "/grant:r",
                "*S-1-5-32-544:(F)", // BUILTIN\Administrators
                "/grant:r",
                "*S-1-3-4:(F)", // OWNER RIGHTS
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    )?;
    if status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "icacls restrict {} to SYSTEM/Administrators ({status}) — it stays readable by other \
         local users",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    /// A package manager renames a new file over the path; the kernel suffixes a
    /// deleted exe's link with ` (deleted)`. Both must read as replaced.
    #[cfg(unix)]
    #[test]
    fn spots_a_binary_swapped_under_the_process() {
        use super::replaced_on_disk;
        let dir = std::env::temp_dir().join(format!("pf-exe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("punktfunk-host");
        std::fs::write(&path, b"old").unwrap();
        let running = std::fs::metadata(&path).unwrap();
        assert!(!replaced_on_disk(&path, &running));

        let staged = dir.join("punktfunk-host.new");
        std::fs::write(&staged, b"new").unwrap();
        std::fs::rename(&staged, &path).unwrap();
        assert!(replaced_on_disk(&path, &running));
        assert!(replaced_on_disk(
            &dir.join("punktfunk-host (deleted)"),
            &running
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A rebuilt NixOS system names a new store path; the old one stays on disk untouched.
    #[cfg(unix)]
    #[test]
    fn a_nix_profile_naming_another_build_reads_as_replaced() {
        use super::profile_moved;
        let dir = std::env::temp_dir().join(format!("pf-nix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let running = std::path::Path::new("/nix/store/aaa-punktfunk/bin/punktfunk-host");
        assert!(!profile_moved(running, &dir), "no profile entry");
        std::fs::write(dir.join("new"), b"").unwrap();
        std::os::unix::fs::symlink(dir.join("new"), dir.join("punktfunk-host")).unwrap();
        assert!(profile_moved(running, &dir));
        let elsewhere = std::path::Path::new("/usr/bin/punktfunk-host");
        assert!(!profile_moved(elsewhere, &dir), "only a store path");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    use super::*;

    #[test]
    fn env_file_reads_host_env_as_the_service_does() {
        let text = "# PUNKTFUNK_MGMT_BIND=commented\n  PUNKTFUNK_MGMT_BIND = \"0.0.0.0:48123\" \n\
                    no equals\n=orphan\nRUST_LOG=info\nRUST_LOG=debug\n";
        assert_eq!(
            env_file::parse(text).collect::<Vec<_>>(),
            [
                ("PUNKTFUNK_MGMT_BIND", "0.0.0.0:48123"),
                ("RUST_LOG", "info"),
                ("RUST_LOG", "debug"),
            ]
        );
        assert_eq!(env_file::get(text, "RUST_LOG"), Some("debug"));
        assert_eq!(env_file::get(text, "PUNKTFUNK_UI_BIND"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn per_session_ei_relay_shares_the_global_files_directory() {
        let rt = Some("/run/user/1000");
        let global = gamescope_ei_socket_file(rt);
        assert_eq!(
            global.parent(),
            Some(std::path::Path::new("/run/user/1000"))
        );
        assert_eq!(
            gamescope_ei_socket_file(None).parent(),
            Some(std::path::Path::new("/tmp"))
        );
        let per = gamescope_ei_socket_file_for(rt, "cafe0123");
        assert_eq!(per.parent(), global.parent());
        assert_eq!(
            per.file_name().unwrap().to_str().unwrap(),
            "punktfunk-gamescope-cafe0123-ei"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_seat_home_is_the_data_dir_plus_the_seat_id() {
        let seat = seat_home("cafe0123");
        assert!(
            seat.ends_with("punktfunk/seats/cafe0123"),
            "{}",
            seat.display()
        );
        assert_ne!(seat, seat_home("anon0"), "two seats never share a home");
        // The record sits beside the home, never inside the `HOME` Steam owns.
        let record = seat_record("cafe0123");
        assert_eq!(record.parent(), seat.parent());
        assert!(!record.starts_with(&seat), "{}", record.display());
    }

    #[test]
    fn system_tools_resolve_under_system32_never_by_bare_name() {
        let p = system32("icacls.exe");
        assert!(p.ends_with(r"\System32\icacls.exe"), "{p}");
        assert!(
            p.len() > r"\System32\icacls.exe".len(),
            "a root, not a bare name: {p}"
        );
        let ps = system32(POWERSHELL);
        assert!(
            ps.ends_with(r"\System32\WindowsPowerShell\v1.0\powershell.exe"),
            "{ps}"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn an_empty_or_relative_xdg_value_falls_back_to_home() {
        let home = || Some("/home/u".into());
        for bad in ["", "punktfunk-rel"] {
            assert_eq!(
                xdg_home_from(Some(bad.into()), home(), ".config"),
                PathBuf::from("/home/u/.config"),
                "{bad:?}"
            );
        }
        assert_eq!(
            xdg_home_from(Some("/xdg".into()), home(), ".config"),
            PathBuf::from("/xdg")
        );
        assert_eq!(
            xdg_home_from(None, home(), ".local/share"),
            PathBuf::from("/home/u/.local/share")
        );
    }

    #[test]
    fn published_mgmt_port_follows_the_endpoint_file_and_is_absent_without_it() {
        let dir = std::env::temp_dir().join(format!("pf-paths-endpoint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        assert_eq!(
            published_mgmt_port_in(&dir),
            None,
            "no file → fall back to the default"
        );

        std::fs::write(
            dir.join("mgmt-endpoint"),
            "PUNKTFUNK_MGMT_URL=https://127.0.0.1:47995\n",
        )
        .unwrap();
        assert_eq!(published_mgmt_port_in(&dir), Some(47995));

        std::fs::write(dir.join("mgmt-endpoint"), "\n").unwrap();
        assert_eq!(
            published_mgmt_port_in(&dir),
            None,
            "blank reads as unset, not port 0"
        );

        std::fs::write(dir.join("mgmt-endpoint"), "PUNKTFUNK_MGMT_URL=\n").unwrap();
        assert_eq!(published_mgmt_port_in(&dir), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn precreated_directory_junction_is_refused() {
        let root = std::env::temp_dir().join(format!("pf-paths-junction-{}", std::process::id()));
        let target = root.join("target");
        let link = root.join("config");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&target).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J must work for a standard user");
        assert!(create_private_dir(&link).is_err());
        std::fs::remove_dir(&link).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Only an admin-owned folder is locked in place; whether its parent let others write in it
    /// decides between setting their entries aside and re-owning the tree.
    #[test]
    fn the_first_pass_follows_the_owner_then_the_inherited_acl() {
        assert_eq!(first_pass(Some(true), false), FirstPass::Reown);
        assert_eq!(first_pass(Some(true), true), FirstPass::SetAside);
        for open in [false, true] {
            assert_eq!(first_pass(Some(false), open), FirstPass::Refuse);
            assert_eq!(first_pass(None, open), FirstPass::Refuse);
        }
    }

    /// Elevated: a healthy folder keeps the exact ACL the old `icacls` sequence gave it, an open
    /// one ends with that same ACL, and an entry another account owns goes aside while an
    /// administrator's stays. Not elevated, the folder this test owns is refused untouched.
    #[cfg(windows)]
    #[test]
    fn an_open_folder_ends_like_a_healthy_one_without_what_others_own() {
        use std::process::{Command, Stdio};
        let icacls = |path: &std::path::Path, args: &[&str]| {
            Command::new(system32("icacls.exe"))
                .arg(path)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let sddl = |path: &std::path::Path| {
            let out = Command::new(system32(POWERSHELL))
                .args([
                    "-NoProfile",
                    "-Command",
                    "(Get-Acl -LiteralPath $env:PF_ACL_PATH).Sddl",
                ])
                .env("PF_ACL_PATH", path)
                // A PowerShell 7 parent leaves a module path Windows PowerShell can't load from.
                .env_remove("PSModulePath")
                .output()
                .unwrap();
            let sddl = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            assert!(!sddl.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
            sddl
        };
        let base = std::env::temp_dir().join(format!("pf-paths-open-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        // Users may write below `base`, as `%ProgramData%` lets them.
        assert!(icacls(&base, &["/grant", "*S-1-5-32-545:(OI)(CI)(M)"]));
        let (healthy, open) = (base.join("healthy"), base.join("open"));
        std::fs::create_dir_all(&healthy).unwrap();
        std::fs::create_dir_all(open.join("sub")).unwrap();
        std::fs::write(open.join("sub").join("host.env"), "x").unwrap();
        std::fs::write(open.join("hooks.json"), "x").unwrap();

        if pf_paths_win::is_admin_owned(&open) != Some(true) {
            assert!(create_private_dir(&open).is_err());
            assert!(open.join("hooks.json").exists());
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        assert!(icacls(&healthy, &["/setowner", "*S-1-5-32-544"]));
        assert!(icacls(&healthy, &["/reset"]));
        assert!(icacls(
            &healthy,
            &[
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-18:(OI)(CI)(F)",
                "/grant:r",
                "*S-1-5-32-544:(OI)(CI)(F)",
            ],
        ));
        let before = sddl(&healthy);
        assert!(before.starts_with("O:BA"), "{before}");
        create_private_dir(&healthy).unwrap();
        assert_eq!(
            sddl(&healthy),
            before,
            "a healthy folder's ACL does not move"
        );

        // LocalService stands in for the standard user who planted the file.
        assert!(icacls(
            &open.join("hooks.json"),
            &["/setowner", "*S-1-5-19"]
        ));
        create_private_dir(&open).unwrap();
        assert_eq!(sddl(&open), before);
        assert!(open.join("sub").join("host.env").exists());
        assert!(!open.join("hooks.json").exists());
        assert!(open.join("hooks.json.untrusted").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Elevated: a plant held open without share-delete fails the first pass, which leaves the
    /// folder locked, the plant another account's and the marker in place, so the pass after
    /// the handle closes sets it aside instead of re-owning it. Not elevated, the folder this
    /// test owns is refused.
    #[cfg(windows)]
    #[test]
    fn a_failed_set_aside_is_retried_not_adopted() {
        use std::os::windows::fs::OpenOptionsExt;
        let icacls = |path: &std::path::Path, args: &[&str]| {
            std::process::Command::new(system32("icacls.exe"))
                .arg(path)
                .args(args)
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let base = std::env::temp_dir().join(format!("pf-paths-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let open = base.join("open");
        std::fs::create_dir_all(&open).unwrap();
        assert!(icacls(&base, &["/grant", "*S-1-5-32-545:(OI)(CI)(M)"]));
        let plant = open.join("hooks.json");
        std::fs::write(&plant, "x").unwrap();
        if pf_paths_win::is_admin_owned(&open) != Some(true) {
            assert!(create_private_dir(&open).is_err());
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        // LocalService stands in for the standard user who planted the file.
        assert!(icacls(&plant, &["/setowner", "*S-1-5-19"]));
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1) // FILE_SHARE_READ: no rename while this is open
            .open(&plant)
            .unwrap();
        let marker = open.join(pf_paths_win::UNPROVEN);
        assert!(create_private_dir(&open).is_err());
        assert!(
            pf_paths_win::ensure_admin_only_source(&open).is_ok(),
            "a failed pass reopened the folder"
        );
        assert!(marker.is_file(), "nothing tells the next pass");
        assert_eq!(
            pf_paths_win::is_admin_owned(&plant),
            Some(false),
            "the plant was adopted"
        );
        drop(held);
        create_private_dir(&open).unwrap();
        assert!(!plant.exists(), "the plant stayed");
        assert!(open.join("hooks.json.untrusted").exists());
        assert!(!marker.exists(), "a proven folder keeps its marker");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Elevated: a marker another account planted and holds without share-delete fails the
    /// first pass, which leaves its own marker beside it, so the pass after the planter deletes
    /// theirs sets the plant aside instead of re-owning it. Not elevated, the folder is refused.
    #[cfg(windows)]
    #[test]
    fn a_planted_marker_held_open_is_no_proof() {
        use std::os::windows::fs::OpenOptionsExt;
        let icacls = |path: &std::path::Path, args: &[&str]| {
            std::process::Command::new(system32("icacls.exe"))
                .arg(path)
                .args(args)
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        let base = std::env::temp_dir().join(format!("pf-paths-marker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let open = base.join("open");
        std::fs::create_dir_all(&open).unwrap();
        assert!(icacls(&base, &["/grant", "*S-1-5-32-545:(OI)(CI)(M)"]));
        let (plant, marker) = (open.join("hooks.json"), open.join(pf_paths_win::UNPROVEN));
        std::fs::write(&plant, "x").unwrap();
        std::fs::write(&marker, "").unwrap();
        if pf_paths_win::is_admin_owned(&open) != Some(true) {
            assert!(create_private_dir(&open).is_err());
            let _ = std::fs::remove_dir_all(&base);
            return;
        }
        // LocalService stands in for the standard user who planted both files.
        assert!(icacls(&plant, &["/setowner", "*S-1-5-19"]));
        assert!(icacls(&marker, &["/setowner", "*S-1-5-19"]));
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x1) // FILE_SHARE_READ: no rename while this is open
            .open(&marker)
            .unwrap();
        assert!(create_private_dir(&open).is_err());
        drop(held);
        std::fs::remove_file(&marker).unwrap();
        assert!(
            pf_paths_win::is_unproven(&open),
            "the planter's marker was the only one"
        );
        create_private_dir(&open).unwrap();
        assert!(!plant.exists(), "the plant was adopted");
        assert_eq!(
            pf_paths_win::is_admin_owned(&open.join("hooks.json.untrusted")),
            Some(false)
        );
        assert!(
            !pf_paths_win::is_unproven(&open),
            "a proven folder keeps a marker"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn secrets_are_owner_only_on_create_and_on_rewrite() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pf-paths-secret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        create_private_dir(&dir).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, "a secrets dir is owner-only");

        let shared = dir.join("tray-token");
        replace_users_readable_file(&shared, b"PUNKTFUNK_TRAY_TOKEN=abc\n").unwrap();
        assert_eq!(
            mode(&shared),
            0o600,
            "Unix has no Users ACE: the dir is the boundary"
        );
        assert_eq!(
            env_file::get(
                &std::fs::read_to_string(&shared).unwrap(),
                "PUNKTFUNK_TRAY_TOKEN"
            ),
            Some("abc")
        );

        let key = dir.join("key.pem");
        write_secret_file(&key, b"-----BEGIN PRIVATE KEY-----\n").unwrap();
        assert_eq!(mode(&key), 0o600);
        write_secret_file(&key, b"rotated").unwrap();
        assert_eq!(mode(&key), 0o600, "the rewrite path keeps 0600");
        assert_eq!(std::fs::read(&key).unwrap(), b"rotated", "and truncates");

        let planted = dir.join("mgmt-token");
        std::fs::write(&planted, b"old").unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_secret_file(&planted, b"new").unwrap();
        assert_eq!(mode(&planted), 0o600);

        let store = dir.join("store").join("hooks.json");
        replace_file(&store, b"plain").unwrap();
        replace_secret_file(&store, b"secret").unwrap();
        assert_eq!(mode(&store), 0o600, "the rename carries the temp's mode");
        assert_eq!(mode(store.parent().unwrap()), 0o700);

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn temp_names_never_repeat_and_stay_beside_the_file() {
        let path = PathBuf::from("/tmp/pf/display-settings.json");
        let (a, b) = (unique_tmp_path(&path), unique_tmp_path(&path));
        assert_ne!(a, b, "a shared temp name is what two writers collide on");
        assert_eq!(a.parent(), path.parent(), "rename stays intra-filesystem");
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("display-settings.json."), "{name}");
        assert!(name.ends_with(".tmp"), "{name}");
    }

    /// Only a door's own box directory is walkable by others; its subdirectories and every
    /// other host's directory stay private.
    #[cfg(unix)]
    #[test]
    fn the_door_s_box_directory_is_the_one_private_dir_others_may_walk() {
        let box_dir = PathBuf::from("/var/lib/punktfunk");
        assert_eq!(private_dir_mode(true, &box_dir, &box_dir), 0o711);
        assert_eq!(private_dir_mode(false, &box_dir, &box_dir), 0o700);
        assert_eq!(
            private_dir_mode(true, &box_dir.join("seats"), &box_dir),
            0o700
        );
    }

    #[test]
    fn a_replace_lands_whole_and_a_failed_one_leaves_no_temp() {
        let dir = std::env::temp_dir().join(format!("pf-paths-replace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("store.json");
        replace_file(&path, b"first").unwrap();
        replace_secret_file(&path, b"second").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        assert_eq!(names(&dir), ["store.json"]);

        // A non-empty directory on the target refuses the rename on every platform.
        let blocked = dir.join("blocked.json");
        std::fs::create_dir_all(blocked.join("child")).unwrap();
        assert!(replace_file(&blocked, b"x").is_err());
        assert!(replace_secret_file(&blocked, b"x").is_err());
        assert_eq!(names(&dir), ["blocked.json", "store.json"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tool_that_outlasts_its_limit_is_killed() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let (program, args): (_, &[&str]) = if cfg!(windows) {
            ("cmd", &["/C", "echo hi"])
        } else {
            ("echo", &["hi"])
        };
        let done = run_within(
            Command::new(program).args(args).stdout(Stdio::piped()),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(done.status.success());
        assert_eq!(String::from_utf8_lossy(&done.stdout).trim(), "hi");

        let (program, args): (_, &[&str]) = if cfg!(windows) {
            ("ping", &["-n", "30", "127.0.0.1"])
        } else {
            ("sleep", &["30"])
        };
        let started = Instant::now();
        let hung = run_within(
            Command::new(program)
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
            Duration::from_millis(200),
        );
        assert_eq!(hung.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// A process the tool left behind holds the pipe open; the call still ends, with the output.
    #[test]
    fn a_grandchild_holding_the_pipe_does_not_hold_the_call() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        let (program, args): (_, &[&str]) = if cfg!(windows) {
            ("cmd", &["/C", "start /B ping -n 20 127.0.0.1 & echo hi"])
        } else {
            ("sh", &["-c", "sleep 20 & echo hi"])
        };
        let started = Instant::now();
        let done = run_within(
            Command::new(program).args(args).stdout(Stdio::piped()),
            Duration::from_secs(30),
        )
        .unwrap();
        assert!(String::from_utf8_lossy(&done.stdout).contains("hi"));
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
