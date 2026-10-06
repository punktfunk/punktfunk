//! On-disk store for log bundles paired clients upload to this host.
//!
//! Locked-down clients (Steam Deck Gaming Mode, tvOS, webOS) cannot export
//! their own log, so a report would otherwise be host-only. The client POSTs
//! recent log over the management API with its paired mTLS cert; the web
//! console lists the bundle next to the host export.
//!
//! A file store, not `log_capture::ring()`: ingesting a multi-thousand-line
//! bundle into the host's ~4096-entry ring would evict the host half. The host
//! log gets one INFO breadcrumb per received bundle instead.
//!
//! Quota: a paired device may upload at will. Only the newest
//! [`KEEP_PER_DEVICE`] bundles per fingerprint prefix survive; the endpoint
//! caps one bundle at [`MAX_BUNDLE_BYTES`]. Paired devices are enumerable, so
//! the total is bounded too.

use serde::Serialize;
use std::path::PathBuf;
use utoipa::ToSchema;

/// Pruned on that device's next upload; no sweeper.
pub const KEEP_PER_DEVICE: usize = 5;

/// Endpoint size cap. Client rings render to a few hundred KiB; past 1 MiB is not a log.
pub const MAX_BUNDLE_BYTES: usize = 1024 * 1024;

/// `<config-dir>/client-logs/`, beside `captures/`.
pub fn default_dir() -> PathBuf {
    pf_paths::config_dir().join("client-logs")
}

#[derive(Clone, Serialize, ToSchema)]
pub struct ClientLogMeta {
    /// Filename stem; pass to fetch/delete.
    pub id: String,
    /// Paired device name at upload, filesystem-sanitized.
    pub device_name: String,
    /// First 16 hex chars of the pairing fingerprint — enough to match the roster
    /// without repeating the full identity in every filename.
    pub fingerprint_prefix: String,
    /// Upload time (unix ms from the file mtime, not the stem timestamp).
    pub received_ms: u64,
    pub size_bytes: u64,
}

/// Flat directory of `<ts>_<fp16>_<name>.log` files.
pub struct ClientLogStore {
    dir: PathBuf,
}

/// Same charset [`bundle_id`] emits. Excludes `/` and `\`, so `dir.join(id + ".log")`
/// is always a single child of `dir`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Never empty: the id must parse back into three fields, and an all-symbols
/// name would leave a dangling `_`.
fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        .take(24)
        .collect();
    if cleaned.is_empty() {
        "device".into()
    } else {
        cleaned
    }
}

/// `{ISO-date}T{h-m-s}Z_{fp16}_{name}` — timestamp first so a directory sort is
/// newest-last; dashes (not colons) so the stem is a valid Windows filename.
/// Underscores separate the three fields; name/fp stay `[A-Za-z0-9.-]`.
fn bundle_id(unix_ms: u64, fp_hex: &str, name: &str) -> String {
    let stamp = punktfunk_core::time::utc_rfc3339(unix_ms, false).replace(':', "-");
    let fp16: String = fp_hex
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(16)
        .collect();
    format!("{stamp}_{fp16}_{}", sanitize_name(name))
}

impl ClientLogStore {
    /// Owner-only: a stored bundle is another person's log.
    pub fn new(dir: PathBuf) -> std::sync::Arc<Self> {
        if let Err(e) = pf_paths::create_private_dir(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "client-logs dir not created");
        }
        std::sync::Arc::new(ClientLogStore { dir })
    }

    /// Prunes that device past [`KEEP_PER_DEVICE`].
    pub fn save(&self, fp_hex: &str, device_name: &str, body: &[u8]) -> std::io::Result<String> {
        let id = bundle_id(crate::clock::unix_ms(), fp_hex, device_name);
        // Body may contain addresses and host names; owner-only, like host secrets.
        // The dir ACL is not the only gate.
        pf_paths::write_secret_file(&self.dir.join(format!("{id}.log")), body)?;
        let fp16: String = fp_hex
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(16)
            .collect();
        let mut mine: Vec<String> = self
            .stems()
            .into_iter()
            .filter(|stem| stem.split('_').nth(1) == Some(fp16.as_str()))
            .collect();
        mine.sort();
        if mine.len() > KEEP_PER_DEVICE {
            for stale in &mine[..mine.len() - KEEP_PER_DEVICE] {
                let _ = std::fs::remove_file(self.dir.join(format!("{stale}.log")));
            }
        }
        Ok(id)
    }

    pub fn list(&self) -> Vec<ClientLogMeta> {
        let mut out: Vec<ClientLogMeta> = self
            .stems()
            .into_iter()
            .filter_map(|stem| {
                let mut parts = stem.splitn(3, '_');
                let _ts = parts.next()?;
                let fp = parts.next()?.to_string();
                let name = parts.next()?.to_string();
                let md = std::fs::metadata(self.dir.join(format!("{stem}.log"))).ok()?;
                let received_ms = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Some(ClientLogMeta {
                    id: stem,
                    device_name: name,
                    fingerprint_prefix: fp,
                    received_ms,
                    size_bytes: md.len(),
                })
            })
            .collect();
        out.sort_by(|a, b| b.id.cmp(&a.id)); // timestamp-led stem ⇒ lex = chronological
        out
    }

    /// Bundle body for `id`. Unknown AND invalid ids are `NotFound` so a charset
    /// miss is indistinguishable from absence.
    pub fn load(&self, id: &str) -> std::io::Result<Vec<u8>> {
        if !valid_id(id) {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        std::fs::read(self.dir.join(format!("{id}.log")))
    }

    pub fn delete(&self, id: &str) -> std::io::Result<()> {
        if !valid_id(id) {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        std::fs::remove_file(self.dir.join(format!("{id}.log")))
    }

    fn stems(&self) -> Vec<String> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        rd.filter_map(|e| {
            let name = e.ok()?.file_name();
            let name = name.to_str()?;
            let stem = name.strip_suffix(".log")?;
            valid_id(stem).then(|| stem.to_string())
        })
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (std::sync::Arc<ClientLogStore>, PathBuf) {
        // pid+timestamp collided: tests run in parallel in one process and can
        // share a millisecond, then one cleanup deletes the other's live dir.
        // A process-wide counter is unique regardless of timing.
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pf-client-logs-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        (ClientLogStore::new(dir.clone()), dir)
    }

    #[test]
    fn save_list_load_delete_roundtrip() {
        let (s, dir) = store();
        let id = s
            .save("abcdef0123456789ff", "Steam Deck!", b"hello log")
            .unwrap();
        assert!(id.contains("abcdef0123456789"), "{id}");
        assert!(id.ends_with("SteamDeck"), "sanitized name: {id}");
        let listed = s.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].device_name, "SteamDeck");
        assert_eq!(listed[0].size_bytes, 9);
        assert_eq!(s.load(&id).unwrap(), b"hello log");
        s.delete(&id).unwrap();
        assert!(s.list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn traversal_ids_read_as_absent() {
        let (s, dir) = store();
        for bad in ["../cert", "..", ".", "a/b", "a\\b", ""] {
            assert_eq!(
                s.load(bad).unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "{bad:?}"
            );
            assert_eq!(
                s.delete(bad).unwrap_err().kind(),
                std::io::ErrorKind::NotFound,
                "{bad:?}"
            );
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn per_device_quota_prunes_oldest() {
        let (s, dir) = store();
        // Same device, KEEP_PER_DEVICE + 2. Ids share a 1 s timestamp in a fast
        // test, so check survival by count; the other device must stay untouched.
        let other = s.save("ffff00000000000000", "other", b"keep me").unwrap();
        let mut ids = Vec::new();
        for i in 0..(KEEP_PER_DEVICE + 2) {
            // Timestamp field is 1 s; uniqueness goes through the name (embedded in the id).
            ids.push(
                s.save("abcdef0123456789", &format!("dev{i}"), b"x")
                    .unwrap(),
            );
        }
        let listed = s.list();
        let mine = listed
            .iter()
            .filter(|m| m.fingerprint_prefix == "abcdef0123456789")
            .count();
        assert_eq!(mine, KEEP_PER_DEVICE);
        assert!(listed.iter().any(|m| m.id == other), "other device pruned");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn stored_bundles_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let (s, dir) = store();
        let id = s.save("abcdef0123456789", "Deck", b"secret log").unwrap();
        let md = std::fs::metadata(dir.join(format!("{id}.log"))).unwrap();
        assert_eq!(md.permissions().mode() & 0o077, 0, "bundle is owner-only");
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o077,
            0,
            "so is the store dir"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
