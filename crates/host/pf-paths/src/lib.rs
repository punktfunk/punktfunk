//! Host config-dir and owner-private file helpers.
//!
//! Leaf crate so `pf-media`, `pf-vdisplay`, and the orchestrator share these
//! without depending on `gamestream`. Std + `tracing` only.
//!
//! [`config_dir`] is XDG / `%ProgramData%`, overridable with `PUNKTFUNK_CONFIG_DIR`.
//! [`seat_home`] is the XDG data dir a seat's nested Steam runs under.
//! [`create_private_dir`] / [`write_secret_file`] apply 0700 / 0600 on Unix and a
//! SYSTEM/Administrators DACL on Windows. [`replace_file`] / [`replace_secret_file`] /
//! [`replace_users_readable_file`] are the one temp-and-rename writer for stores; the last
//! is for the two files every local account may read, `mgmt-endpoint` and `tray-token`.
//! [`system32`] is how a privileged process names a Windows system tool; [`remove_device`]
//! runs one. [`seat`] is the Windows multi-seat marker. [`tray_autostart`] is the tray's
//! per-user autostart entry on Linux.
#![forbid(unsafe_code)]

use std::path::PathBuf;

pub mod seat;
#[cfg(not(windows))]
pub mod tray_autostart;

/// `$XDG_RUNTIME_DIR/punktfunk-gamescope-ei` (per-user 0700), or `/tmp/…`
/// when the runtime dir is unset. `pf-vdisplay` writes it under the session
/// env lock; `pf-inject` reads it after that env is applied.
#[cfg(target_os = "linux")]
pub fn gamescope_ei_socket_file() -> PathBuf {
    gamescope_ei_relay("punktfunk-gamescope-ei")
}

/// `$XDG_RUNTIME_DIR/punktfunk-gamescope-{id}-ei`. Isolated bare-spawn
/// sessions (`design/gamescope-multiuser.md`) must not overwrite each
/// other's relay; `id` is `pf-vdisplay`'s `SessionIsolation`.
#[cfg(target_os = "linux")]
pub fn gamescope_ei_socket_file_for(id: &str) -> PathBuf {
    gamescope_ei_relay(&format!("punktfunk-gamescope-{id}-ei"))
}

/// What a seat device directory is called. [`gamescope_seat_dev_dir`] writes the
/// name, [`is_gamescope_seat_dev_dir`] reads it, and one spelling serves both:
/// `pf-inject` takes a node number off a sibling seat by deleting inside it.
const SEAT_DEV_PREFIX: &str = "punktfunk-gamescope-";
const SEAT_DEV_SUFFIX: &str = "-dev";

/// `$XDG_RUNTIME_DIR/punktfunk-gamescope-{id}-dev` — the device nodes a sandboxed
/// seat may open. `hostdev/` is where its sandbox mounts the real `/dev`, so the
/// links under `input/` and `hidraw/` resolve there and nowhere on this side.
/// `pf-vdisplay` builds the sandbox, `pf-inject` writes the links; path only, the
/// caller creates it 0700 ([`create_private_dir`]).
#[cfg(target_os = "linux")]
pub fn gamescope_seat_dev_dir(id: &str) -> PathBuf {
    gamescope_ei_relay(&format!("{SEAT_DEV_PREFIX}{id}{SEAT_DEV_SUFFIX}"))
}

/// Is this one of our seat device directories? The name is the whole test: another
/// seat is a directory we made, never one that happens to hold the same folders.
/// Ungated — `pf-inject` carries the link arithmetic on every target.
pub fn is_gamescope_seat_dev_dir(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix(SEAT_DEV_PREFIX))
        .and_then(|n| n.strip_suffix(SEAT_DEV_SUFFIX))
        .is_some_and(|id| !id.is_empty())
}

#[cfg(target_os = "linux")]
fn gamescope_ei_relay(name: &str) -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|s| !s.is_empty()) {
        Some(rt) => PathBuf::from(rt).join(name),
        None => PathBuf::from("/tmp").join(name),
    }
}

/// `$XDG_DATA_HOME/punktfunk/seats` — every seat's home and record. Path only.
#[cfg(target_os = "linux")]
pub fn seats_dir() -> PathBuf {
    data_dir().join("seats")
}

/// `$XDG_DATA_HOME/punktfunk/seats/<id>` — the `HOME` a seat profile's nested Steam runs
/// under. `id` is the profile id. Path only; the caller creates it 0700
/// ([`create_private_dir`]).
#[cfg(target_os = "linux")]
pub fn seat_home(id: &str) -> PathBuf {
    seats_dir().join(id)
}

/// `…/seats/<id>.json` — what pre-warming that profile's seat needs, beside its home rather
/// than inside it: the home is a `HOME` Steam owns, and a file of ours in it is one Steam may
/// clean up.
#[cfg(target_os = "linux")]
pub fn seat_record(id: &str) -> PathBuf {
    seats_dir().join(format!("{id}.json"))
}

/// `$XDG_DATA_HOME/punktfunk`, else `~/.local/share/punktfunk`. Separate from
/// [`config_dir`]: it holds installs (a seat's Steam, managed emulators), and the plugin
/// runner shares nothing under the config dir.
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

/// Tightens an already-existing dir. Windows refuses a reparse point
/// ([`reject_reparse_point`]): hardening a junction would harden the
/// attacker-chosen target while the link stays theirs. Default
/// `%ProgramData%` ACLs grant Users *create*.
pub fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mode = private_dir_mode(seat::is_door(), dir, &config_dir());
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
    #[cfg(not(unix))]
    {
        #[cfg(windows)]
        reject_reparse_point(dir)?;
        let r = std::fs::create_dir_all(dir);
        #[cfg(windows)]
        if r.is_ok() {
            let _hold = hold_plain_directory(dir)?;
            restrict_dir_to_system_admins(dir, first_hardening_of(dir));
        }
        r
    }
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

/// First hardening of `dir` in this process — the pass that re-owns recursively.
///
/// A planted tree exists before the host starts, so one deep pass is enough.
/// Library CRUD calls `create_private_dir` per write; repeating `/T` would
/// re-walk recordings and the art cache.
#[cfg(windows)]
fn first_hardening_of(dir: &std::path::Path) -> bool {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut s| s.insert(dir.to_path_buf()))
        .unwrap_or(false)
}

/// Re-own to Administrators then re-ACL. A planted file's creator keeps
/// `WRITE_DAC`, so ACL-only would let them put access back.
#[cfg(windows)]
pub fn restrict_existing_secret_file(path: &std::path::Path) {
    if !path.exists() {
        return;
    }
    let icacls = system32("icacls.exe");
    let _ = std::process::Command::new(&icacls)
        .arg(path.as_os_str())
        .args(["/setowner", "*S-1-5-32-544"]) // BUILTIN\Administrators
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
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
    let root = std::env::var("SystemRoot")
        .or_else(|_| std::env::var("WINDIR"))
        .unwrap_or_else(|_| r"C:\Windows".to_string());
    format!(r"{root}\System32\{rel}")
}

/// Windows PowerShell under System32, for [`system32`].
pub const POWERSHELL: &str = r"WindowsPowerShell\v1.0\powershell.exe";

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

/// Default `%ProgramData%` lets `BUILTIN\Users` create and become
/// `CREATOR OWNER`. Re-owns to Administrators, resets the dir's own ACL, strips
/// inheritance, grants SYSTEM/Administrators `(OI)(CI)(F)` and nobody else: a
/// file a local account may read carries its own ACE. Hard-coded SIDs; never fatal.
#[cfg(windows)]
fn restrict_dir_to_system_admins(dir: &std::path::Path, deep: bool) {
    let icacls = system32("icacls.exe");
    // Re-own to Administrators first: an owner keeps WRITE_DAC.
    // `deep` (once per dir per process) also re-owns contents; directory-only
    // left planted files still writable by their creator.
    let mut own = std::process::Command::new(&icacls);
    own.arg(dir.as_os_str())
        .args(["/setowner", "*S-1-5-32-544"]); // BUILTIN\Administrators
    if deep {
        own.args(["/T", "/C", "/Q"]); // recurse, continue on error, quiet
    }
    let _ = own
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // `/inheritance:r` drops inherited ACEs only. A planted dir keeps an explicit
    // `Everyone:(F)` through it, so reset the dir's own ACL first (not `/T`: the
    // plugin runner's grants live on children and are re-applied only by `enable`).
    let _ = std::process::Command::new(&icacls)
        .arg(dir.as_os_str())
        .args(["/reset", "/C", "/Q"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    // Do not grant OWNER `(OI)(CI)(F)` — WRITE_DAC comes back on every child
    // the attacker already owned. SYSTEM and Administrators cover writers here.
    let status = std::process::Command::new(&icacls)
        .arg(dir.as_os_str())
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)(F)", // NT AUTHORITY\SYSTEM
            "/grant:r",
            "*S-1-5-32-544:(OI)(CI)(F)", // BUILTIN\Administrators
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {}
        _ => tracing::warn!(
            dir = %dir.display(),
            "config-dir DACL hardening did not fully succeed — a local user may be able to plant config files"
        ),
    }
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
    let status = icacls
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
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
    let status = std::process::Command::new(icacls)
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
        .stderr(std::process::Stdio::null())
        .status()?;
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
        let global = gamescope_ei_socket_file();
        let per = gamescope_ei_socket_file_for("cafe0123");
        assert_eq!(per.parent(), global.parent());
        assert_eq!(
            per.file_name().unwrap().to_str().unwrap(),
            "punktfunk-gamescope-cafe0123-ei"
        );
        // The device directory is that seat's alone, beside its relay.
        let dev = gamescope_seat_dev_dir("cafe0123");
        assert_eq!(dev.parent(), global.parent());
        assert_ne!(dev, gamescope_seat_dev_dir("dead0001"));
        // What we write is what we recognise, or a prune reaches somebody else's directory.
        assert!(is_gamescope_seat_dev_dir(&dev));
        assert!(!is_gamescope_seat_dev_dir(&per), "the relay is not a seat");
        for other in [
            "hidraw",
            "punktfunk-gamescope--dev",
            "some-app-dev",
            "pipewire-0",
            "",
        ] {
            assert!(
                !is_gamescope_seat_dev_dir(std::path::Path::new(other)),
                "{other}"
            );
        }
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
}
