//! Client identity, known-hosts (pinned fingerprints), and app settings.
//!
//! Identity PEMs live in `~/.config/punktfunk/` (Linux) or `%APPDATA%\punktfunk`
//! (Windows). A non-empty `PUNKTFUNK_CONFIG_DIR` overrides that directory; an
//! empty value does not. `punktfunk-probe` reads this same directory, so a box
//! pairs once. On Windows the WinUI shell re-exports this module
//! (`clients/windows/src/trust.rs`) and is the settings file's only writer; the
//! session binary reads the same stores.
//!
//! Pin a host via [`persist_host`]. Settings resolve through [`effective_settings`].
//! Evidence: the migration and known-hosts tests below;
//! `design/client-settings-profiles.md`.

use crate::presets::{PresetsFile, Resolution, StreamPreset};
use anyhow::{Context, Result};
// The `quic` half of the store. wasm takes punktfunk-core WITHOUT `quic` — WebTransport is
// the QUIC layer in a browser, so quinn never builds for that target — and these are the
// only items here that reach for it. Everything else in this file is data the console reads
// on every platform.
#[cfg(not(target_family = "wasm"))]
use anyhow::anyhow;
#[cfg(not(target_family = "wasm"))]
use punktfunk_core::client::NativeClient;
#[cfg(not(target_family = "wasm"))]
use punktfunk_core::quic::endpoint;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Load a client JSON file, or `T::default()`.
///
/// A leading UTF-8 BOM is stripped — PowerShell `Set-Content -Encoding UTF8` writes
/// one, and serde refuses it. Missing files are silent; other failures warn then
/// fall back so a parse error is not an unexplained reset. A bad file never
/// blocks a stream.
pub(crate) fn load_json_or_default<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    load_json(path).unwrap_or_default()
}

/// [`load_json_or_default`] without the fallback: `None` for a missing, unreadable or
/// unparsable file, with the same warnings.
pub(crate) fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config file could not be read — every setting in it is being IGNORED \
                 (a UTF-16 file reads as invalid UTF-8 here; re-save it as UTF-8)"
            );
            return None;
        }
    };
    match serde_json::from_str(raw.strip_prefix('\u{feff}').unwrap_or(&raw)) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "config file did not parse — falling back to defaults for it, and the \
                 settings in it are being IGNORED (fix or delete the file)"
            );
            None
        }
    }
}

/// Directory for the client identity, known hosts, and settings.
///
/// A non-empty `PUNKTFUNK_CONFIG_DIR` wins over `~/.config/punktfunk` (`HOME`)
/// or `%APPDATA%\punktfunk`. An empty value is ignored. The OS default is read
/// only when no override is set.
pub fn config_dir() -> Result<PathBuf> {
    let env = std::env::var_os("PUNKTFUNK_CONFIG_DIR");
    if let Some(dir) = resolve_config_dir(env.as_deref()) {
        return Ok(dir);
    }
    os_config_base()
}

/// OS default for [`config_dir`]. Missing `HOME` or `APPDATA` keeps its context
/// string. Not called when an override already won.
fn os_config_base() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA").context("APPDATA unset")?;
        Ok(PathBuf::from(appdata).join("punktfunk"))
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").context("HOME unset")?;
        Ok(PathBuf::from(home).join(".config/punktfunk"))
    }
}

/// A non-empty `PUNKTFUNK_CONFIG_DIR`, or `None` when the OS default applies.
///
/// An empty value is `None`. The caller then reads `HOME` or `APPDATA`, so an
/// override never builds that error and never requires the variable to be set.
pub(crate) fn resolve_config_dir(env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    env.filter(|value| !value.is_empty()).map(PathBuf::from)
}

/// Persistent mTLS identity, generated once and presented on every connect.
///
/// The private key is owner-only wherever the directory is. Unix uses mode 0600.
/// Windows drops inherited ACEs and grants the owner, including when
/// `PUNKTFUNK_CONFIG_DIR` points outside `%APPDATA%`.
#[cfg(not(target_family = "wasm"))]
pub fn load_or_create_identity() -> Result<(String, String)> {
    let dir = config_dir()?;
    let (cp, kp) = (dir.join("client-cert.pem"), dir.join("client-key.pem"));
    if let (Ok(c), Ok(k)) = (std::fs::read_to_string(&cp), std::fs::read_to_string(&kp)) {
        // Older Unix builds left the key world-readable. Re-lock on load. Best-effort:
        // a read-only store still returns the key it already has. Windows keys only
        // ever lived in the per-user `%APPDATA%`, so nothing there needs a re-lock.
        #[cfg(unix)]
        lock_identity_perms(&dir, &kp);
        return Ok((c, k));
    }
    let (c, k) = endpoint::generate_identity().map_err(|e| anyhow!("generate identity: {e}"))?;
    std::fs::create_dir_all(&dir)?;
    // Unix: the directory is 0700 before the key is written. Windows locks the key
    // file alone in `write_private_key`; the directory keeps the ACL it came with.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::write(&cp, &c)?;
    write_private_key(&kp, k.as_bytes())?;
    tracing::info!(cert = %cp.display(), "generated client identity");
    Ok((c, k))
}

/// Write the mTLS private key owner-only.
///
/// Unix creates it mode 0600. Writing first and then chmod would expose the
/// bytes at the umask default. Windows creates an empty file, locks it to the
/// owner, then writes the bytes. A failed lock removes the file.
#[cfg(not(target_family = "wasm"))]
fn write_private_key(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
    }
    #[cfg(windows)]
    {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        std::fs::write(path, [])?;
        if let Err(e) = restrict_to_owner(path) {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        std::fs::write(path, bytes)?;
    }
    Ok(())
}

/// `icacls.exe` from `SystemRoot`, or `icacls` on `PATH`.
#[cfg(windows)]
fn icacls_exe() -> String {
    std::env::var("SystemRoot")
        .map(|root| format!("{root}\\System32\\icacls.exe"))
        .unwrap_or_else(|_| "icacls".to_string())
}

/// One `icacls` call with no console window: the WinUI shell has no console, so a
/// plain spawn would open one. Failure is `restrict client key`.
#[cfg(windows)]
fn run_icacls(path: &std::path::Path, args: &[&str]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let status = std::process::Command::new(icacls_exe())
        .arg(path)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("restrict client key")?;
    if status.success() {
        return Ok(());
    }
    anyhow::bail!("restrict client key: {status}");
}

/// Owner-only ACL on one file. `/reset` runs first because `/inheritance:r`
/// leaves an explicit grant in place.
#[cfg(windows)]
fn restrict_to_owner(path: &std::path::Path) -> Result<()> {
    run_icacls(path, &["/reset"])?;
    run_icacls(path, &["/inheritance:r", "/grant:r", "*S-1-3-4:(F)"])
}

/// Best-effort dir 0700 / key 0600 on an existing store. Errors ignored: this never
/// loosens perms, so a failure leaves what was already there.
#[cfg(all(unix, not(target_family = "wasm")))]
fn lock_identity_perms(dir: &std::path::Path, key: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let _ = std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600));
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
/// and reads the bytes back — `Ok(())` alone was the silent-loss failure mode. The
/// temp+rename stays the normal route everywhere it works.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = temp_sibling(path);
    let atomic = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, path));
    let Err(e) = atomic else {
        store_health::clear();
        return Ok(());
    };
    // Drop the temp so the next writer (or a backup tool) does not see it.
    let _ = std::fs::remove_file(&tmp);
    match std::fs::write(path, bytes) {
        Ok(()) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
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
        // Both routes failed. Report the in-place error (permission/space); the rename's
        // may only say the paths landed on different volumes.
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

pub fn hex(fp: &[u8; 32]) -> String {
    fp.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// One trusted host: pinned cert fingerprint, how trust was granted, last-reached address.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(remote = "Self")]
pub struct KnownHost {
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// SHA-256 of the host certificate, lowercase hex — the pin for later connects.
    pub fp_hex: String,
    /// True if trust came from the SPAKE2 PIN ceremony (vs. trust-on-first-use).
    pub paired: bool,
    /// Unix seconds of the last successful connect. `default` so older stores load.
    #[serde(default)]
    pub last_used: Option<u64>,
    /// Wake-on-LAN MACs (`aa:bb:cc:dd:ee:ff`) learned from mDNS `mac` TXT while online,
    /// so we can wake a host that has stopped advertising. `default`; empty until learned.
    #[serde(default)]
    pub mac: Vec<String>,
    /// OS-identity chain (`windows` | `macos` | `linux[/<family>][/<id>]`) from mDNS `os`
    /// TXT, so the card icon survives sleep. `default`; elided when empty.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub os: String,
    /// Management-API port (mDNS `mgmt` TXT), distinct from `port` (native QUIC). Persisted
    /// so a host that moved off 47990 stays reachable once the advert is gone. `None` =
    /// never learned; resolve via [`KnownHost::effective_mgmt_port`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mgmt_port: Option<u16>,
    /// Share this machine's clipboard with this host (design/clipboard-and-file-transfer.md).
    /// Per-host, not global. Default off; the host must also advertise `HOST_CAP_CLIPBOARD`.
    #[serde(default)]
    pub clipboard_sync: bool,
    /// Default settings preset for a plain click (design/client-settings-profiles.md).
    /// `None` or a deleted id → global defaults; a dangling binding never blocks a connect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    /// Extra preset cards for this host; order = card order. Presentation only — not
    /// the default (`preset_id`). Duplicates and dangling ids are dropped at resolve.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pinned_presets: Vec<String>,
    /// Library title id → preset id: what a launch of that title streams with, beating
    /// `preset_id`. Keyed here rather than in the catalog for the §4.1 reason the host
    /// binding is — the catalog owns no host keys, and a title id is only unique per host.
    /// Dangling ids resolve to nothing, exactly like a dangling `preset_id`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub game_presets: BTreeMap<String, String>,
    /// Stable record id, minted lazily, never rewritten. Survives rename and DHCP.
    /// No lookup here is keyed by it — `fp_hex` / `addr:port` stay the keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Addresses this host left or was advertised at, newest first, at most
    /// [`PREV_ADDRS_MAX`]. A host lives at more than one — its LAN lease at home, a
    /// Tailscale address anywhere — so when `addr` goes silent `probe_known` asks these too,
    /// and moves there only when the pin answers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prev_addrs: Vec<String>,
}

/// Pre-rename binding keys (`design/preset-rename.md`): read when the new key is absent,
/// and written beside it so a client older than the rename keeps its bindings.
const LEGACY_HOST_KEYS: [(&str, &str); 3] = [
    ("preset_id", "profile_id"),
    ("pinned_presets", "pinned_profiles"),
    ("game_presets", "game_profiles"),
];

impl<'de> Deserialize<'de> for KnownHost {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut v = serde_json::Value::deserialize(d)?;
        if let Some(host) = v.as_object_mut() {
            for (new, old) in LEGACY_HOST_KEYS {
                // The new key wins: only this client writes it, and always beside an equal
                // old one, so a record carrying it was last saved here.
                if let Some(legacy) = host.remove(old) {
                    host.entry(new).or_insert(legacy);
                }
            }
        }
        KnownHost::deserialize(v).map_err(serde::de::Error::custom)
    }
}

impl Serialize for KnownHost {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut v = KnownHost::serialize(self, serde_json::value::Serializer)
            .map_err(serde::ser::Error::custom)?;
        if let Some(host) = v.as_object_mut() {
            for (new, old) in LEGACY_HOST_KEYS {
                if let Some(value) = host.get(new).cloned() {
                    host.insert(old.into(), value);
                }
            }
        }
        v.serialize(s)
    }
}

/// How many left-behind addresses a host keeps.
pub const PREV_ADDRS_MAX: usize = 3;

impl Default for KnownHost {
    /// Blank record with a fresh stable id — construction sites use
    /// `KnownHost { name, addr, port, ..Default::default() }`, so a new field here cannot
    /// silently omit it.
    fn default() -> KnownHost {
        KnownHost {
            name: String::new(),
            addr: String::new(),
            port: 9777,
            fp_hex: String::new(),
            paired: false,
            last_used: None,
            mac: Vec::new(),
            os: String::new(),
            mgmt_port: None,
            clipboard_sync: false,
            preset_id: None,
            pinned_presets: Vec::new(),
            game_presets: BTreeMap::new(),
            id: Some(crate::presets::new_record_uuid()),
            prev_addrs: Vec::new(),
        }
    }
}

impl KnownHost {
    /// Learned mgmt port, else compiled-in 47990. Library/art calls must use this, not
    /// [`crate::library::DEFAULT_MGMT_PORT`] — that constant is the fallback, not the answer.
    pub fn effective_mgmt_port(&self) -> u16 {
        self.mgmt_port.unwrap_or(crate::library::DEFAULT_MGMT_PORT)
    }

    /// Re-point at `addr:port`, remembering the address it leaves. `false`, and nothing
    /// changed, when already there.
    pub fn move_to(&mut self, addr: &str, port: u16) -> bool {
        if self.addr == addr && self.port == port {
            return false;
        }
        let old = std::mem::replace(&mut self.addr, addr.to_string());
        self.prev_addrs.retain(|a| a != addr && *a != old);
        if old != addr {
            self.prev_addrs.insert(0, old);
        }
        self.prev_addrs.truncate(PREV_ADDRS_MAX);
        self.port = port;
        true
    }

    /// Pins that still exist, in card order, no duplicates. Dangling ids disappear —
    /// a pin is presentation state, never an error.
    pub fn resolved_pins<'a>(&self, catalog: &'a PresetsFile) -> Vec<&'a StreamPreset> {
        let mut out: Vec<&StreamPreset> = Vec::new();
        for id in &self.pinned_presets {
            if out.iter().any(|p| p.id == *id) {
                continue;
            }
            if let Some(p) = catalog.find_by_id(id) {
                out.push(p);
            }
        }
        out
    }

    /// This title's binding, if it has one. Not resolved against the catalog here —
    /// [`resolve_preset`] drops a dangling id, the same way it does for `preset_id`.
    pub fn preset_for_game(&self, game_id: &str) -> Option<&str> {
        self.game_presets.get(game_id).map(String::as_str)
    }

    /// Bind (or with `None`, clear) a title's preset. Idempotent, and a clear removes
    /// the key rather than storing an empty one — the map is skipped when serialising,
    /// so an unbound host keeps writing no `game_presets` at all.
    pub fn bind_game_preset(&mut self, game_id: &str, preset_id: Option<&str>) {
        match preset_id {
            Some(id) => drop(
                self.game_presets
                    .insert(game_id.to_string(), id.to_string()),
            ),
            None => drop(self.game_presets.remove(game_id)),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct KnownHosts {
    pub hosts: Vec<KnownHost>,
}

impl KnownHosts {
    fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("client-known-hosts.json"))
    }

    /// The store, minting ids on records that lack one. Written back here, not "on the
    /// next save", so the id a caller sees is the one on disk. A read-only dir re-mints
    /// in memory; no lookup is keyed by id yet.
    pub fn load() -> KnownHosts {
        let mut k = Self::read();
        if k.mint_missing_ids() {
            let _ = k.save();
        }
        k
    }

    /// The store as on disk — no mint, so no write.
    ///
    /// [`KnownHosts::load`]'s mint is a write: two processes against a pre-mint store each
    /// mint a different id and race to save. A read-only consumer cannot take part in that.
    pub fn read() -> KnownHosts {
        Self::path()
            .map(|p| load_json_or_default(&p))
            .unwrap_or_default()
    }

    /// Mint a stable id on every record that lacks one. `true` = needs persisting.
    /// Idempotent: a store that has been through it once is byte-identical.
    pub fn mint_missing_ids(&mut self) -> bool {
        let mut minted = false;
        for h in &mut self.hosts {
            if h.id.as_deref().is_none_or(str::is_empty) {
                h.id = Some(crate::presets::new_record_uuid());
                minted = true;
            }
        }
        minted
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path()?;
        std::fs::create_dir_all(p.parent().unwrap())?;
        // Temp+rename: losing this file to a torn write costs the user every pairing.
        write_atomic(&p, serde_json::to_string_pretty(self)?.as_bytes())?;
        // Omarchy menu mirrors this store; save() is the one door every mutation walks.
        // No-op unless `--omarchy-menu on` — a scoped test HOME never has that.
        // `desktop` too: `trust` is portable, and a TV build has no omarchy_menu to call.
        #[cfg(all(feature = "desktop", target_os = "linux"))]
        crate::omarchy_menu::sync_if_enabled();
        Ok(())
    }

    /// The record pinned to `fp_hex`. An empty fingerprint is not a key — it would match
    /// every not-yet-paired placeholder, and the first one is never the one meant.
    pub fn find_by_fp(&self, fp_hex: &str) -> Option<&KnownHost> {
        if fp_hex.is_empty() {
            return None;
        }
        self.hosts.iter().find(|h| h.fp_hex == fp_hex)
    }

    /// Index of the record an `addr:port` lookup resolves to (so mutators avoid a second
    /// borrow).
    ///
    /// A real fingerprint beats a placeholder; among real ones the last record wins —
    /// records are only appended by a trust decision. Lookup order, not authorisation:
    /// the pin still has to match the cert the host presents.
    pub fn index_by_addr(&self, addr: &str, port: u16) -> Option<usize> {
        let mut best: Option<usize> = None;
        for (i, h) in self.hosts.iter().enumerate() {
            if h.addr != addr || h.port != port {
                continue;
            }
            let better = match best {
                None => true,
                Some(b) => !h.fp_hex.is_empty() || self.hosts[b].fp_hex.is_empty(),
            };
            if better {
                best = Some(i);
            }
        }
        best
    }

    pub fn find_by_addr(&self, addr: &str, port: u16) -> Option<&KnownHost> {
        self.index_by_addr(addr, port).map(|i| &self.hosts[i])
    }

    /// Index of the unpinned placeholder saved at `addr:port`. A record pinned there is an
    /// identity the address does not name on its own: both OS installs of a dual-boot box
    /// answer at one lease.
    pub fn placeholder_at(&self, addr: &str, port: u16) -> Option<usize> {
        self.hosts
            .iter()
            .position(|h| h.fp_hex.is_empty() && h.addr == addr && h.port == port)
    }

    /// Index of the record a dial is about. With a pin: the record pinned to it, else the
    /// placeholder at `addr:port` waiting for one — never a record pinned to another
    /// fingerprint, which at a shared address is the other OS of a dual-boot box, with its
    /// own name, binding and clipboard. `Some("")` is a card saved without a pin: its
    /// placeholder only. `None` is a bare typed address: whatever it answers with
    /// ([`KnownHosts::index_by_addr`]).
    pub fn resolve_index(&self, fp_hex: Option<&str>, addr: &str, port: u16) -> Option<usize> {
        let Some(fp_hex) = fp_hex else {
            return self.index_by_addr(addr, port);
        };
        (!fp_hex.is_empty())
            .then(|| self.hosts.iter().position(|h| h.fp_hex == fp_hex))
            .flatten()
            .or_else(|| self.placeholder_at(addr, port))
    }

    pub fn resolve(&self, fp_hex: Option<&str>, addr: &str, port: u16) -> Option<&KnownHost> {
        self.resolve_index(fp_hex, addr, port)
            .map(|i| &self.hosts[i])
    }

    /// Drop the record pinned to `fp_hex`. An empty fingerprint removes NOTHING: `retain`
    /// on `!= ""` would delete every not-yet-paired host at once, which is how one Forget
    /// used to take the whole set. Placeholders go through [`KnownHosts::remove_card`].
    pub fn remove_by_fp(&mut self, fp_hex: &str) -> bool {
        if fp_hex.is_empty() {
            return false;
        }
        let before = self.hosts.len();
        self.hosts.retain(|h| h.fp_hex != fp_hex);
        self.hosts.len() != before
    }

    /// Index of the record a UI card names: its stable [`KnownHost::id`], falling back to
    /// `addr:port` for a record minted before ids existed. Deliberately never keyed by a
    /// fingerprint — a card for an unpaired host carries an empty one.
    pub fn index_of_card(&self, id: Option<&str>, addr: &str, port: u16) -> Option<usize> {
        if let Some(id) = id.filter(|i| !i.is_empty()) {
            if let Some(i) = self.hosts.iter().position(|h| h.id.as_deref() == Some(id)) {
                return Some(i);
            }
        }
        self.index_by_addr(addr, port)
    }

    /// Drop exactly the record a card names — one record, never a class of them.
    pub fn remove_card(&mut self, id: Option<&str>, addr: &str, port: u16) -> bool {
        match self.index_of_card(id, addr, port) {
            Some(i) => {
                self.hosts.remove(i);
                true
            }
            None => false,
        }
    }

    /// Insert or refresh an entry, keyed by fingerprint. `paired` only ever upgrades
    /// (a later TOFU connect must not demote a PIN-paired host).
    pub fn upsert(&mut self, entry: KnownHost) {
        if let Some(h) = self.hosts.iter_mut().find(|h| h.fp_hex == entry.fp_hex) {
            // A label the user chose is theirs. A name that is empty, or is just the address,
            // is one the caller synthesised for want of anything better — re-pairing used to
            // replace "Desk" with "192.168.1.50".
            if !entry.name.is_empty() && (entry.name != entry.addr || h.name.is_empty()) {
                h.name = entry.name;
            }
            h.addr = entry.addr;
            h.port = entry.port;
            h.paired |= entry.paired;
            // A refresh without a timestamp must not erase the stored one.
            if entry.last_used.is_some() {
                h.last_used = entry.last_used;
            }
            // A trust-decision upsert carries no MAC — do not wipe learned ones.
            if !entry.mac.is_empty() {
                h.mac = entry.mac;
            }
            // Same for the OS chain: only a carrier moves it.
            if !entry.os.is_empty() {
                h.os = entry.os;
            }
            // Same for mgmt port: `None` on reconnect must not clear a learned 47991.
            if entry.mgmt_port.is_some() {
                h.mgmt_port = entry.mgmt_port;
            }
            // User-set fields a refresh never carries: clipboard, preset, pins,
            // per-game bindings, id. Only an upsert carrying a value moves one.
            if entry.clipboard_sync {
                h.clipboard_sync = true;
            }
            if entry.preset_id.is_some() {
                h.preset_id = entry.preset_id;
            }
            if !entry.pinned_presets.is_empty() {
                h.pinned_presets = entry.pinned_presets;
            }
            if !entry.game_presets.is_empty() {
                h.game_presets = entry.game_presets;
            }
            if h.id.as_deref().is_none_or(str::is_empty) {
                h.id = entry.id;
            }
        } else {
            self.hosts.push(entry);
        }
    }

    /// [`upsert`](Self::upsert) for an authorised trust decision (PIN, TOFU accept,
    /// delegated, headless pair). Also retires the fp-less placeholders claiming the
    /// same `addr:port`, whose pin this decision is.
    ///
    /// A record carrying a DIFFERENT fingerprint survives: an address is not an
    /// identity. Both OS installs of a dual-boot box answer at one lease with one MAC
    /// and a certificate each, so retiring by address takes the sibling the user
    /// paired. A re-keyed host leaves its old card behind for one Forget.
    /// Box fields (MAC, OS, preset, pins, last_used) ride onto the survivor. Not
    /// carried: `paired`, `clipboard_sync` (cert decisions), and the stable id (a deep
    /// link must not silently retarget).
    ///
    /// Discovery and wake re-key stay on plain `upsert` — an unauthenticated advert
    /// must not delete a saved host by claiming its address.
    pub fn upsert_trusted(&mut self, entry: KnownHost) {
        let (addr, port, fp_hex) = (entry.addr.clone(), entry.port, entry.fp_hex.clone());
        self.upsert(entry);
        // Nothing to supersede with: an fp-less record is a placeholder, not an identity.
        if fp_hex.is_empty() {
            return;
        }
        let (keep, retired): (Vec<KnownHost>, Vec<KnownHost>) = std::mem::take(&mut self.hosts)
            .into_iter()
            .partition(|h| !(h.addr == addr && h.port == port && h.fp_hex.is_empty()));
        self.hosts = keep;
        if retired.is_empty() {
            return;
        }
        let Some(h) = self.hosts.iter_mut().find(|h| h.fp_hex == fp_hex) else {
            return;
        };
        for old in retired {
            tracing::info!(
                addr = %addr, port, kept_fp = %fp_hex,
                "retiring the unpinned placeholder this decision pins"
            );
            if h.mac.is_empty() {
                h.mac = old.mac;
            }
            if h.os.is_empty() {
                h.os = old.os;
            }
            if h.mgmt_port.is_none() {
                h.mgmt_port = old.mgmt_port;
            }
            if h.preset_id.is_none() {
                h.preset_id = old.preset_id;
            }
            if h.pinned_presets.is_empty() {
                h.pinned_presets = old.pinned_presets;
            }
            if h.game_presets.is_empty() {
                h.game_presets = old.game_presets;
            }
            if h.last_used.is_none() {
                h.last_used = old.last_used;
            }
        }
    }
}

/// Load-upsert-save: the pin every trust decision (TOFU, PIN, delegated, headless) ends in.
pub fn persist_host(name: &str, addr: &str, port: u16, fp_hex: &str, paired: bool) -> Result<()> {
    let mut known = KnownHosts::load();
    // `..Default::default()` so user-set fields arrive uncarried; a literal would
    // reset them on re-pair. `upsert_trusted`: this is the authorised decision.
    known.upsert_trusted(KnownHost {
        name: name.to_string(),
        addr: addr.to_string(),
        port,
        fp_hex: fp_hex.to_string(),
        paired,
        ..Default::default()
    });
    // Returned, not swallowed: this is the door every trust decision walks through, and the
    // callers say "Paired" the moment it comes back. A read-only config dir or a sandbox
    // denial used to be announced as success and was gone by the next launch.
    known.save()
}

/// Label a host files this client under. Re-export of `punktfunk_core::client::device_name`.
#[cfg(not(target_family = "wasm"))]
pub fn device_name() -> String {
    punktfunk_core::client::device_name()
}

/// Drop the fp-less placeholder for `addr:port`. `--add-host` with no `--fp` stores one;
/// [`persist_host`] then writes the real pin, so the placeholder would show twice.
/// No-op, and no disk write, when there is none.
pub fn forget_placeholder(addr: &str, port: u16) {
    let mut known = KnownHosts::load();
    let before = known.hosts.len();
    known
        .hosts
        .retain(|h| !(h.fp_hex.is_empty() && h.addr == addr && h.port == port));
    if known.hosts.len() != before {
        let _ = known.save();
    }
}

/// Record an advert should land on: the one its caller matched, by pin, or the placeholder
/// at its address. The address alone would teach the other OS of a dual-boot box this
/// one's OS mark and MAC.
fn learn_target<'a>(
    known: &'a mut KnownHosts,
    fp_hex: &str,
    addr: &str,
    port: u16,
) -> Option<&'a mut KnownHost> {
    let i = known.resolve_index(Some(fp_hex), addr, port)?;
    known.hosts.get_mut(i)
}

/// Copy MAC / OS / mgmt port from an advert onto a saved record and note the address it
/// advertises; `true` if anything moved. Pure (no disk). An omitted field is left alone, and
/// the MAC is learned once — see below.
fn apply_advert(
    h: &mut KnownHost,
    addr: &str,
    mac: &[String],
    os: &str,
    mgmt_port: Option<u16>,
) -> bool {
    let mut changed = false;
    // A candidate for `probe_known`, never a move: two machines sharing an mDNS hostname
    // hand each other's IPs to both adverts. A record with no pin cannot verify one.
    if !h.fp_hex.is_empty()
        && !addr.is_empty()
        && addr != h.addr
        && !h.prev_addrs.iter().any(|a| a == addr)
    {
        h.prev_addrs.insert(0, addr.to_string());
        h.prev_addrs.truncate(PREV_ADDRS_MAX);
        changed = true;
    }
    // An advert may TEACH a wake MAC, never replace one. The fingerprint it matched on is
    // broadcast in clear, so anything on the LAN can claim to be this host; every other
    // field here is corrected by the next real advert, but a MAC is not — a sleeping host
    // sends none, which is exactly when wake is the only way back.
    if !mac.is_empty() && h.mac.is_empty() {
        h.mac = mac.to_vec();
        changed = true;
    }
    if !os.is_empty() && h.os != os {
        h.os = os.to_string();
        changed = true;
    }
    // 0 is how "not advertised" reaches us from a caller whose own type has no `Option`.
    if mgmt_port.is_some_and(|p| p != 0 && h.mgmt_port != Some(p)) {
        h.mgmt_port = mgmt_port;
        changed = true;
    }
    changed
}

/// Persist MAC / OS / mgmt port from a live advert onto the record saved at `addr:port`,
/// and keep `advert_addr` as a place [`probe_known`] asks. No-op, and no disk write, when
/// nothing changed — call it on every discovery tick.
///
/// [`KnownHosts::read`], not [`KnownHosts::load`]: `punktfunk discover` is not an
/// id-minter (see the race on [`KnownHosts::read`]). Takes the fields rather than a
/// `DiscoveredHost` because core and the WinUI shell each have their own type.
pub fn learn_from_advert(
    fp_hex: &str,
    addr: &str,
    port: u16,
    advert_addr: &str,
    mac: &[String],
    os: &str,
    mgmt_port: Option<u16>,
) {
    let mut known = KnownHosts::read();
    let Some(h) = learn_target(&mut known, fp_hex, addr, port) else {
        return;
    };
    if apply_advert(h, advert_addr, mac, os, mgmt_port) {
        let _ = known.save();
    }
}

/// Re-point a saved host at the address it now answers at, matched by fingerprint, keeping
/// the one it leaves in `prev_addrs`. No-op, and no disk write, when unchanged. Only for an
/// address where the pin answered: an mDNS advert's address is not proof of who is there.
pub fn rekey_addr(fp_hex: &str, addr: &str, port: u16) {
    if fp_hex.is_empty() {
        return;
    }
    let mut known = KnownHosts::load();
    let Some(h) = known.hosts.iter_mut().find(|h| h.fp_hex == fp_hex) else {
        return;
    };
    if h.move_to(addr, port) {
        let _ = known.save();
    }
}

/// Stamp now as this host's last successful connect. No-op if the fingerprint is not stored.
pub fn touch_last_used(fp_hex: &str) {
    // An empty fingerprint would stamp the first placeholder in the file, and `last_used`
    // drives the "most recent" accent on the hosts page.
    if fp_hex.is_empty() {
        return;
    }
    let mut known = KnownHosts::load();
    if let Some(h) = known.hosts.iter_mut().find(|h| h.fp_hex == fp_hex) {
        h.last_used = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .ok();
        let _ = known.save();
    }
}

/// Persist mgmt port from the session `Welcome`, keyed by fingerprint.
///
/// mDNS-free: [`learn_from_advert`] needs a visible advert; this fires on any successful
/// connect, including a host added by IP. No-op, and no disk write, when unchanged.
pub fn learn_mgmt_port_by_fp(fp_hex: &str, mgmt_port: u16) {
    if fp_hex.is_empty() || mgmt_port == 0 {
        return;
    }
    let mut known = KnownHosts::load();
    let Some(h) = known.hosts.iter_mut().find(|h| h.fp_hex == fp_hex) else {
        return;
    };
    if h.mgmt_port == Some(mgmt_port) {
        return;
    }
    h.mgmt_port = Some(mgmt_port);
    let _ = known.save();
}

/// SPAKE2 PIN ceremony. `device_name` is the label the host stores; 90 s covers a
/// human-typed PIN. Returns the verified host certificate fingerprint.
#[cfg(not(target_family = "wasm"))]
pub fn pair_with_host(
    addr: &str,
    port: u16,
    identity: &(String, String),
    pin: &str,
    device_name: &str,
) -> std::result::Result<[u8; 32], punktfunk_core::PunktfunkError> {
    NativeClient::pair(
        addr,
        port,
        (&identity.0, &identity.1),
        pin.trim(),
        device_name,
        std::time::Duration::from_secs(90),
    )
}

/// The sentence to show for a typed rejection: the host's own words when it sent
/// any, else [`connect_reject_message`] for the code.
///
/// A host knows things no client can — which monitor it was told to capture, and
/// that it no longer has one by that name. Only a host that had nothing specific
/// to say leaves the text empty, and then the generic line is the better one.
pub fn reject_message(reason: punktfunk_core::reject::RejectReason, said: Option<&str>) -> String {
    match said {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => connect_reject_message(reason),
    }
}

/// User-facing sentence for a typed host rejection, shared by every desktop/console
/// surface so "declined" never renders as "timed out". The caller words other errors.
pub fn connect_reject_message(reason: punktfunk_core::reject::RejectReason) -> String {
    use punktfunk_core::reject::RejectReason as R;
    match reason {
        R::Denied => "The host declined this device's request.".into(),
        R::ApprovalTimeout => {
            "Nobody approved the request on the host in time — approve this device in the \
             host's console or web UI, then request access again."
                .into()
        }
        R::Superseded => {
            "A newer request from this device replaced this one — approve the latest request \
             on the host."
                .into()
        }
        R::IdentityRequired => {
            "The host requires pairing — pair this device (PIN or request access) first.".into()
        }
        R::PairingNotArmed => {
            "Pairing isn't armed on the host — arm it on the host's Pairing page, then try \
             again."
                .into()
        }
        R::PairingBoundToOtherDevice => {
            "The host's pairing window is armed for a different device — arm it for this one."
                .into()
        }
        R::PairingRateLimited => {
            "Too many pairing attempts — wait a couple of seconds and try again.".into()
        }
        R::WireVersionMismatch => {
            "Client and host versions don't match — update both to the same release.".into()
        }
        R::Busy => "The host is busy with another session.".into(),
        R::SetupFailed => {
            "The host accepted the connection but couldn't start the stream — the host's log \
             (web console → Log) has the cause."
                .into()
        }
        R::AccessExpired => {
            "Your access to this host has expired — ask the host's owner to grant it again.".into()
        }
        R::LaunchNotPermitted => {
            "This device isn't permitted to launch games on the host — connect without picking \
             a game, or ask the host's owner to allow launching."
                .into()
        }
        R::HostPower => {
            "The host is going to sleep or shutting down — wake it when you want to play again."
                .into()
        }
    }
}

/// User-facing sentence for a failed [`pair_with_host`]. Crypto is a wrong PIN; do not
/// report a dead path or a disarmed host as one.
pub fn pair_error_message(err: &punktfunk_core::PunktfunkError) -> String {
    use punktfunk_core::PunktfunkError as E;
    match err {
        E::Crypto => "Wrong PIN — check the PIN on the host's Pairing page and try again.".into(),
        E::Rejected(reason) => connect_reject_message(*reason),
        E::Timeout => "The host didn't answer. Is it running and reachable?".into(),
        E::Io(_) => {
            "Couldn't reach the host — check that this device and the host are on the same \
             network (no VPN on this device, no guest-Wi-Fi / AP isolation)."
                .into()
        }
        other => {
            tracing::warn!(error = %other, "pairing failed");
            "Pairing didn't finish — try again from the host's Pairing page.".to_string()
        }
    }
}

/// Probe several hosts in parallel — wall-clock is ~one `timeout`, not the sum. Result
/// index matches `targets`, each `(addr, port, expected_fp_hex)`.
///
/// A target carrying a fingerprint is online only when THAT host answers. An address is
/// not an identity: a stranger who inherits a sleeping host's lease answers at it, and
/// counting that as the host lights the pip and shuts the wake gate (`!online`) against
/// the machine that needs waking. An empty fingerprint is a record saved by address alone,
/// which has nothing to compare — any answer is the host it names.
#[cfg(not(target_family = "wasm"))]
pub fn probe_reachable_many(
    targets: Vec<(String, u16, String)>,
    timeout: std::time::Duration,
) -> Vec<bool> {
    let handles: Vec<_> = targets
        .into_iter()
        .map(|(addr, port, want)| {
            std::thread::spawn(move || {
                answered_as_self(&want, NativeClient::probe_identity(&addr, port, timeout))
            })
        })
        .collect();
    handles
        .into_iter()
        .map(|h| h.join().unwrap_or(false))
        .collect()
}

/// Whether a probe's answer is the host that was asked for. `answered` is the fingerprint
/// that replied, or `None` when nothing did; `want` is the record's pin, empty for one saved
/// by address alone — that has nothing to compare, so any answer is the host it names.
#[cfg(not(target_family = "wasm"))]
fn answered_as_self(want: &str, answered: Option<[u8; 32]>) -> bool {
    match answered {
        None => false,
        Some(fp) => want.is_empty() || hex(&fp).eq_ignore_ascii_case(want),
    }
}

/// Probe every saved host where it could be, in parallel: its address, then — only when
/// that is silent — the addresses it left (`prev_addrs`), all at once. A pinned host that
/// answers at one of those moves back there, so Tailscale takes over again once the LAN is
/// gone. A host saved by address alone is asked there only. Result index matches `hosts`:
/// `true` = online.
#[cfg(not(target_family = "wasm"))]
pub fn probe_known(hosts: &[KnownHost], timeout: std::time::Duration) -> Vec<bool> {
    let found: Vec<Option<String>> = std::thread::scope(|s| {
        let handles: Vec<_> = hosts
            .iter()
            .map(|h| s.spawn(move || where_answers(h, timeout)))
            .collect();
        handles
            .into_iter()
            .map(|j| j.join().ok().flatten())
            .collect()
    });
    for (h, at) in hosts.iter().zip(&found) {
        if let Some(a) = at.as_deref().filter(|a| *a != h.addr) {
            rekey_addr(&h.fp_hex, a, h.port);
        }
    }
    found.iter().map(Option::is_some).collect()
}

/// Where `h` answers as itself: its address, else the first of the addresses it left.
#[cfg(not(target_family = "wasm"))]
fn where_answers(h: &KnownHost, timeout: std::time::Duration) -> Option<String> {
    let asks = |addr: &str| {
        answered_as_self(
            &h.fp_hex,
            NativeClient::probe_identity(addr, h.port, timeout),
        )
    };
    if asks(&h.addr) {
        return Some(h.addr.clone());
    }
    if h.fp_hex.is_empty() {
        return None;
    }
    let asks = &asks;
    let hits: Vec<bool> = std::thread::scope(|s| {
        let handles: Vec<_> = h
            .prev_addrs
            .iter()
            .map(|a| s.spawn(move || asks(a.as_str())))
            .collect();
        handles
            .into_iter()
            .map(|j| j.join().unwrap_or(false))
            .collect()
    });
    h.prev_addrs
        .iter()
        .zip(hits)
        .find_map(|(a, hit)| hit.then(|| a.clone()))
}

/// Overlay tier. Lives in the core so every client formats with the same enum.
pub use punktfunk_core::hud::StatsVerbosity;

/// How a touchscreen drives the host (Android `TouchMode`, Apple `TouchInputMode`).
/// Stored stringly in [`Settings::touch_mode`]; parsed with [`TouchMode::from_name`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TouchMode {
    /// Relative cursor (touchpad): stays put on down, moves by delta, tap to click.
    /// Default — a cursor works on a screen the host is not sized for.
    Trackpad,
    /// Direct pointing: the cursor jumps to the finger and follows it (absolute).
    Pointer,
    /// Multi-touch passthrough: each finger is a host contact, no gesture interpretation.
    Touch,
    /// Fingers reach the host as nothing. Client gestures (ring twist, stats tap) still
    /// run, so a miss beside the on-screen pad cannot move the cursor.
    Off,
}

impl TouchMode {
    pub const ALL: [TouchMode; 4] = [
        TouchMode::Trackpad,
        TouchMode::Pointer,
        TouchMode::Touch,
        TouchMode::Off,
    ];

    /// Persisted name; unknown / unset → `Trackpad`.
    pub fn from_name(s: &str) -> TouchMode {
        match s {
            "pointer" => TouchMode::Pointer,
            "touch" => TouchMode::Touch,
            "off" => TouchMode::Off,
            _ => TouchMode::Trackpad,
        }
    }

    pub fn as_name(self) -> &'static str {
        match self {
            TouchMode::Trackpad => "trackpad",
            TouchMode::Pointer => "pointer",
            TouchMode::Touch => "touch",
            TouchMode::Off => "off",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TouchMode::Trackpad => "Trackpad",
            TouchMode::Pointer => "Direct pointer",
            TouchMode::Touch => "Touch passthrough",
            TouchMode::Off => "Off",
        }
    }
}

/// How a physical mouse drives the host (design/remote-desktop-sweep.md). Stored
/// stringly in [`Settings::mouse_mode`]; parsed with [`MouseMode::from_name`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseMode {
    /// Pointer lock (relative deltas, hidden cursor). Default: the only cursor is the host's.
    Capture,
    /// Uncaptured absolute pointer through the letterbox. Needs an injector with
    /// absolute support (not gamescope).
    Desktop,
}

impl MouseMode {
    pub const ALL: [MouseMode; 2] = [MouseMode::Capture, MouseMode::Desktop];

    /// Persisted name; unknown / unset → `Capture`.
    pub fn from_name(s: &str) -> MouseMode {
        match s {
            "desktop" => MouseMode::Desktop,
            _ => MouseMode::Capture,
        }
    }

    pub fn as_name(self) -> &'static str {
        match self {
            MouseMode::Capture => "capture",
            MouseMode::Desktop => "desktop",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MouseMode::Capture => "Capture (games)",
            MouseMode::Desktop => "Desktop (absolute)",
        }
    }
}

/// Presentation intent (design/desktop-presentation-rebuild.md). Stored as
/// [`Settings::present_priority`] + [`Settings::smooth_buffer`]; resolved with
/// [`PresentPriority::resolve`]: anything but `"smooth"` is latency; a buffer
/// outside 1..=3 (including 0 = Automatic) becomes 2.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PresentPriority {
    /// Present the moment the display can take it. Default.
    Latency,
    /// Buffer 1–3 frames of jitter, at that many frames of added display latency.
    Smooth { buffer: u8 },
}

impl PresentPriority {
    /// Shared resolution rule — pure, so every embedder agrees on a foreign preset.
    pub fn resolve(name: &str, buffer: u8) -> PresentPriority {
        if name == "smooth" {
            PresentPriority::Smooth {
                buffer: if (1..=3).contains(&buffer) { buffer } else { 2 },
            }
        } else {
            PresentPriority::Latency
        }
    }

    /// Frames the smoothing store holds; `0` = newest-wins (the latency intent).
    pub fn fifo_capacity(self) -> u8 {
        match self {
            PresentPriority::Latency => 0,
            PresentPriority::Smooth { buffer } => buffer,
        }
    }
}

/// App settings, persisted as JSON. Stringly-typed prefs so the file stays readable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Stream mode; `0` = native size/refresh of the window's monitor, resolved at connect.
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// Requested encoder bitrate (kbps); 0 = host default.
    pub bitrate_kbps: u32,
    /// Host render/encode at `mode × render_scale`; presenter downscales. `> 1`
    /// supersamples; `< 1` under-renders; `1.0` = native. Clamped even, codec max.
    pub render_scale: f64,
    /// How a frame whose aspect differs from the window fills it: `"fit"` (default, bars),
    /// `"crop"` or `"stretch"`. Parsed with `punktfunk_core::video_fit::VideoFit::from_name`;
    /// unknown reads as fit.
    #[serde(default = "default_video_fit")]
    pub video_fit: String,
    pub gamepad: String,
    /// Forward this device's controllers. Default on.
    ///
    /// Off: the client never opens the pad (SDL HIDAPI takes hidraw). Needed when a
    /// USB passthrough or a pad plugged into the host already owns it — otherwise the
    /// host sees two controllers. See [`crate::gamepad::GamepadService::set_forwarding`].
    #[serde(default = "default_true")]
    pub gamepad_forwarding: bool,
    /// `vid:pid:name` (`PadInfo::key`) forwarded as pad 0; empty = most recently connected.
    ///
    /// Per device, and NOT the same value as Apple's `gamepadID`, which is
    /// `vendorName|productCategory`: GameController exposes no vid/pid, and iOS and tvOS have no
    /// IOKit to get one from. The two grammars cannot be exchanged — do not "unify" them by
    /// copying a value across.
    pub forward_pad: String,
    /// Guide / QAM while streaming: `"auto"` (default), `"forward"`, or `"local"`.
    /// Auto forwards everywhere except Gaming Mode, where the local Steam UI also
    /// reacts — forwarding there opens both overlays. Resolved in
    /// [`Settings::system_buttons_forward`].
    #[serde(default = "default_auto")]
    pub system_buttons: String,
    /// Hold-Select ≥ ~350 ms sends the host the guide button (down for the hold).
    /// `"auto"` / `"on"` / `"off"`; auto = on only where raw guide cannot reach the
    /// host cleanly (Gaming Mode). A Select tap is delayed up to the threshold.
    #[serde(default = "default_auto")]
    pub guide_gesture: String,
    /// Host compositor backend to request (advisory; the host falls back if unavailable).
    pub compositor: String,
    /// [`TouchMode`] name: `"trackpad"` (default), `"pointer"`, `"touch"`, or `"off"`.
    /// `default` so older stores load as trackpad.
    #[serde(default = "default_touch_mode")]
    pub touch_mode: String,
    /// [`MouseMode`] name: `"capture"` (default) or `"desktop"`. `default` so older
    /// stores load as capture.
    #[serde(default = "default_mouse_mode")]
    pub mouse_mode: String,
    /// Send system chords (Alt+Tab, Super) to the host while input is captured.
    /// Off leaves them with the local shell. Applies in both mouse models.
    pub inhibit_shortcuts: bool,
    pub mic_enabled: bool,
    /// Platform echo cancellation (PipeWire echo-cancelled source; WASAPI Communications
    /// category). Default on — without it a laptop speaker looping host audio is heard
    /// by the mic. `PUNKTFUNK_NO_AEC=1` overrides off. Only while `mic_enabled`.
    #[serde(default = "default_true")]
    pub echo_cancel: bool,
    /// Requested channels: 2 (stereo), 6 (5.1), 8 (7.1). Host clamps; decoder follows.
    pub audio_channels: u8,
    /// Cross-client `audio_format`: Opus (default), lossless 48, or lossless 96
    /// (`crate::audio_format::AUDIO_FORMATS`).
    ///
    /// Off by default: lossless takes 2.3–4.6 Mbps outside the ABR video budget, vs
    /// ~256 kbps Opus. A request, never a fact — the host may still answer Opus.
    /// Stereo-only: a lossless surround frame does not fit one QUIC datagram
    /// (`design/hi-res-audio.md`). A `String` so an unrecognized value resolves to
    /// Opus rather than ending a session.
    #[serde(default = "default_audio_format")]
    pub audio_format: String,
    /// Ask the host to leave its own audio devices alone (`CLIENT_CAP_KEEP_HOST_AUDIO`).
    /// Off (default): the host parks playback on a silent endpoint. Best-effort; older
    /// hosts ignore it.
    #[serde(default)]
    pub keep_host_audio: bool,
    /// Preferred video codec: `"auto"` (host decides), `"hevc"`, `"h264"`, or `"av1"`.
    /// Soft preference — the host honors it when it can, else falls back.
    #[serde(default = "default_codec")]
    pub codec: String,
    /// Decoder preference: `"auto"` (vendor-ordered native ladder), `"native-vulkan"`,
    /// `"native-vaapi"`, `"native-d3d11va"`, or `"software"`.
    ///
    /// A stored value is not validated. Pre-native spellings `"vulkan"`/`"vaapi"`/`"d3d11va"`
    /// map in `video::migrate_decoder_pref` at warn; the store is not rewritten, so a
    /// downgrade still works. `PUNKTFUNK_DECODER` overrides (see `video::Decoder::new`).
    pub decoder: String,
    /// Decode/present GPU marketing name; empty = automatic. Maps to `PUNKTFUNK_VK_ADAPTER`.
    #[serde(default)]
    pub adapter: String,
    /// Ask for 4:4:4 (`quic::VIDEO_CAP_444`). Default off: bandwidth and encode
    /// headroom; per-preset because a desktop wants it and a game usually does not.
    #[serde(default)]
    pub enable_444: bool,
    /// Advertise 10-bit + HDR10. Off means never send HDR. Default true: Linux stores
    /// never carried this and always advertised.
    #[serde(default = "default_true")]
    pub hdr_enabled: bool,
    /// Advertise 10-bit without HDR (`VIDEO_CAP_10BIT`): SDR desktop at Main10.
    /// Subsumed by `hdr_enabled`. `default` so older stores load off.
    ///
    /// Unlike `hdr_enabled` this asks nothing of the panel, so no client gates it on a display
    /// probe.
    #[serde(default)]
    pub ten_bit_sdr: bool,
    /// `"latency"` (default) or `"smooth"`. Unknown reads as latency so a future
    /// value degrades safely.
    #[serde(default = "default_present_priority")]
    pub present_priority: String,
    /// Smoothness buffer in frames: `0` = Automatic (resolves to 2), else 1–3.
    /// Only under `present_priority = "smooth"`. One frame ≈ one refresh of jitter
    /// and one refresh of display latency.
    #[serde(default)]
    pub smooth_buffer: u8,
    /// Tear-free presentation (default on = MAILBOX, FIFO fallback). Off asks
    /// IMMEDIATE. Shared `vsync` key; macOS defaults false — sync-off means
    /// something different per platform.
    #[serde(default = "default_true")]
    pub vsync: bool,
    /// Let a VRR display follow the stream cadence when fullscreen. Inert on
    /// fixed-refresh (measured from on-glass timestamps). Default on.
    ///
    /// Desktop only, and deliberately absent on Android: that client pins a fixed display mode
    /// and declares its surface FIXED_SOURCE, because OEM refresh governors ignore the advisory
    /// hint. A setting there would have to fight that, not merely gate it.
    #[serde(default = "default_true")]
    pub allow_vrr: bool,
    /// Legacy on/off for the stats overlay — kept in sync with `stats_verbosity`
    /// so pre-tier binaries reading the same file keep working. `alias`: older
    /// WinUI shells persisted this as `show_hud`.
    #[serde(alias = "show_hud")]
    pub show_stats: bool,
    /// Stats overlay tier. `None` = a pre-tier store; resolve through
    /// [`Settings::stats_verbosity`], which falls back to `show_stats`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats_verbosity: Option<StatsVerbosity>,
    /// Overlay vocabulary: off = the Standard figures Moonlight also shows, on = the Advanced
    /// capture→glass view. Device-wide; a preset never carries it.
    #[serde(default)]
    pub advanced_stats: bool,
    /// Enter fullscreen when a stream starts. `--fullscreen` (Gaming Mode) ignores this.
    pub fullscreen_on_stream: bool,
    /// Gamepad-UI backdrop palette (`"violet"` default). Presentation only — never
    /// part of a settings preset. Unknown name → default (a newer client may have
    /// shipped one this binary does not know).
    #[serde(default = "default_ui_palette")]
    pub ui_palette: String,
    /// Follow the desktop theme where the platform exposes one (Omarchy on Linux).
    /// While on, [`ui_palette`](Self::ui_palette) stays stored but does not draw.
    /// Default on: following the desk is the integration; the switch is the way out.
    #[serde(default = "default_true")]
    pub follow_os_theme: bool,
    /// Freeze decorative motion. Presentation only, like [`ui_palette`](Self::ui_palette).
    /// No portable OS "reduce motion" via SDL; also the OLED-friendly mode.
    #[serde(default)]
    pub reduce_motion: bool,
    /// Library order within a group: `""`/unknown = host order, `"title"`,
    /// `"platform"`, or `"store"`. Presentation only; unknown → default shelf.
    #[serde(default)]
    pub library_sort: String,
    /// Library arrangement: `"shelf"` (default, and unknown values) or `"grid"`.
    #[serde(default)]
    pub library_view: String,
    /// The Games tab's sections, in order: ids comma-separated, a leading `-` on a section
    /// switched off (`desktops,recent,-favorites,launchers,collections,games`). `""` = every section, on,
    /// in that order. The Apple app's `librarySections` spelling.
    #[serde(default)]
    pub library_sections: String,
    /// Where a bare launch opens: `"hosts"` (default), `"library"`, or `"stream"`.
    /// `""`/unknown = hosts, the `library_view` convention. Resolve through
    /// [`crate::start::start_screen`] — no default host degrades every value to the list.
    #[serde(default)]
    pub start_in: String,
    /// The host a bare launch opens on, a [`KnownHost::id`]. `None` = derive it: the sole
    /// paired record, else none. A dangling id falls through to that rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_host: Option<String>,
    /// Wake-on-LAN before connecting and wait for boot. Default on. Off for VPN
    /// hosts, where broadcast never reaches and the wait only adds delay.
    #[serde(default = "default_true")]
    pub auto_wake: bool,
    /// Reverse wheel/trackpad scroll sent to the host. Default off = host matches this machine.
    #[serde(default)]
    pub invert_scroll: bool,
    /// In-stream quick-action ring JSON ([`crate::overlay_actions::OverlayConfig::parse`]).
    /// Empty = platform default. One opaque field: each cross-client setting is ~six edits.
    #[serde(default)]
    pub overlay_actions: String,
    /// Playback endpoint (PipeWire `node.name` / WASAPI `IMMDevice` id); empty = OS default.
    /// Maps to `PUNKTFUNK_AUDIO_SINK`. A gone pick falls back to default.
    #[serde(default)]
    pub speaker_device: String,
    /// Capture endpoint; same semantics as `speaker_device` (`PUNKTFUNK_AUDIO_SOURCE`).
    #[serde(default)]
    pub mic_device: String,
    /// DualSense voice-coil haptics (0xD1 kind 0) on a wired pad's audio device.
    /// Gates `CLIENT_CAP_PAD_AUDIO`; wire rumble is suppressed while the stream is
    /// live (see `gamepad.rs`). Default on: no-op without a capable host and a wired DS5.
    #[serde(default = "default_true")]
    pub pad_haptics: bool,
    /// DualSense speaker stream (0xD1 kind 1): `"pad"` (default), `"mix"` (renders
    /// as `"off"` today; see `pad_audio::speaker_active`), or `"off"`.
    #[serde(default = "default_pad_speaker")]
    pub pad_speaker: String,
    /// Stream mode follows the session window (design/midstream-resolution-resize.md).
    /// Overrides `width`/`height` while on; fullscreen degenerates to the display's
    /// native mode. Default off until per-backend validation is green.
    pub match_window: bool,
    /// Last logical window size under `match_window`, so the next launch's first
    /// connect already matches. `0` = never stored → 1280×720.
    pub last_window_w: u32,
    pub last_window_h: u32,
    /// Keys this build does not model, carried through load→save so an older writer
    /// does not drop a newer client's fields. Empty map serializes to nothing.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_codec() -> String {
    "auto".into()
}

/// Opus plane — the one an older client's store must load as. Named from `session`
/// so the default and the menu's first row cannot be two different strings.
fn default_audio_format() -> String {
    crate::audio_format::AUDIO_FORMAT_OPUS.into()
}

fn default_auto() -> String {
    "auto".into()
}

fn default_video_fit() -> String {
    "fit".into()
}

fn default_touch_mode() -> String {
    "trackpad".into()
}

fn default_mouse_mode() -> String {
    "capture".into()
}

fn default_present_priority() -> String {
    "latency".into()
}

fn default_true() -> bool {
    true
}

fn default_ui_palette() -> String {
    "violet".into()
}

fn default_pad_speaker() -> String {
    "pad".into()
}

impl Settings {
    /// Overlay tier, resolving pre-tier stores: `show_stats = false` → Off, else Normal.
    pub fn stats_verbosity(&self) -> StatsVerbosity {
        self.stats_verbosity.unwrap_or(if self.show_stats {
            StatsVerbosity::Normal
        } else {
            StatsVerbosity::Off
        })
    }

    /// Set the tier, keeping the legacy `show_stats` bool coherent for pre-tier readers.
    pub fn set_stats_verbosity(&mut self, v: StatsVerbosity) {
        self.stats_verbosity = Some(v);
        self.show_stats = v != StatsVerbosity::Off;
    }

    pub fn touch_mode(&self) -> TouchMode {
        TouchMode::from_name(&self.touch_mode)
    }

    pub fn mouse_mode(&self) -> MouseMode {
        MouseMode::from_name(&self.mouse_mode)
    }

    pub fn present_priority(&self) -> PresentPriority {
        PresentPriority::resolve(&self.present_priority, self.smooth_buffer)
    }

    /// Whether raw system-button presses (guide + QAM) go to the host.
    /// `game_mode`: auto keeps them local under gamescope (Steam UI reacts too).
    pub fn system_buttons_forward(&self, game_mode: bool) -> bool {
        match self.system_buttons.as_str() {
            "forward" => true,
            "local" => false,
            _ => !game_mode,
        }
    }

    /// Whether the hold-Select guide gesture is armed.
    /// Auto = on only under Gaming Mode, the sole controller route once raw presses stay local.
    pub fn guide_gesture_enabled(&self, game_mode: bool) -> bool {
        match self.guide_gesture.as_str() {
            "on" => true,
            "off" => false,
            _ => game_mode,
        }
    }

    /// The `codec` setting as a `quic::CODEC_*` preference bit (`0` = auto).
    #[cfg(not(target_family = "wasm"))]
    pub fn preferred_codec(&self) -> u8 {
        match self.codec.as_str() {
            "h264" | "avc" => punktfunk_core::quic::CODEC_H264,
            "hevc" | "h265" => punktfunk_core::quic::CODEC_HEVC,
            "av1" => punktfunk_core::quic::CODEC_AV1,
            // Wired-LAN wavelet: preference-only (`resolve_codec` never auto-picks it).
            // Harmless if the bit is not advertised — the ladder falls back to HEVC.
            "pyrowave" => punktfunk_core::quic::CODEC_PYROWAVE,
            _ => 0,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            width: 0,
            height: 0,
            refresh_hz: 0,
            bitrate_kbps: 0,
            render_scale: 1.0,
            video_fit: default_video_fit(),
            gamepad: "auto".into(),
            gamepad_forwarding: true,
            forward_pad: String::new(),
            system_buttons: "auto".into(),
            guide_gesture: "auto".into(),
            compositor: "auto".into(),
            touch_mode: "trackpad".into(),
            mouse_mode: "capture".into(),
            inhibit_shortcuts: true,
            mic_enabled: false,
            echo_cancel: true,
            audio_channels: 2,
            audio_format: default_audio_format(),
            keep_host_audio: false,
            codec: "auto".into(),
            decoder: "auto".into(),
            adapter: String::new(),
            enable_444: false,
            hdr_enabled: true,
            ten_bit_sdr: false,
            present_priority: "latency".into(),
            smooth_buffer: 0,
            vsync: true,
            allow_vrr: true,
            show_stats: true,
            stats_verbosity: None,
            advanced_stats: false,
            fullscreen_on_stream: true,
            ui_palette: default_ui_palette(),
            follow_os_theme: true,
            reduce_motion: false,
            library_sort: String::new(),
            library_view: String::new(),
            library_sections: String::new(),
            start_in: String::new(),
            default_host: None,
            auto_wake: true,
            invert_scroll: false,
            overlay_actions: String::new(),
            speaker_device: String::new(),
            mic_device: String::new(),
            pad_haptics: true,
            pad_speaker: "pad".into(),
            match_window: false,
            last_window_w: 0,
            last_window_h: 0,
            extra: BTreeMap::new(),
        }
    }
}

impl Settings {
    fn path() -> Result<PathBuf> {
        // GTK settings file on Linux, WinUI on Windows. Desktop shells and the session
        // console write it; a plain `--connect` stream only reads.
        #[cfg(windows)]
        return Ok(config_dir()?.join("client-windows-settings.json"));
        #[cfg(not(windows))]
        Ok(config_dir()?.join("client-gtk-settings.json"))
    }

    pub fn load() -> Settings {
        Self::path()
            .map(|p| load_json_or_default(&p))
            .unwrap_or_default()
    }

    /// Fire-and-forget (a failed write must never take a stream down), but temp+rename:
    /// five whole-file writers, and a torn file loads as `Default` — silent reset.
    pub fn save(&self) {
        let Ok(p) = Self::path() else { return };
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = write_atomic(&p, s.as_bytes());
        }
    }
}

/// Settings resolver every front-end and the session go through
/// (design/client-settings-profiles.md):
///
/// ```text
/// effective = overlay(preset).apply(global)
/// preset   = one-off override  ??  title binding  ??  host binding  ??  none
/// ```
///
/// `one_off` is Connect-with / `--preset` / `preset=`; `Some("")` forces globals
/// on a bound host and never rebinds. Unknown one-off → defaults (not a binding).
/// `launch` is the library title id, `None` for the desktop. The host is the record
/// [`KnownHosts::resolve`] names for the pin being dialled, as for the per-host clipboard:
/// a dual-boot box's address also names the other OS, with its own binding.
pub fn effective_settings(
    fp_hex: Option<&str>,
    addr: &str,
    port: u16,
    one_off: Option<&str>,
    launch: Option<&str>,
) -> (Settings, Option<StreamPreset>) {
    let base = Settings::load();
    let catalog = PresetsFile::load();
    let known = KnownHosts::load();
    let host = known.resolve(fp_hex, addr, port);
    let bound = host.and_then(|h| h.preset_id.clone());
    let per_game = launch
        .and_then(|game| host.and_then(|h| h.preset_for_game(game)))
        .map(str::to_string);

    match resolve_preset(&catalog, bound.as_deref(), per_game.as_deref(), one_off) {
        Some(p) => (p.overrides.apply(&base), Some(p)),
        None => (base, None),
    }
}

/// Preset half of [`effective_settings`], split so the precedence rules are testable
/// without touching the config directory: one-off ?? title ?? host ?? none.
///
/// A title binding beats the host's default because it is the more specific answer to
/// the same question — the host default is what a title with no opinion inherits. Public
/// for the clients that keep their own document (webOS) and must launch with this order.
pub fn resolve_preset(
    catalog: &PresetsFile,
    bound: Option<&str>,
    per_game: Option<&str>,
    one_off: Option<&str>,
) -> Option<StreamPreset> {
    match one_off {
        // `--preset ""` forces defaults on a bound host.
        Some("") => None,
        Some(reference) => match catalog.resolve(reference) {
            (Some(p), _) => Some(p.clone()),
            (_, res) => {
                tracing::warn!(
                    preset = %reference,
                    ambiguous = res == Resolution::Ambiguous,
                    "no such settings preset — streaming with the default settings"
                );
                None
            }
        },
        // Bindings are ids, never names — a rename must not hijack one. A dangling id
        // falls through to the next-most-general answer, the way a dangling pin just
        // disappears: the title's deleted preset leaves the host's default standing.
        None => {
            let find = |id: &str| catalog.find_by_id(id).cloned();
            per_game.and_then(find).or_else(|| bound.and_then(find))
        }
    }
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

    /// The key file is owner-only on create. Unix is mode 0600. Windows is an
    /// owner ACE with inherited access removed.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn the_private_key_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("pf-client-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("client-key.pem");
        write_private_key(&key, b"secret").unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), b"secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        // `icacls` prints localized account names; `/save` writes SDDL (UTF-16),
        // which does not follow the display language.
        #[cfg(windows)]
        {
            let saved = dir.join("key-acl.txt");
            let out = std::process::Command::new(icacls_exe())
                .arg(&key)
                .arg("/save")
                .arg(&saved)
                .output()
                .expect("save key acl");
            assert!(out.status.success(), "{out:?}");
            let raw = std::fs::read(&saved).unwrap();
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
                .collect();
            let text = String::from_utf16_lossy(&units);
            let dacl = text.lines().find(|l| l.starts_with("D:")).unwrap_or("");
            assert_eq!(dacl.trim(), "D:PAI(A;;FA;;;OW)", "{text}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 64-hex fingerprint of one repeated digit — readable and distinct per letter.
    fn fp(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }

    #[test]
    fn the_hosts_own_words_outrank_our_generic_line() {
        use punktfunk_core::reject::RejectReason as R;
        let said = "The host is set to capture a monitor called \"HDMI-A-3\", which isn't \
                    connected to it.";
        assert_eq!(reject_message(R::SetupFailed, Some(said)), said);

        // No sentence, or nothing but space: ours is the better line.
        for empty in [None, Some(""), Some("   ")] {
            assert_eq!(
                reject_message(R::SetupFailed, empty),
                connect_reject_message(R::SetupFailed),
                "{empty:?}"
            );
        }
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

    /// A pre-touch-mode store loads as `trackpad`; names round-trip through the enum.
    #[test]
    fn settings_touch_mode_defaults_trackpad() {
        let old = r#"{"width":1280,"height":720,"gamepad":"auto","compositor":"auto"}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.touch_mode, "trackpad");
        assert_eq!(s.touch_mode(), TouchMode::Trackpad);
        // Unknown name falls back to trackpad.
        assert_eq!(TouchMode::from_name("pointer"), TouchMode::Pointer);
        assert_eq!(TouchMode::from_name("touch"), TouchMode::Touch);
        assert_eq!(TouchMode::from_name("off"), TouchMode::Off);
        assert_eq!(TouchMode::from_name("bogus"), TouchMode::Trackpad);
        for m in TouchMode::ALL {
            assert_eq!(TouchMode::from_name(m.as_name()), m);
        }
    }

    /// A pre-presentation store loads latency / Automatic / tear-free / VRR.
    /// Anything but `"smooth"` is latency; a buffer outside 1..=3 becomes 2.
    #[test]
    fn settings_presentation_defaults_and_resolution() {
        let old = r#"{"width":1280,"height":720,"gamepad":"auto","compositor":"auto"}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.present_priority, "latency");
        assert_eq!(s.smooth_buffer, 0);
        assert!(s.vsync);
        assert!(s.allow_vrr);
        assert_eq!(s.present_priority(), PresentPriority::Latency);

        assert_eq!(
            PresentPriority::resolve("smooth", 0),
            PresentPriority::Smooth { buffer: 2 },
            "Automatic resolves to 2"
        );
        assert_eq!(
            PresentPriority::resolve("smooth", 3),
            PresentPriority::Smooth { buffer: 3 }
        );
        assert_eq!(
            PresentPriority::resolve("smooth", 9),
            PresentPriority::Smooth { buffer: 2 },
            "out-of-range pins to the Automatic resolution"
        );
        assert_eq!(
            PresentPriority::resolve("balanced-from-the-future", 2),
            PresentPriority::Latency,
            "unknown intents degrade to latency"
        );
        assert_eq!(PresentPriority::Latency.fifo_capacity(), 0);
        assert_eq!(PresentPriority::Smooth { buffer: 3 }.fifo_capacity(), 3);
    }

    /// A pre-`forward_pad` store loads with the pin on automatic.
    #[test]
    fn settings_forward_pad_defaults_empty() {
        let old = r#"{"width":1280,"height":720,"refresh_hz":60,"bitrate_kbps":0,
            "gamepad":"auto","compositor":"auto","inhibit_shortcuts":true,"mic_enabled":true}"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        assert_eq!(s.forward_pad, "");
        let round: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round.forward_pad, "");
    }

    /// Older WinUI shell files still load: `show_hud` aliases onto `show_stats`,
    /// dropped `engine` is ignored, missing fields default.
    #[test]
    fn settings_reads_winui_shell_shape() {
        let shell = r#"{
            "width": 2560, "height": 1440, "refresh_hz": 120, "bitrate_kbps": 20000,
            "gamepad": "dualsense", "compositor": "auto",
            "inhibit_shortcuts": true, "mic_enabled": true, "audio_channels": 6,
            "hdr_enabled": true, "decoder": "hardware", "codec": "av1",
            "adapter": "NVIDIA GeForce RTX 4080", "show_hud": false, "engine": "builtin"
        }"#;
        let s: Settings = serde_json::from_str(shell).unwrap();
        assert_eq!((s.width, s.height, s.refresh_hz), (2560, 1440, 120));
        assert_eq!(s.bitrate_kbps, 20000);
        assert_eq!(s.audio_channels, 6);
        assert!(s.mic_enabled);
        assert_eq!(s.decoder, "hardware");
        assert_eq!(s.preferred_codec(), punktfunk_core::quic::CODEC_AV1);
        let mut pw = s.clone();
        pw.codec = "pyrowave".into();
        assert_eq!(pw.preferred_codec(), punktfunk_core::quic::CODEC_PYROWAVE);
        assert_eq!(s.adapter, "NVIDIA GeForce RTX 4080");
        assert!(s.hdr_enabled);
        assert!(!s.show_stats);
        assert_eq!(s.forward_pad, "");
        assert!(s.fullscreen_on_stream);
        // Echo cancellation post-dates every stored file: it must load on.
        assert!(s.echo_cancel);
    }

    /// Unknown keys survive load→save. An empty flatten map adds nothing, so files
    /// without extras do not churn.
    #[test]
    fn settings_unknown_keys_survive_round_trip() {
        let newer = r#"{"width":1920,"height":1080,"frob_mode":"fancy","frob_level":3}"#;
        let s: Settings = serde_json::from_str(newer).unwrap();
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(
            s.extra.get("frob_mode").and_then(|v| v.as_str()),
            Some("fancy")
        );
        let out = serde_json::to_string(&s).unwrap();
        assert!(out.contains(r#""frob_mode":"fancy""#), "{out}");
        assert!(out.contains(r#""frob_level":3"#), "{out}");
        // No unknown keys → no artifact of the passthrough field.
        let plain = serde_json::to_string(&Settings::default()).unwrap();
        assert!(!plain.contains("extra"), "{plain}");
        assert!(!plain.contains("frob"), "{plain}");
    }

    /// A retired key (`library_enabled`) must not fail the load, and must survive the
    /// next whole-file write so a downgrade still reads it.
    #[test]
    fn settings_retired_library_key_loads_and_survives() {
        let stored = r#"{"width":1920,"height":1080,"library_enabled":false}"#;
        let s: Settings = serde_json::from_str(stored).unwrap();
        assert_eq!((s.width, s.height), (1920, 1080));
        assert_eq!(
            s.extra.get("library_enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        let out = serde_json::to_string(&s).unwrap();
        assert!(out.contains(r#""library_enabled":false"#), "{out}");
    }

    /// Pre-tier store falls back to `show_stats`; setting a tier keeps the legacy bool in sync.
    #[test]
    fn stats_verbosity_migrates_and_round_trips() {
        let mut s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.stats_verbosity(), StatsVerbosity::Normal);
        let off: Settings = serde_json::from_str(r#"{"show_stats":false}"#).unwrap();
        assert_eq!(off.stats_verbosity(), StatsVerbosity::Off);

        s.set_stats_verbosity(StatsVerbosity::Compact);
        assert!(s.show_stats);
        s.set_stats_verbosity(StatsVerbosity::Off);
        assert!(!s.show_stats);

        s.set_stats_verbosity(StatsVerbosity::Detailed);
        let round: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(round.stats_verbosity(), StatsVerbosity::Detailed);
        // Lowercase so the file stays readable.
        assert!(serde_json::to_string(&s).unwrap().contains("\"detailed\""));
    }

    /// WinUI known-hosts shape (no `last_used`) loads; same path so the two clients share it.
    #[test]
    fn known_hosts_reads_winui_shell_shape() {
        let shell = r#"{"hosts":[{
            "name": "Gaming PC", "addr": "192.168.1.50", "port": 9777,
            "fp_hex": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "paired": true, "mac": ["aa:bb:cc:dd:ee:ff"]
        }]}"#;
        let k: KnownHosts = serde_json::from_str(shell).unwrap();
        let h = k.find_by_addr("192.168.1.50", 9777).unwrap();
        assert!(h.paired);
        assert_eq!(h.last_used, None);
        assert_eq!(h.mac, vec!["aa:bb:cc:dd:ee:ff".to_string()]);
        assert!(parse_hex32(&h.fp_hex).is_some());
        // Pre-`os` store loads empty and serializes without the key.
        assert_eq!(h.os, "");
        assert!(!serde_json::to_string(&k).unwrap().contains("\"os\""));
    }

    /// Learned OS chain round-trips; an absent key stays absent.
    #[test]
    fn known_hosts_os_chain_round_trips() {
        let k = KnownHosts {
            hosts: vec![KnownHost {
                name: "HTPC".into(),
                addr: "192.168.1.181".into(),
                port: 9777,
                os: "linux/fedora/bazzite".into(),
                ..Default::default()
            }],
        };
        let text = serde_json::to_string(&k).unwrap();
        let back: KnownHosts = serde_json::from_str(&text).unwrap();
        assert_eq!(back.hosts[0].os, "linux/fedora/bazzite");
    }

    /// A pre-presets store loads with no binding/pins and serializes without the new
    /// keys. Id is minted by `load()`, not by deserialization.
    #[test]
    fn known_hosts_migration_is_a_no_op_on_a_pre_presets_store() {
        let old = r#"{"hosts":[{
            "name": "Gaming PC", "addr": "192.168.1.50", "port": 9777,
            "fp_hex": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "paired": true, "clipboard_sync": true
        }]}"#;
        let mut k: KnownHosts = serde_json::from_str(old).unwrap();
        let h = &k.hosts[0];
        assert_eq!(h.preset_id, None);
        assert!(h.pinned_presets.is_empty());
        assert!(h.game_presets.is_empty());
        assert_eq!(h.id, None);
        assert!(h.clipboard_sync);
        let text = serde_json::to_string(&k).unwrap();
        assert!(!text.contains("preset_id"));
        assert!(!text.contains("pinned_presets"));
        assert!(!text.contains("game_presets"));
        assert!(!text.contains("\"id\""));

        // Second pass reports nothing to persist and leaves the minted id alone.
        assert!(k.mint_missing_ids());
        let minted = k.hosts[0].id.clone().unwrap();
        assert_eq!(minted.len(), 36);
        assert!(!k.mint_missing_ids());
        assert_eq!(k.hosts[0].id.as_deref(), Some(minted.as_str()));
        // Empty-string id counts as missing, not as an identity.
        k.hosts[0].id = Some(String::new());
        assert!(k.mint_missing_ids());
        assert_ne!(k.hosts[0].id.as_deref(), Some(""));
    }

    /// A record saved before the rename keeps its bindings, and serializing writes both
    /// spellings, so a client older than the rename still reads them.
    #[test]
    fn a_pre_rename_host_record_keeps_its_bindings() {
        let old = r#"{"hosts":[{
            "name": "Desk", "addr": "192.168.1.50", "port": 9777, "fp_hex": "", "paired": true,
            "profile_id": "aaaaaaaaaaaa", "pinned_profiles": ["bbbbbbbbbbbb"],
            "game_profiles": {"halo": "cccccccccccc"}
        }]}"#;
        let k: KnownHosts = serde_json::from_str(old).unwrap();
        let h = &k.hosts[0];
        assert_eq!(h.preset_id.as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(h.pinned_presets, vec!["bbbbbbbbbbbb".to_string()]);
        assert_eq!(h.preset_for_game("halo"), Some("cccccccccccc"));

        let saved = serde_json::to_value(&k).unwrap();
        let rec = &saved["hosts"][0];
        for (new, old) in LEGACY_HOST_KEYS {
            assert!(!rec[new].is_null(), "{new} is written");
            assert_eq!(rec[new], rec[old], "{old} mirrors {new}");
        }
        let again: KnownHosts = serde_json::from_value(saved.clone()).unwrap();
        assert_eq!(serde_json::to_value(&again).unwrap(), saved);
    }

    /// Both spellings present: the new one wins, since only this client writes it.
    #[test]
    fn a_new_binding_key_wins_over_its_old_spelling() {
        let both = r#"{"hosts":[{"name": "Desk", "addr": "10.0.0.2", "port": 9777,
            "fp_hex": "", "paired": true, "preset_id": "111111111111", "profile_id": "222222222222"}]}"#;
        let k: KnownHosts = serde_json::from_str(both).unwrap();
        assert_eq!(k.hosts[0].preset_id.as_deref(), Some("111111111111"));
    }

    /// `upsert` preserves user-set fields a trust-decision payload does not carry.
    #[test]
    fn upsert_preserves_user_set_host_state() {
        let fp = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let mut k = KnownHosts {
            hosts: vec![KnownHost {
                name: "Desk".into(),
                addr: "192.168.1.50".into(),
                port: 9777,
                fp_hex: fp.into(),
                paired: true,
                last_used: Some(1000),
                mac: vec!["aa:bb:cc:dd:ee:ff".into()],
                os: "linux/fedora/bazzite".into(),
                // Not 47990: the default would make the keep-on-upsert assertions pass vacuously.
                mgmt_port: Some(47991),
                clipboard_sync: true,
                preset_id: Some("aaaaaaaaaaaa".into()),
                pinned_presets: vec!["bbbbbbbbbbbb".into()],
                game_presets: [("halo".to_string(), "cccccccccccc".to_string())].into(),
                id: Some("11111111-2222-4333-8444-555555555555".into()),
                prev_addrs: vec![],
            }],
        };
        // What `persist_host` builds: a trust decision, nothing else.
        k.upsert(KnownHost {
            name: "Desk".into(),
            addr: "192.168.1.51".into(),
            port: 9777,
            fp_hex: fp.into(),
            paired: false,
            ..Default::default()
        });
        let h = &k.hosts[0];
        assert_eq!(k.hosts.len(), 1);
        assert_eq!(h.addr, "192.168.1.51");
        assert!(h.paired);
        assert_eq!(h.last_used, Some(1000));
        assert_eq!(h.mac, vec!["aa:bb:cc:dd:ee:ff".to_string()]);
        assert_eq!(h.os, "linux/fedora/bazzite");
        // Reconnect must not reset mgmt port to None — the library would 404 on 47990.
        assert_eq!(h.mgmt_port, Some(47991));
        assert!(h.clipboard_sync);
        assert_eq!(h.preset_id.as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(h.pinned_presets, vec!["bbbbbbbbbbbb".to_string()]);
        assert_eq!(h.preset_for_game("halo"), Some("cccccccccccc"));
        assert_eq!(
            h.id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555")
        );

        // A carried value does move the binding (UI rebind path).
        k.upsert(KnownHost {
            fp_hex: fp.into(),
            preset_id: Some("cccccccccccc".into()),
            pinned_presets: vec!["dddddddddddd".into()],
            ..Default::default()
        });
        assert_eq!(k.hosts[0].preset_id.as_deref(), Some("cccccccccccc"));
        assert_eq!(k.hosts[0].pinned_presets, vec!["dddddddddddd".to_string()]);

        // Same for the per-game map: only a payload that carries one replaces it.
        k.hosts[0].bind_game_preset("halo", Some("dddddddddddd"));
        k.upsert(KnownHost {
            fp_hex: fp.into(),
            ..Default::default()
        });
        assert_eq!(k.hosts[0].preset_for_game("halo"), Some("dddddddddddd"));
        k.hosts[0].bind_game_preset("halo", None);
        assert!(k.hosts[0].game_presets.is_empty());
    }

    /// A store written before `mgmt_port` loads, resolves to 47990, then takes and
    /// keeps a learned value — the port must outlive the advert.
    #[test]
    fn mgmt_port_survives_a_store_that_predates_it_and_then_persists() {
        // Store written before the field existed: no `mgmt_port` key.
        let old = r#"{"hosts":[{
            "name": "Gaming PC", "addr": "192.168.1.50", "port": 9777,
            "fp_hex": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "paired": true
        }]}"#;
        let mut k: KnownHosts = serde_json::from_str(old).unwrap();
        assert_eq!(k.hosts[0].mgmt_port, None, "absent key decodes to None");
        assert_eq!(
            k.hosts[0].effective_mgmt_port(),
            crate::library::DEFAULT_MGMT_PORT,
            "unknown resolves to the compiled-in default, i.e. today's behaviour"
        );
        // Unset stays out of the serialized form so an untouched store is byte-stable.
        assert!(!serde_json::to_string(&k).unwrap().contains("mgmt_port"));

        // A learned port takes effect and round-trips.
        k.hosts[0].mgmt_port = Some(47991);
        assert_eq!(k.hosts[0].effective_mgmt_port(), 47991);
        let round: KnownHosts = serde_json::from_str(&serde_json::to_string(&k).unwrap()).unwrap();
        assert_eq!(round.hosts[0].mgmt_port, Some(47991));

        // A placeholder's learned port rides onto the pin that retires it, else it
        // drops back to 47990.
        let fresh = fp('a');
        let mut k2 = KnownHosts {
            hosts: vec![KnownHost {
                addr: "192.168.1.50".into(),
                port: 9777,
                mgmt_port: Some(47991),
                ..Default::default()
            }],
        };
        k2.upsert_trusted(KnownHost {
            name: "Gaming PC".into(),
            addr: "192.168.1.50".into(),
            port: 9777,
            fp_hex: fresh.clone(),
            paired: true,
            ..Default::default()
        });
        let kept = k2.hosts.iter().find(|h| h.fp_hex == fresh).unwrap();
        assert_eq!(
            kept.mgmt_port,
            Some(47991),
            "the pin must not lose the port"
        );
    }

    /// An address is not an identity: a stranger on a sleeping host's lease completes the
    /// same handshake. Counting that as the host lights the pip and, since wake reads
    /// `!online`, silences Wake-on-LAN for the machine that needs it.
    #[test]
    fn a_probe_answered_by_someone_else_is_not_this_host() {
        let ours = fp('a');
        let theirs = [0xbbu8; 32];
        assert!(!answered_as_self(&ours, None), "nothing answered");
        assert!(
            !answered_as_self(&ours, Some(theirs)),
            "a stranger on the lease"
        );
        assert!(answered_as_self(&ours, Some([0xaau8; 32])));
        // The store spells a pin lowercase; a hand-typed one need not.
        assert!(answered_as_self(&ours.to_uppercase(), Some([0xaau8; 32])));
        // Saved by address, never paired: there is no pin to compare against.
        assert!(answered_as_self("", Some(theirs)));
        assert!(!answered_as_self("", None));
    }

    /// Two identities at one address are two records. A dual-boot box answers on one
    /// lease with one MAC and a certificate per OS, so pairing the second OS must not
    /// retire the first: it took the user's name, presets and pins with it.
    #[test]
    fn upsert_trusted_keeps_a_second_identity_at_one_address() {
        let (first, second) = (fp('c'), fp('a'));
        let mut k = KnownHosts {
            hosts: vec![KnownHost {
                name: "ENRICOS-DESKTOP (local)".into(),
                addr: "127.0.0.1".into(),
                port: 9777,
                fp_hex: first.clone(),
                paired: true,
                last_used: Some(1000),
                mac: vec!["aa:bb:cc:dd:ee:ff".into()],
                os: "windows".into(),
                mgmt_port: Some(47991),
                clipboard_sync: true,
                preset_id: Some("aaaaaaaaaaaa".into()),
                pinned_presets: vec!["bbbbbbbbbbbb".into()],
                game_presets: [("halo".to_string(), "cccccccccccc".to_string())].into(),
                id: Some("11111111-2222-4333-8444-555555555555".into()),
                prev_addrs: vec![],
            }],
        };
        // The other OS on that box: same address, a certificate the client has never seen.
        k.upsert_trusted(KnownHost {
            name: "127.0.0.1".into(),
            addr: "127.0.0.1".into(),
            port: 9777,
            fp_hex: second.clone(),
            paired: true,
            ..Default::default()
        });
        assert_eq!(k.hosts.len(), 2);
        let kept = k.find_by_fp(&first).expect("the first OS keeps its record");
        assert_eq!(kept.name, "ENRICOS-DESKTOP (local)");
        assert_eq!(kept.mac, vec!["aa:bb:cc:dd:ee:ff".to_string()]);
        assert_eq!(kept.preset_id.as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(kept.pinned_presets, vec!["bbbbbbbbbbbb".to_string()]);
        assert_eq!(kept.preset_for_game("halo"), Some("cccccccccccc"));
        assert!(kept.clipboard_sync);
        assert_eq!(
            kept.id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555"),
            "a deep link to the first OS still resolves"
        );
        assert!(k.find_by_fp(&second).is_some());
        // The address alone can only answer with one of them: the newest decision.
        assert_eq!(k.find_by_addr("127.0.0.1", 9777).unwrap().fp_hex, second);
    }

    /// A host that only moved address keeps its one record, `paired`, clipboard, and id.
    #[test]
    fn upsert_trusted_keeps_a_host_that_only_moved_address() {
        let same = fp('a');
        let mut k = KnownHosts {
            hosts: vec![KnownHost {
                name: "Desk".into(),
                addr: "192.168.1.50".into(),
                port: 9777,
                fp_hex: same.clone(),
                paired: true,
                clipboard_sync: true,
                preset_id: Some("aaaaaaaaaaaa".into()),
                id: Some("11111111-2222-4333-8444-555555555555".into()),
                ..Default::default()
            }],
        };
        k.upsert_trusted(KnownHost {
            name: "Desk".into(),
            addr: "192.168.1.51".into(),
            port: 9777,
            fp_hex: same.clone(),
            paired: false,
            ..Default::default()
        });
        assert_eq!(k.hosts.len(), 1);
        let h = &k.hosts[0];
        assert_eq!(h.addr, "192.168.1.51");
        assert!(h.paired);
        assert!(h.clipboard_sync);
        assert_eq!(h.preset_id.as_deref(), Some("aaaaaaaaaaaa"));
        assert_eq!(
            h.id.as_deref(),
            Some("11111111-2222-4333-8444-555555555555")
        );
    }

    /// The addresses a host left are kept, newest first, without repeats, three at most.
    #[test]
    fn move_to_remembers_the_addresses_a_host_left() {
        let mut h = KnownHost {
            addr: "100.64.0.7".into(),
            fp_hex: fp('a'),
            ..Default::default()
        };
        assert!(h.move_to("192.168.1.9", 9777));
        assert!(!h.move_to("192.168.1.9", 9777));
        assert!(h.move_to("100.64.0.7", 9777));
        assert_eq!(h.prev_addrs, ["192.168.1.9"]);
        for a in ["10.0.0.1", "10.0.0.2", "10.0.0.3"] {
            h.move_to(a, 9777);
        }
        assert_eq!(h.prev_addrs, ["10.0.0.2", "10.0.0.1", "100.64.0.7"]);
        assert_eq!(h.addr, "10.0.0.3");
    }

    /// Superseding is scoped to the decision's `addr:port`. An fp-less save retires nothing.
    #[test]
    fn upsert_trusted_leaves_other_addresses_and_placeholders_alone() {
        let mut k = KnownHosts {
            hosts: vec![
                KnownHost {
                    name: "Other box".into(),
                    addr: "192.168.1.50".into(),
                    port: 9777,
                    fp_hex: fp('c'),
                    paired: true,
                    ..Default::default()
                },
                // Same address, different port: a distinct endpoint, not a duplicate.
                KnownHost {
                    name: "Second host".into(),
                    addr: "192.168.1.51".into(),
                    port: 9778,
                    fp_hex: fp('d'),
                    paired: true,
                    ..Default::default()
                },
            ],
        };
        k.upsert_trusted(KnownHost {
            name: "New box".into(),
            addr: "192.168.1.51".into(),
            port: 9777,
            fp_hex: fp('a'),
            paired: true,
            ..Default::default()
        });
        assert_eq!(k.hosts.len(), 3);
        assert_eq!(
            k.find_by_addr("192.168.1.50", 9777).unwrap().fp_hex,
            fp('c')
        );
        assert_eq!(
            k.find_by_addr("192.168.1.51", 9778).unwrap().fp_hex,
            fp('d')
        );

        // Fp-less save alongside a real record: nothing retired; address still hits the pin.
        k.upsert_trusted(KnownHost {
            name: "Typed by hand".into(),
            addr: "192.168.1.50".into(),
            port: 9777,
            ..Default::default()
        });
        assert_eq!(k.hosts.len(), 4);
        assert_eq!(
            k.find_by_addr("192.168.1.50", 9777).unwrap().fp_hex,
            fp('c')
        );
    }

    /// A duplicated store resolves to the newest trust decision, not the first record.
    /// Load does not delete; retirement waits for the next trust decision.
    #[test]
    fn a_duplicated_store_resolves_to_the_newest_record() {
        let (dead, live) = (fp('c'), fp('a'));
        let mut k = KnownHosts {
            hosts: vec![
                KnownHost {
                    name: "ENRICOS-DESKTOP (local)".into(),
                    addr: "127.0.0.1".into(),
                    port: 9777,
                    fp_hex: dead.clone(),
                    paired: true,
                    last_used: Some(9999),
                    ..Default::default()
                },
                KnownHost {
                    name: "127.0.0.1".into(),
                    addr: "127.0.0.1".into(),
                    port: 9777,
                    fp_hex: live.clone(),
                    paired: true,
                    ..Default::default()
                },
            ],
        };
        assert_eq!(k.find_by_addr("127.0.0.1", 9777).unwrap().fp_hex, live);
        assert!(k.find_by_fp(&dead).is_some());
        // A placeholder appended later never displaces a real pin.
        k.hosts.push(KnownHost {
            addr: "127.0.0.1".into(),
            port: 9777,
            ..Default::default()
        });
        assert_eq!(k.find_by_addr("127.0.0.1", 9777).unwrap().fp_hex, live);
        k.upsert_trusted(KnownHost {
            name: "127.0.0.1".into(),
            addr: "127.0.0.1".into(),
            port: 9777,
            fp_hex: live.clone(),
            paired: true,
            ..Default::default()
        });
        assert_eq!(
            k.hosts.len(),
            2,
            "the placeholder goes, the other pin stays"
        );
        assert!(k.find_by_fp(&dead).is_some());
        assert_eq!(k.find_by_addr("127.0.0.1", 9777).unwrap().fp_hex, live);
    }

    /// An advert lands on the fingerprint match, not a stale namesake earlier in the file.
    /// Forgetting one host that has never been paired takes that host and no other.
    ///
    /// `remove_by_fp` is `retain(|h| h.fp_hex != key)`, so an empty key kept only the hosts
    /// WITH a fingerprint — one Forget on an address-added card wiped every address-added
    /// card in the file. Records saved by address really do carry an empty fingerprint
    /// (`KnownHost { addr, port, ..Default::default() }`), which is what made it reachable.
    #[test]
    fn forgetting_one_unpaired_host_keeps_the_others() {
        let mut k = KnownHosts {
            hosts: vec![
                KnownHost {
                    name: "Desk".into(),
                    addr: "192.168.1.50".into(),
                    port: 9777,
                    ..Default::default()
                },
                KnownHost {
                    name: "Shed".into(),
                    addr: "192.168.1.51".into(),
                    port: 9777,
                    ..Default::default()
                },
                KnownHost {
                    name: "Paired".into(),
                    addr: "192.168.1.52".into(),
                    port: 9777,
                    fp_hex: fp('a'),
                    ..Default::default()
                },
            ],
        };
        assert!(k.hosts.iter().all(|h| h.id.is_some()), "ids are minted");

        // An empty fingerprint is not a key, in either direction.
        assert!(!k.remove_by_fp(""), "an empty fingerprint removes nothing");
        assert_eq!(k.hosts.len(), 3);
        assert!(k.find_by_fp("").is_none(), "and finds nothing");

        // Forget "Shed" by the card's own identity.
        let shed = k.hosts[1].id.clone();
        assert!(k.remove_card(shed.as_deref(), "192.168.1.51", 9777));
        let left: Vec<&str> = k.hosts.iter().map(|h| h.name.as_str()).collect();
        assert_eq!(left, ["Desk", "Paired"], "only the named card went");

        // A record old enough to predate the minted ids still resolves by address.
        k.hosts[0].id = None;
        assert!(k.remove_card(None, "192.168.1.50", 9777));
        assert_eq!(k.hosts.len(), 1);
        // …and a card naming nothing in the file removes nothing.
        assert!(!k.remove_card(None, "192.168.1.99", 9777));
        assert_eq!(k.hosts.len(), 1);
    }

    #[test]
    fn learn_target_prefers_the_fingerprint_match() {
        let (dead, live) = (fp('c'), fp('a'));
        let mut k = KnownHosts {
            hosts: vec![
                KnownHost {
                    addr: "127.0.0.1".into(),
                    port: 9777,
                    fp_hex: dead.clone(),
                    ..Default::default()
                },
                KnownHost {
                    addr: "127.0.0.1".into(),
                    port: 9777,
                    fp_hex: live.clone(),
                    ..Default::default()
                },
            ],
        };
        learn_target(&mut k, &live, "127.0.0.1", 9777).unwrap().os = "windows".into();
        assert_eq!(k.find_by_fp(&live).unwrap().os, "windows");
        assert_eq!(k.find_by_fp(&dead).unwrap().os, "");
        // A pin nobody holds is not its neighbour's; no pin names only a placeholder.
        assert!(learn_target(&mut k, &fp('e'), "127.0.0.1", 9777).is_none());
        assert!(learn_target(&mut k, "", "127.0.0.1", 9777).is_none());
        // Unknown host: write nothing.
        assert!(learn_target(&mut k, &fp('e'), "10.0.0.9", 9777).is_none());
    }

    /// Both OS installs of a dual-boot box at one address: a pin resolves its own record or
    /// the placeholder waiting for it, never the sibling. Only a bare address falls back.
    #[test]
    fn a_pin_resolves_its_own_record_never_the_sibling() {
        let (windows, linux) = (fp('a'), fp('b'));
        let mut k = KnownHosts {
            hosts: vec![KnownHost {
                name: "Desk (Windows)".into(),
                addr: "192.168.1.9".into(),
                port: 9777,
                fp_hex: windows.clone(),
                paired: true,
                ..Default::default()
            }],
        };
        assert!(k.resolve(Some(&linux), "192.168.1.9", 9777).is_none());
        assert!(k.resolve(Some(""), "192.168.1.9", 9777).is_none());
        assert!(k.placeholder_at("192.168.1.9", 9777).is_none());
        assert_eq!(
            k.resolve(None, "192.168.1.9", 9777).unwrap().fp_hex,
            windows
        );
        // A moved lease is still the same pin.
        let moved = k.resolve(Some(&windows), "192.168.1.20", 9777);
        assert_eq!(moved.unwrap().name, "Desk (Windows)");

        k.hosts.push(KnownHost {
            name: "Desk (Linux)".into(),
            addr: "192.168.1.9".into(),
            port: 9777,
            ..Default::default()
        });
        assert_eq!(k.placeholder_at("192.168.1.9", 9777), Some(1));
        let waiting = k.resolve(Some(&linux), "192.168.1.9", 9777);
        assert_eq!(waiting.unwrap().name, "Desk (Linux)");
        // An advert from the second OS teaches its placeholder, not the Windows record.
        learn_target(&mut k, "", "192.168.1.9", 9777).unwrap().os = "linux/arch/cachyos".into();
        assert_eq!(k.hosts[0].os, "");
        assert_eq!(k.hosts[1].os, "linux/arch/cachyos");
    }

    /// An advert writes what it carries, leaves omitted fields, and reports no change
    /// on a repeat — so every discovery tick can call it.
    #[test]
    fn apply_advert_learns_what_it_carries_and_keeps_what_it_omits() {
        let mut h = KnownHost::default();
        let mac = vec!["aa:bb:cc:dd:ee:ff".to_string()];
        assert!(apply_advert(&mut h, "", &mac, "linux/arch", Some(47991)));
        assert_eq!(h.mac, mac);
        assert_eq!(h.os, "linux/arch");
        assert_eq!(h.mgmt_port, Some(47991));
        assert!(!apply_advert(&mut h, "", &mac, "linux/arch", Some(47991)));
        // Absent fields must not overwrite a known MAC — that would cost wake.
        assert!(!apply_advert(&mut h, "", &[], "", None));
        assert_eq!(h.mac, mac);
        assert_eq!(h.os, "linux/arch");
        assert_eq!(h.mgmt_port, Some(47991));
        // 0 is "not advertised" from a caller with no Option — not a port.
        assert!(!apply_advert(&mut h, "", &[], "", Some(0)));
        assert_eq!(h.mgmt_port, Some(47991));
        assert!(apply_advert(&mut h, "", &[], "", Some(47992)));
        assert_eq!(h.mgmt_port, Some(47992));
    }

    /// An advertised address never moves a pinned card: two machines sharing a hostname
    /// hand each other's IPs to both adverts. It waits in `prev_addrs` for `probe_known`.
    #[test]
    fn apply_advert_notes_its_address_without_moving() {
        let mut h = KnownHost {
            addr: "192.168.1.9".into(),
            fp_hex: "ab".repeat(32),
            ..Default::default()
        };
        assert!(apply_advert(&mut h, "192.168.1.20", &[], "", None));
        assert_eq!(h.addr, "192.168.1.9");
        assert_eq!(h.prev_addrs, ["192.168.1.20"]);
        assert!(!apply_advert(&mut h, "192.168.1.20", &[], "", None));
        assert!(!apply_advert(&mut h, "192.168.1.9", &[], "", None));
        // Nothing to verify a candidate against without a pin.
        let mut bare = KnownHost {
            addr: "192.168.1.9".into(),
            ..Default::default()
        };
        assert!(!apply_advert(&mut bare, "192.168.1.20", &[], "", None));
        assert!(bare.prev_addrs.is_empty());
    }

    /// Pins render in card order, deduplicated; dangling ids disappear, never error.
    #[test]
    fn resolved_pins_drop_duplicates_and_dangling_ids() {
        use crate::presets::{PresetsFile, StreamPreset};
        let catalog = PresetsFile {
            version: 1,
            presets: vec![
                StreamPreset {
                    id: "aaaaaaaaaaaa".into(),
                    name: "Work".into(),
                    ..StreamPreset::new("")
                },
                StreamPreset {
                    id: "bbbbbbbbbbbb".into(),
                    name: "Game".into(),
                    ..StreamPreset::new("")
                },
            ],
        };
        let h = KnownHost {
            pinned_presets: vec![
                "bbbbbbbbbbbb".into(),
                "deleted00000".into(),
                "bbbbbbbbbbbb".into(),
                "aaaaaaaaaaaa".into(),
            ],
            ..Default::default()
        };
        let names: Vec<&str> = h
            .resolved_pins(&catalog)
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, vec!["Game", "Work"]);
        assert!(KnownHost::default().resolved_pins(&catalog).is_empty());
    }

    /// One-off beats binding; `""` forces defaults; unknown one-off falls back to
    /// defaults, not the host's preset.
    #[test]
    fn preset_resolution_precedence() {
        use crate::presets::{PresetsFile, StreamPreset};
        let catalog = PresetsFile {
            version: 1,
            presets: vec![
                StreamPreset {
                    id: "aaaaaaaaaaaa".into(),
                    name: "Game".into(),
                    ..StreamPreset::new("")
                },
                StreamPreset {
                    id: "bbbbbbbbbbbb".into(),
                    name: "Work".into(),
                    ..StreamPreset::new("")
                },
                StreamPreset {
                    id: "cccccccccccc".into(),
                    name: "work".into(),
                    ..StreamPreset::new("")
                },
            ],
        };
        let name_of = |p: Option<StreamPreset>| p.map(|p| p.name);

        assert_eq!(resolve_preset(&catalog, None, None, None), None);
        assert_eq!(
            name_of(resolve_preset(&catalog, Some("aaaaaaaaaaaa"), None, None)),
            Some("Game".into())
        );
        assert_eq!(
            name_of(resolve_preset(
                &catalog,
                Some("aaaaaaaaaaaa"),
                None,
                Some("bbbbbbbbbbbb")
            )),
            Some("Work".into())
        );
        assert_eq!(
            name_of(resolve_preset(&catalog, None, None, Some("GAME"))),
            Some("Game".into())
        );
        assert_eq!(
            resolve_preset(&catalog, Some("aaaaaaaaaaaa"), None, Some("")),
            None
        );
        assert_eq!(
            resolve_preset(&catalog, Some("deleted00000"), None, None),
            None
        );
        assert_eq!(
            resolve_preset(&catalog, Some("aaaaaaaaaaaa"), None, Some("nope")),
            None
        );
        assert_eq!(
            resolve_preset(&catalog, Some("aaaaaaaaaaaa"), None, Some("work")),
            None
        );
        // Binding is by id only — a preset named like the bound id must not hijack it.
        assert_eq!(resolve_preset(&catalog, Some("Game"), None, None), None);
    }

    /// A title's binding sits between the one-off and the host's default: more specific
    /// than the host, still overridable for a single launch. A dangling one falls through
    /// to the host rather than to the defaults.
    #[test]
    fn a_title_binding_outranks_the_host_and_yields_to_a_one_off() {
        use crate::presets::{PresetsFile, StreamPreset};
        let catalog = PresetsFile {
            version: 1,
            presets: vec![
                StreamPreset {
                    id: "aaaaaaaaaaaa".into(),
                    name: "Game".into(),
                    ..StreamPreset::new("")
                },
                StreamPreset {
                    id: "bbbbbbbbbbbb".into(),
                    name: "Work".into(),
                    ..StreamPreset::new("")
                },
            ],
        };
        let name_of = |p: Option<StreamPreset>| p.map(|p| p.name);
        let (host, game) = (Some("aaaaaaaaaaaa"), Some("bbbbbbbbbbbb"));

        assert_eq!(
            name_of(resolve_preset(&catalog, host, game, None)),
            Some("Work".into())
        );
        // No binding on the title: the host's default is what it inherits.
        assert_eq!(
            name_of(resolve_preset(&catalog, host, None, None)),
            Some("Game".into())
        );
        // A pinned card's one-off still wins over both.
        assert_eq!(
            name_of(resolve_preset(&catalog, host, game, Some("Game"))),
            Some("Game".into())
        );
        // `--preset ""` forces the globals past a title binding too.
        assert_eq!(resolve_preset(&catalog, host, game, Some("")), None);
        // Deleted title preset: the host's default stands, not the defaults.
        assert_eq!(
            name_of(resolve_preset(&catalog, host, Some("deleted00000"), None)),
            Some("Game".into())
        );
        assert_eq!(
            resolve_preset(&catalog, None, Some("deleted00000"), None),
            None
        );
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

    /// `store_health` is process-global: one test's successful write would clear
    /// another's recorded failure. Nothing else in this crate's tests hits `write_atomic`.
    fn store_health_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
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
    fn the_atomic_route_failing_falls_back_to_an_in_place_write() {
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

        // Success must be readable back — `Ok(())` that lost the bytes is the failure mode.
        write_atomic(&p, b"{\"new\":true}").unwrap();
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
