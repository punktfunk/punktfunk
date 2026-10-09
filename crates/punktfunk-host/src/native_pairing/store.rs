//! Persistent paired-client store: `punktfunk1-paired.json` under the config dir.
//!
//! Owns the paired-clients [`Mutex`] and atomic-replace persistence. GameStream
//! pairing is a separate store. Pending knocks live in [`super::approval`].
//! Persist failures roll back in-memory state so RAM never diverges from disk.
//!
//! Pin this via [`TrustStore`]. Grant masks are stored as written; readers AND
//! with [`GRANT_ALL`]. Expiry is host wall clock, re-evaluated at each check.
//! A seat host opens the box's store read-only and re-reads it when it changes.

use anyhow::Result;
use punktfunk_core::quic::GRANT_ALL;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Paired punktfunk/1 clients. GameStream pairing uses a different store.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct PairedClients {
    pub clients: Vec<PairedClient>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct PairedClient {
    pub name: String,
    /// Hex SHA-256 of the client's certificate.
    pub fingerprint: String,
    /// `GRANT_*` mask. `None` (pre-grants stores) is full control. Readers AND with
    /// [`GRANT_ALL`] so reserved bits from a newer writer cannot take effect here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grants: Option<u32>,
    /// Host wall-clock unix seconds. `None` is permanent. Re-evaluate at each check;
    /// do not cache against a monotonic offset — an NTP step must move the deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<i64>,
    /// Grant time, unix seconds. Display/audit only; never enforced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_unix: Option<i64>,
    /// Drop this record once the device's last session ends and stays ended. Not an
    /// authorization input — while the record is here, `grants` and `expires_unix` govern
    /// exactly as they always do. Older stores read as `false`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub until_disconnect: bool,
    /// Player slot this device's pads take, 0-based, or `None` for the lazy claim.
    /// Not an authorization input: it places controllers, it never widens access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_pad_slot: Option<u8>,
}

/// Operator access choice. `Option<Access>::None` means no choice: new records get
/// full/permanent, existing records keep their grants and expiry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// `GRANT_*` bits. The store masks reserved bits on read; the management API
    /// must 400 them — never silently clear.
    pub grants: u32,
    /// Host wall-clock unix seconds. `None` is permanent.
    pub expires_unix: Option<i64>,
    /// Remove the record when the device disconnects for good, rather than at a clock time.
    /// Independent of `expires_unix`: a grant may have both, and whichever lands first wins.
    pub until_disconnect: bool,
}

impl PairedClients {
    #[cfg(test)]
    fn contains(&self, fp_hex: &str) -> bool {
        self.clients
            .iter()
            .any(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))
    }
}

struct PairedState {
    path: PathBuf,
    clients: PairedClients,
    /// The box's store on a seat host: never written, re-read when its mtime moves.
    read_only: bool,
    stamp: Option<std::time::SystemTime>,
}

fn mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn default_path() -> Result<PathBuf> {
    // `config_dir()` falls back to %APPDATA% when HOME is unset (Windows service).
    Ok(pf_paths::config_dir().join("punktfunk1-paired.json"))
}

/// A store a non-admin planted before the first elevated run is a pre-trusted device.
fn load(path: &Path) -> PairedClients {
    if crate::planted::quarantine_planted_secret(path) {
        return PairedClients::default();
    }
    read_plain(path)
}

fn read_plain(path: &Path) -> PairedClients {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Owner-only so a local user cannot inject a fingerprint.
fn save(state: &PairedState) -> Result<()> {
    if state.read_only {
        anyhow::bail!("the pairing store is the box's; a seat host never writes it");
    }
    let bytes = serde_json::to_vec_pretty(&state.clients)?;
    pf_paths::replace_secret_file(&state.path, &bytes)?;
    Ok(())
}

pub(super) struct TrustStore {
    paired: Mutex<PairedState>,
}

impl TrustStore {
    pub(super) fn open(store_path: Option<PathBuf>) -> Result<TrustStore> {
        let (path, read_only) = match (store_path, pf_paths::seat::trust_dir()) {
            (Some(p), _) => (p, false),
            (None, Some(dir)) => (dir.join("punktfunk1-paired.json"), true),
            (None, None) => (default_path()?, false),
        };
        Ok(TrustStore::at(path, read_only))
    }

    fn at(path: PathBuf, read_only: bool) -> TrustStore {
        // The box host owns its file's planted-secret check; a seat only reads it.
        let clients = if read_only {
            read_plain(&path)
        } else {
            load(&path)
        };
        TrustStore {
            paired: Mutex::new(PairedState {
                stamp: if read_only { mtime(&path) } else { None },
                path,
                clients,
                read_only,
            }),
        }
    }

    pub(super) fn read_only(&self) -> bool {
        self.paired.lock().unwrap().read_only
    }

    /// The state, re-read first on a seat host when the box's file changed, so a grant
    /// edited in the box console applies to the seat's next check.
    fn lock(&self) -> std::sync::MutexGuard<'_, PairedState> {
        let mut p = self.paired.lock().unwrap();
        if p.read_only {
            let now = mtime(&p.path);
            if now != p.stamp {
                p.clients = read_plain(&p.path);
                p.stamp = now;
            }
        }
        p
    }

    /// Present in the store, including expired records. Use [`Self::effective`] for authorization.
    #[cfg(test)]
    pub(super) fn is_paired(&self, fp_hex: &str) -> bool {
        self.lock().clients.contains(fp_hex)
    }

    /// Authorized mask right now: `None` if unpaired or expired. Absent grants are
    /// [`GRANT_ALL`]. AND with [`GRANT_ALL`] on the way out; `normalize_legacy_full`
    /// maps the old full-control value. `now_unix` is the caller's clock so expiry
    /// and the decision it feeds share one instant.
    pub(super) fn effective(&self, fp_hex: &str, now_unix: i64) -> Option<u32> {
        let p = self.lock();
        let c = p
            .clients
            .clients
            .iter()
            .find(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))?;
        if c.expires_unix.is_some_and(|t| now_unix >= t) {
            return None;
        }
        Some(punktfunk_core::quic::normalize_legacy_full(c.grants.unwrap_or(GRANT_ALL)) & GRANT_ALL)
    }

    /// Stored record, verbatim: no expiry check and no grant mask.
    pub(super) fn get(&self, fp_hex: &str) -> Option<PairedClient> {
        self.lock()
            .clients
            .clients
            .iter()
            .find(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))
            .cloned()
    }

    /// Pair with no access choice. A new fingerprint gets full/permanent; an existing
    /// one updates the name only. Widening grants requires [`Self::add_with_access`]
    /// or [`Self::set_access`].
    #[cfg(test)]
    pub(super) fn add(&self, name: &str, fp_hex: &str) -> Result<()> {
        self.add_with_access(name, fp_hex, None)
    }

    /// Pair, optionally with [`Access`]. `Some` replaces grants and expiry; `None`
    /// matches [`Self::add`]. Persist failure rolls back RAM. The caller clears any
    /// pending knock ([`super::approval::ApprovalQueue::admit_and_clear`]).
    pub(super) fn add_with_access(
        &self,
        name: &str,
        fp_hex: &str,
        access: Option<Access>,
    ) -> Result<()> {
        let name = super::sanitize_device_name(name, fp_hex);
        let mut p = self.lock();
        let snapshot = p.clients.clients.clone(); // rollback if save fails
        match p
            .clients
            .clients
            .iter_mut()
            .find(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))
        {
            Some(existing) => {
                existing.name = name;
                if let Some(a) = access {
                    existing.grants = Some(a.grants);
                    existing.expires_unix = a.expires_unix;
                    existing.until_disconnect = a.until_disconnect;
                    existing.granted_unix = Some(crate::clock::unix_secs());
                }
            }
            None => p.clients.clients.push(PairedClient {
                name,
                fingerprint: fp_hex.to_string(),
                grants: access.map(|a| a.grants),
                expires_unix: access.and_then(|a| a.expires_unix),
                granted_unix: access.map(|_| crate::clock::unix_secs()),
                until_disconnect: access.is_some_and(|a| a.until_disconnect),
                preferred_pad_slot: None,
            }),
        }
        if let Err(e) = save(&p) {
            p.clients.clients = snapshot;
            return Err(e);
        }
        Ok(())
    }

    /// Overwrite access on an existing record. Unknown fingerprint returns `false`
    /// and writes nothing — this is not a pairing path. Persist failure rolls back RAM.
    pub(super) fn set_access(&self, fp_hex: &str, access: Access) -> Result<bool> {
        let mut p = self.lock();
        let snapshot = p.clients.clients.clone();
        let Some(existing) = p
            .clients
            .clients
            .iter_mut()
            .find(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))
        else {
            return Ok(false);
        };
        existing.grants = Some(access.grants);
        existing.expires_unix = access.expires_unix;
        existing.until_disconnect = access.until_disconnect;
        existing.granted_unix = Some(crate::clock::unix_secs());
        if let Err(e) = save(&p) {
            p.clients.clients = snapshot;
            return Err(e);
        }
        Ok(true)
    }

    /// Place this device's pads on `slot`, or hand them back to the lazy claim with
    /// `None`. Unknown fingerprint returns `false` and writes nothing — a player pick
    /// is not a pairing path. Persist failure rolls back RAM.
    pub(super) fn set_pad_slot(&self, fp_hex: &str, slot: Option<u8>) -> Result<bool> {
        let mut p = self.lock();
        let snapshot = p.clients.clients.clone();
        let Some(existing) = p
            .clients
            .clients
            .iter_mut()
            .find(|c| c.fingerprint.eq_ignore_ascii_case(fp_hex))
        else {
            return Ok(false);
        };
        existing.preferred_pad_slot = slot;
        if let Err(e) = save(&p) {
            p.clients.clients = snapshot;
            return Err(e);
        }
        Ok(true)
    }

    pub(super) fn list(&self) -> Vec<PairedClient> {
        self.lock().clients.clients.clone()
    }

    /// Drop this fingerprint. Persist failure rolls back RAM so it matches disk.
    pub(super) fn remove(&self, fp_hex: &str) -> Result<bool> {
        let mut p = self.lock();
        let before = p.clients.clients.len();
        let snapshot = p.clients.clients.clone();
        p.clients
            .clients
            .retain(|c| !c.fingerprint.eq_ignore_ascii_case(fp_hex));
        let removed = p.clients.clients.len() != before;
        if removed {
            if let Err(e) = save(&p) {
                p.clients.clients = snapshot;
                return Err(e);
            }
        }
        Ok(removed)
    }

    /// Drop every client in one persist. Returns the removed fingerprints so live
    /// sessions can be torn down. Not a loop over [`Self::remove`]: a mid-loop
    /// failure would leave a half-emptied store. Persist failure rolls back RAM.
    pub(super) fn remove_all(&self) -> Result<Vec<String>> {
        let mut p = self.lock();
        if p.clients.clients.is_empty() {
            return Ok(Vec::new());
        }
        // Empty list is what we persist; the taken vec is rollback and the return value.
        let snapshot = std::mem::take(&mut p.clients.clients);
        if let Err(e) = save(&p) {
            p.clients.clients = snapshot;
            return Err(e);
        }
        Ok(snapshot.into_iter().map(|c| c.fingerprint).collect())
    }

    pub(super) fn count(&self) -> u32 {
        self.lock().clients.clients.len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seat's copy follows the box's file and refuses every write.
    #[test]
    fn a_read_only_store_follows_the_box_and_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("punktfunk1-paired.json");
        let box_store = TrustStore::at(path.clone(), false);
        box_store.add("TV", "aa").unwrap();
        let seat = TrustStore::at(path.clone(), true);
        assert!(seat.is_paired("aa"));
        assert!(seat.add("Phone", "bb").is_err());
        assert!(seat.remove("aa").is_err());
        assert!(
            seat.is_paired("aa"),
            "a refused write leaves memory as it was"
        );

        box_store.remove("aa").unwrap();
        // A later stamp than the seat read, whatever the filesystem's clock granularity.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(later).unwrap();
        assert!(
            !seat.is_paired("aa"),
            "a revoke on the box reaches the seat"
        );
    }

    #[test]
    fn corrupt_store_files_open_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cases: [&[u8]; 3] = [b"", br#"{"clients":["#, b"not json"];

        for (index, contents) in cases.into_iter().enumerate() {
            let path = dir.path().join(format!("{index}.json"));
            std::fs::write(&path, contents).unwrap();

            let store = TrustStore::open(Some(path)).unwrap();

            assert_eq!(store.count(), 0);
            assert!(store.list().is_empty());
        }
    }
}
