//! Client identity, known-hosts (pinned fingerprints), and the store they share.
//!
//! Identity PEMs live in `$XDG_CONFIG_HOME/punktfunk/` (Linux) or `%APPDATA%\punktfunk`
//! (Windows). A non-empty `PUNKTFUNK_CONFIG_DIR` overrides that directory; an
//! empty value does not. `punktfunk-probe` reads this same directory, so a box
//! pairs once. On Windows the WinUI shell re-exports this module
//! (`clients/windows/src/trust.rs`) and is the settings file's only writer; the
//! session binary reads the same stores.
//!
//! Pin a host via [`persist_host`]. App settings live in [`crate::settings`] and are
//! re-exported here, so `trust::Settings` still resolves. Evidence: the migration and
//! known-hosts tests; `design/client-settings-profiles.md`.

use anyhow::{Context as _, Result};
use std::path::{Path, PathBuf};

mod hosts;
// The `quic` half of the store. wasm takes punktfunk-core WITHOUT `quic` — WebTransport is
// the QUIC layer in a browser, so quinn never builds for that target.
#[cfg(not(target_family = "wasm"))]
mod identity;
mod messages;
#[cfg(not(target_family = "wasm"))]
mod probe;

pub use crate::settings::{
    effective_settings, resolve_preset, HudCorner, MouseMode, PresentPriority, Settings,
    StatsVerbosity, TouchMode,
};
pub(crate) use hosts::StoreLock;
pub use hosts::{
    add_host, learn_from_advert, learn_mgmt_port_by_fp, persist_host, rekey_addr, same_pin,
    save_paired, touch_last_used, AddOutcome, FieldError, FileStamp, HostEdit, HostField,
    KnownHost, KnownHosts, Paired, PREV_ADDRS_MAX,
};
#[cfg(not(target_family = "wasm"))]
pub use identity::{
    device_code, device_name, load_or_create_identity, pair_and_save, pair_with_host,
    request_access_to_host, REQUEST_ACCESS_TIMEOUT,
};
pub use messages::{
    access_error_message, connect_reject_message, pair_error_message, reject_message,
};
#[cfg(not(target_family = "wasm"))]
pub use probe::{probe_known, probe_one, probe_reachable_many, PROBE_INTERVAL, PROBE_TIMEOUT};
pub use punktfunk_core::fp::{grouped, hex, parse_hex32};

/// Load a client JSON file, or `T::default()`.
///
/// A leading UTF-8 BOM is stripped — PowerShell `Set-Content -Encoding UTF8` writes
/// one, and serde refuses it. Missing files are silent; other failures warn then
/// fall back so a parse error is not an unexplained reset. A bad file never
/// blocks a stream. A file that exists but is not UTF-8 or does not parse is copied
/// to `<name>.bad` first, so the store a caller saves next does not destroy it.
pub(crate) fn load_json_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    load_json(path).unwrap_or_default()
}

/// [`load_json_or_default`] without the fallback: `None` for a missing, unreadable or
/// unparsable file, with the same warnings.
pub(crate) fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config file could not be read — every setting in it is being IGNORED"
            );
            return None;
        }
    };
    let parsed = match std::str::from_utf8(&bytes) {
        Ok(raw) => serde_json::from_str(raw.strip_prefix('\u{feff}').unwrap_or(raw))
            .map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    match parsed {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config file did not parse — falling back to defaults for it, and the \
                 settings in it are being IGNORED (fix or delete the file; a UTF-16 file \
                 is not UTF-8, re-save it as UTF-8)"
            );
            set_aside(path, &bytes);
            None
        }
    }
}

/// Keep an unusable store's bytes as `<name>.bad`, so the defaults a caller saves next do
/// not destroy them. The first copy stays until it is deleted. Best effort.
fn set_aside(path: &Path, bytes: &[u8]) {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bad");
    let bad = path.with_file_name(name);
    if bad.exists() {
        return;
    }
    let tmp = temp_sibling(&bad);
    match std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &bad)) {
        Ok(()) => tracing::warn!(kept = %bad.display(), "unusable config file kept for recovery"),
        Err(e) => {
            tracing::warn!(path = %bad.display(), error = %e, "unusable config file not kept");
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// Directory for the client identity, known hosts, and settings.
///
/// A non-empty `PUNKTFUNK_CONFIG_DIR` wins over the OS default: Linux takes
/// `$XDG_CONFIG_HOME/punktfunk` (`~/.config/punktfunk` when unset), so a Flatpak
/// lands in its own `~/.var/app` dir; other Unix `~/.config/punktfunk`; Windows
/// `%APPDATA%\punktfunk`. An empty override is ignored.
pub fn config_dir() -> Result<PathBuf> {
    let env = std::env::var_os("PUNKTFUNK_CONFIG_DIR");
    if let Some(dir) = resolve_config_dir(env.as_deref()) {
        return Ok(dir);
    }
    os_config_base()
}

/// OS default for [`config_dir`]. Missing `HOME` or `APPDATA` keeps its context
/// string. Not called when an override already won. The first Linux call adopts
/// the client files of a legacy `~/.config/punktfunk` the XDG dir lacks.
fn os_config_base() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA").context("APPDATA unset")?;
        Ok(PathBuf::from(appdata).join("punktfunk"))
    }
    #[cfg(target_os = "linux")]
    {
        let dir = crate::paths::xdg_home("XDG_CONFIG_HOME", ".config")
            .context("HOME unset")?
            .join("punktfunk");
        static ADOPTED: std::sync::Once = std::sync::Once::new();
        ADOPTED.call_once(|| {
            if let Some(home) = std::env::var_os("HOME") {
                adopt_legacy(&PathBuf::from(home).join(".config/punktfunk"), &dir);
            }
        });
        Ok(dir)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let home = std::env::var("HOME").context("HOME unset")?;
        Ok(PathBuf::from(home).join(".config/punktfunk"))
    }
}

/// Copy the `client-*` files of `legacy` that `dir` lacks. A Flatpak used to share
/// the host's `~/.config/punktfunk`, and a native box may set `XDG_CONFIG_HOME`
/// after pairing; either way the identity and pins carry over. Copies, so the
/// legacy dir stays usable by an older build. Lock and temp files stay behind.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn adopt_legacy(legacy: &Path, dir: &Path) {
    if legacy == dir {
        return;
    }
    let Ok(entries) = std::fs::read_dir(legacy) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if !text.starts_with("client-") || text.ends_with(".lock") || text.contains(".tmp-") {
            continue;
        }
        let to = dir.join(&name);
        if to.exists() {
            continue;
        }
        if let Err(e) = adopt_one(&entry.path(), dir, &to) {
            tracing::warn!(from = %entry.path().display(), error = %e, "legacy client file not adopted");
        }
    }
}

/// One file of [`adopt_legacy`]. The dir is 0700 before anything lands in it, and
/// `fs::copy` keeps the source mode, so a 0600 key stays 0600.
#[cfg(any(target_os = "linux", all(test, unix)))]
fn adopt_one(from: &Path, dir: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    std::fs::copy(from, to)?;
    tracing::info!(from = %from.display(), to = %to.display(), "adopted legacy client file");
    Ok(())
}

/// A non-empty `PUNKTFUNK_CONFIG_DIR`, or `None` when the OS default applies.
///
/// An empty value is `None`. The caller then reads `HOME` or `APPDATA`, so an
/// override never builds that error and never requires the variable to be set.
pub(crate) fn resolve_config_dir(env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    env.filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Sibling temp unique to this pid. A shared `.json.tmp` lets two writers interleave:
/// Windows sharing-violation, or one process renaming the other's half-written bytes.
/// Leftover only after a hard kill; the rename below removes it on success.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp-{}", std::process::id()));
    path.with_file_name(name)
}

/// Temp sibling then rename over the target. A plain `fs::write` truncates first, so a
/// crash or full disk leaves a torn store — and these files are how a client finds hosts.
/// Rename is atomic within a directory on Unix and Windows (`MoveFileEx` replace).
///
/// Rename is not always available. MSIX AppData virtualization can put the redirected
/// store on the package volume while the path still names `C:\Users\…`; `std::fs::rename`
/// is `MoveFileExW` without `MOVEFILE_COPY_ALLOWED`, so a cross-volume move fails with
/// `ERROR_NOT_SAME_DEVICE`. Creating files still works, which is why an install streams
/// while every setting evaporates.
///
/// A failed rename writes the target in place (same path the identity files already use)
/// and reads the bytes back — `Ok(())` alone was the silent-loss failure mode. A failed
/// temp write returns its error and leaves the target alone: an in-place write would
/// truncate the store and then fail the same way.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = temp_sibling(path);
    if let Err(e) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        store_health::record(path, &e);
        return Err(e);
    }
    let Err(e) = std::fs::rename(&tmp, path) else {
        store_health::clear();
        return Ok(());
    };
    // Drop the temp so the next writer (or a backup tool) does not see it.
    let _ = std::fs::remove_file(&tmp);
    write_in_place(path, bytes, &e)
}

/// The rename-free route: write over the target, then read it back.
fn write_in_place(path: &Path, bytes: &[u8], rename_err: &std::io::Error) -> std::io::Result<()> {
    match std::fs::write(path, bytes) {
        Ok(()) => {
            tracing::warn!(
                path = %path.display(),
                error = %rename_err,
                "atomic replace unavailable in this install; wrote the config in place instead",
            );
            // Read back: a write that returned `Ok(())` and vanished is the failure mode.
            // Only on the degraded path, so the atomic route pays nothing.
            match std::fs::read(path) {
                Ok(back) if back == bytes => {
                    store_health::clear();
                    Ok(())
                }
                Ok(_) => {
                    let e = std::io::Error::other(
                        "the file read back different from what was just written",
                    );
                    store_health::record(path, &e);
                    Err(e)
                }
                Err(reread) => {
                    store_health::record(path, &reread);
                    Err(reread)
                }
            }
        }
        // Report the in-place error (permission/space); the rename's may only say the
        // paths landed on different volumes.
        Err(direct) => {
            store_health::record(path, &direct);
            Err(direct)
        }
    }
}

/// Last persist failure, if any, so a front-end can say the store is unwritable.
///
/// Every save in this crate is fire-and-forget — a failed write must not take a stream
/// down — so an unwritable store otherwise looks healthy. One latch instead of ~15 call sites.
pub mod store_health {
    use std::path::Path;
    use std::sync::Mutex;

    static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

    pub(crate) fn record(path: &Path, err: &std::io::Error) {
        let msg = format!("{}: {err}", path.display());
        tracing::error!(store = %path.display(), error = %err, "client config not saved");
        if let Ok(mut slot) = LAST_ERROR.lock() {
            *slot = Some(msg);
        }
    }

    pub(crate) fn clear() {
        if let Ok(mut slot) = LAST_ERROR.lock() {
            *slot = None;
        }
    }

    /// Last persist failure. One latch for the whole store: any successful write clears it.
    pub fn last_error() -> Option<String> {
        LAST_ERROR.lock().ok().and_then(|s| s.clone())
    }
}

/// `store_health` is process-global: one test's successful write clears another's
/// recorded failure. Every test that writes through [`write_atomic`] holds this.
#[cfg(test)]
pub(crate) fn store_health_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// A non-empty override wins. Empty and absent leave the OS default to the caller.
    /// The helper takes the override as an argument.
    #[test]
    fn config_dir_resolution_prefers_a_non_empty_override() {
        assert_eq!(
            resolve_config_dir(Some(OsStr::new("/from-env"))).as_deref(),
            Some(Path::new("/from-env")),
        );
        assert_eq!(resolve_config_dir(Some(OsStr::new(""))), None);
        assert_eq!(resolve_config_dir(None), None);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let raw = OsStr::from_bytes(b"/from-env/\xff");
            assert_eq!(resolve_config_dir(Some(raw)).unwrap().as_os_str(), raw);
        }
    }

    /// The XDG dir takes the legacy dir's client files, keeps its own, and leaves
    /// locks, temp files and other products' files behind. The key stays 0600.
    #[cfg(unix)]
    #[test]
    fn adopting_the_legacy_dir_copies_only_missing_client_files() {
        use std::os::unix::fs::PermissionsExt;
        let root = crate::presets::tests::scratch_dir("adopt-legacy");
        let (legacy, dir) = (root.join("legacy"), root.join("xdg/punktfunk"));
        std::fs::create_dir_all(&legacy).unwrap();
        for (name, body) in [
            ("client-cert.pem", "cert"),
            ("client-key.pem", "key"),
            ("client-known-hosts.json", "old pins"),
            ("client-known-hosts.json.lock", ""),
            ("client-gtk-settings.json.tmp-7", "half"),
            ("host.env", "host"),
        ] {
            std::fs::write(legacy.join(name), body).unwrap();
        }
        let key = legacy.join("client-key.pem");
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("client-known-hosts.json"), "new pins").unwrap();

        adopt_legacy(&legacy, &dir);

        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
        assert_eq!(read("client-cert.pem").as_deref(), Some("cert"));
        assert_eq!(read("client-known-hosts.json").as_deref(), Some("new pins"));
        for left in [
            "client-known-hosts.json.lock",
            "client-gtk-settings.json.tmp-7",
            "host.env",
        ] {
            assert_eq!(read(left), None, "{left} must stay behind");
        }
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join("client-key.pem")), 0o600);
        assert_eq!(mode(&dir), 0o700);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A UTF-8 BOM must load, not fall back to `Default`. serde refuses `EF BB BF`
    /// at byte 0; PowerShell `Set-Content -Encoding UTF8` writes one.
    #[test]
    fn a_bom_does_not_turn_a_settings_file_into_defaults() {
        let dir = std::env::temp_dir().join(format!(
            "pf-client-core-bom-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let body = r#"{"codec":"av1","bitrate_kbps":42000}"#;

        let plain = dir.join("plain.json");
        std::fs::write(&plain, body).unwrap();
        let s: Settings = load_json_or_default(&plain);
        assert_eq!(s.codec, "av1");
        assert_eq!(s.bitrate_kbps, 42000);

        // Same bytes with a UTF-8 BOM must load identically.
        let bom = dir.join("bom.json");
        std::fs::write(&bom, format!("\u{feff}{body}")).unwrap();
        let s: Settings = load_json_or_default(&bom);
        assert_eq!(s.codec, "av1", "a BOM must not discard the settings file");
        assert_eq!(s.bitrate_kbps, 42000);

        // Broken JSON falls back to defaults; a missing file is first run, not a failure.
        let broken = dir.join("broken.json");
        std::fs::write(&broken, r#"{"codec":"av1",}"#).unwrap();
        let d: Settings = load_json_or_default(&broken);
        assert_eq!(d.codec, Settings::default().codec);
        let gone: Settings = load_json_or_default(&dir.join("nope.json"));
        assert_eq!(gone.codec, Settings::default().codec);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Atomic write replaces the target in one step and leaves no temp behind.
    #[test]
    fn write_atomic_replaces_and_cleans_up() {
        let _guard = store_health_lock();
        let dir = std::env::temp_dir().join(format!(
            "pf-client-core-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("store.json");
        write_atomic(&p, b"{\"a\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"a\":1}");
        write_atomic(&p, b"{\"a\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"a\":2}");
        assert!(!temp_sibling(&p).exists());
        // Scratch file is gone, not renamed aside.
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("store.json")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two writers must not share one scratch file. Same-process: proves the name
    /// varies with pid, not the interleaving.
    #[test]
    fn temp_sibling_is_per_process_and_a_sibling() {
        let p = Path::new("/tmp/pf/client-windows-settings.json");
        let t = temp_sibling(p);
        assert_eq!(t.parent(), p.parent());
        assert_eq!(
            t.file_name().unwrap().to_str().unwrap(),
            format!("client-windows-settings.json.tmp-{}", std::process::id())
        );
        // Must not collide with the store, nor look like one to `load()`.
        assert_ne!(t, p.to_path_buf());
    }

    /// When temp+rename is unavailable, bytes must still reach the target. Simulated
    /// by parking a directory on the temp sibling so the atomic leg cannot complete.
    #[test]
    fn a_failed_temp_write_leaves_the_store_alone() {
        let _guard = store_health_lock();
        let dir = std::env::temp_dir().join(format!(
            "pf-client-core-inplace-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("store.json");
        std::fs::write(&p, b"{\"old\":true}").unwrap();

        std::fs::create_dir_all(temp_sibling(&p)).unwrap();
        assert!(temp_sibling(&p).is_dir());

        assert!(write_atomic(&p, b"{\"new\":true}").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"old\":true}");
        assert!(store_health::last_error().is_some());

        // A failed rename writes in place. Success must be readable back.
        let cross_volume = std::io::Error::other("not the same device");
        write_in_place(&p, b"{\"new\":true}", &cross_volume).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{\"new\":true}");
        assert_eq!(store_health::last_error(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// When the in-place fallback also fails, the error must surface, not be swallowed.
    #[test]
    fn a_failed_rename_still_persists_the_write() {
        let _guard = store_health_lock();
        let dir = std::env::temp_dir().join(format!(
            "pf-client-core-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let ok = dir.join("store.json");
        write_atomic(&ok, b"{}").unwrap();
        assert_eq!(store_health::last_error(), None);

        // Directory in the target's place defeats both rename and in-place write.
        let blocked = dir.join("blocked.json");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(blocked.join("occupant"), b"x").unwrap();
        assert!(write_atomic(&blocked, b"{\"a\":1}").is_err());
        let reported = store_health::last_error().expect("an unwritable store must be reported");
        assert!(
            reported.contains("blocked.json"),
            "the report names the store: {reported}"
        );
        assert!(!temp_sibling(&blocked).exists());

        // A later success clears the latch.
        write_atomic(&ok, b"{\"a\":2}").unwrap();
        assert_eq!(store_health::last_error(), None);
        assert_eq!(std::fs::read_to_string(&ok).unwrap(), "{\"a\":2}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
