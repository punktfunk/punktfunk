//! The files behind [`crate::unlock`]: a person's row's device list and the box's pin.
//!
//! `seats/<id>/devices.json` is root's `0600`, in the row's root-owned directory, written whole
//! and renamed into place on each change. A file that can't be read or doesn't hold a list this
//! supervisor would write loads as no device, and the doctor says so: nobody is let on from a
//! list nobody can vouch for.
//!
//! `box-pin` holds the SHA-256 of the box's certificate, `native-cert.pem` in the door's
//! directory, as the supervisor read it when a key proof first needed it. The certificate is
//! public: a door that changes it afterwards gets every key proof refused, and one that planted
//! another before the pin gets proofs a device signed for the real box refused.

use super::safe_open;
use super::shared::seat_dir;
use crate::backend::BackendError;
use crate::ipc::Diagnostic;
use crate::model::SeatId;
use crate::unlock::DeviceList;
use std::path::{Path, PathBuf};

const FILE: &str = "devices.json";
/// A full list with RSA keys is under 100 KiB.
const MAX_BYTES: u64 = 1024 * 1024;
const PIN: &str = "box-pin";
const CERT: &str = "native-cert.pem";

/// A row's list as loaded: the reason it couldn't be read, while the file on disk is that one.
#[derive(Debug, Default)]
pub(super) struct Held {
    pub list: DeviceList,
    pub unreadable: Option<String>,
}

impl Held {
    /// `list` saved, then held.
    pub(super) fn commit(
        &mut self,
        state: &Path,
        id: &SeatId,
        list: DeviceList,
    ) -> Result<(), BackendError> {
        save(state, id, &list)?;
        self.list = list;
        self.unreadable = None;
        Ok(())
    }
}

pub(super) fn path(state: &Path, id: &SeatId) -> PathBuf {
    seat_dir(state, id).join(FILE)
}

/// `id`'s list as of `now`: none when the file is absent, unreadable or not a valid list.
pub(super) fn load(state: &Path, id: &SeatId, now: i64) -> Held {
    let read = safe_open::read(&path(state, id), MAX_BYTES);
    let parsed = match read {
        Ok(None) => return Held::default(),
        Ok(Some((bytes, _))) => serde_json::from_slice::<DeviceList>(&bytes)
            .map_err(|e| e.to_string())
            .and_then(|list| {
                list.valid()
                    .then_some(list)
                    .ok_or_else(|| "an entry is not one this supervisor writes".to_owned())
            }),
        Err(error) => Err(error.to_string()),
    };
    match parsed {
        Ok(mut list) => {
            list.expire(now);
            Held {
                list,
                unreadable: None,
            }
        }
        Err(cause) => {
            tracing::warn!(seat = %id, %cause, "device list unreadable: no device is signed in");
            Held {
                list: DeviceList::default(),
                unreadable: Some(cause),
            }
        }
    }
}

/// Writes `list` as `id`'s file, `0600`, through a temporary file renamed into place. The row's
/// directory must exist: a row deleted meanwhile gets no file back.
fn save(state: &Path, id: &SeatId, list: &DeviceList) -> Result<(), BackendError> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let io =
        |what: &str, e: std::io::Error| BackendError::new("device_list", format!("{what}: {e}"));
    let mut bytes =
        serde_json::to_vec_pretty(list).map_err(|e| io("encode the device list", e.into()))?;
    bytes.push(b'\n');
    let target = path(state, id);
    let temp = target.with_extension("json.tmp");
    let _ = std::fs::remove_file(&temp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|e| io("create the device list", e))?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| io("write the device list", e))?;
    std::fs::rename(&temp, &target).map_err(|e| io("replace the device list", e))
}

/// The SHA-256 of the box's certificate in `box_dir` as it is now.
fn current(box_dir: &Path) -> Result<[u8; 32], BackendError> {
    use rustls::pki_types::{pem::PemObject as _, CertificateDer};
    let unread = |cause: String| BackendError::new("box_identity", format!("read {CERT}: {cause}"));
    let read =
        safe_open::read(&box_dir.join(CERT), 64 * 1024).map_err(|e| unread(e.to_string()))?;
    let (bytes, _) = read.ok_or_else(|| unread("absent".into()))?;
    let der = CertificateDer::from_pem_slice(&bytes).map_err(|e| unread(e.to_string()))?;
    let digest = aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, der.as_ref());
    Ok(digest.as_ref().try_into().expect("SHA-256 is 32 bytes"))
}

/// The pin in `state`, `None` before the first key proof needed it.
fn pinned(state: &Path) -> Result<Option<String>, BackendError> {
    let read = safe_open::read(&state.join(PIN), 256)
        .map_err(|e| BackendError::new("box_pin", format!("read {PIN}: {e}")))?;
    Ok(read.map(|(bytes, _)| String::from_utf8_lossy(&bytes).trim().to_owned()))
}

/// The box's fingerprint for a key proof: pinned in `state` the first time, then held to the pin.
pub(super) fn box_fingerprint(box_dir: &Path, state: &Path) -> Result<[u8; 32], BackendError> {
    let now = current(box_dir)?;
    let shown = hex::encode(now);
    match pinned(state)? {
        Some(pin) if pin == shown => Ok(now),
        Some(pin) => Err(BackendError::new(
            "box_identity_changed",
            format!("the box certificate is {shown}, pinned as {pin}"),
        )),
        None => {
            super::session::write_if_changed(&state.join(PIN), &format!("{shown}\n"), 0o600)
                .map_err(|e| BackendError::new("box_pin", format!("write {PIN}: {e}")))?;
            tracing::info!(fingerprint = %shown, "box certificate pinned for key proofs");
            Ok(now)
        }
    }
}

/// The doctor's row for the pin, once one is taken: an error while the certificate differs.
pub(super) fn pin_diagnostic(box_dir: &Path, state: &Path) -> Option<Diagnostic> {
    let pin = match pinned(state) {
        Ok(pin) => pin?,
        Err(error) => return Some(Diagnostic::error("box_pin", error.to_string())),
    };
    Some(match current(box_dir) {
        Ok(now) if hex::encode(now) == pin => {
            Diagnostic::info("box_pin", "The box certificate matches the pin key proofs use.")
                .worded("seatcheck_box_pin_ok")
        }
        _ => Diagnostic::error(
            "box_pin",
            format!(
                "The box certificate no longer matches {}/{PIN}, so every face or fingerprint unlock is refused. Once the change is known to be the box's own, remove that file.",
                state.display()
            ),
        )
        .worded("seatcheck_box_pin_changed")
        .param("path", format!("{}/{PIN}", state.display())),
    })
}

/// The doctor's row for a person's row whose list couldn't be read.
pub(super) fn unreadable_diagnostic(id: &SeatId, cause: &str) -> Diagnostic {
    Diagnostic {
        seat_id: Some(id.clone()),
        ..Diagnostic::error(
            "device_list",
            format!(
                "The profile's device list couldn't be read, so no device is signed in: {cause}"
            ),
        )
        .worded("seatcheck_device_list")
        .param("error", cause)
    }
}
