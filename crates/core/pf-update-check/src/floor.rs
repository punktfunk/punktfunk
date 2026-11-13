//! Anti-rollback: the highest manifest serial each channel has accepted.
//!
//! A validly signed older manifest is a replay, never a downgrade. The floor file maps channel
//! to serial; a missing or unparsable one reads as 0, and a read error as 0 for a check but
//! never as a base for a write. The caller supplies the durable write, since host and client
//! persist their state differently.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct FloorFile {
    #[serde(default)]
    serial_floor: BTreeMap<String, u64>,
}

/// Empty when the file is missing or does not parse, so the next raise replaces it. An I/O
/// error is `Err`: the file may hold floors that this read could not see.
fn read(path: &Path) -> io::Result<FloorFile> {
    match std::fs::read(path) {
        Ok(b) => Ok(serde_json::from_slice(&b).unwrap_or_default()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FloorFile::default()),
        Err(e) => Err(e),
    }
}

/// The channel's floor, or 0 when none was ever accepted.
pub fn load(path: &Path, channel: &str) -> u64 {
    let file = read(path).unwrap_or_default();
    file.serial_floor.get(channel).copied().unwrap_or(0)
}

/// Refuse a manifest serial below the channel's floor.
pub fn check(path: &Path, channel: &str, serial: u64) -> Result<(), String> {
    let floor = load(path, channel);
    if serial < floor {
        return Err(format!(
            "manifest serial {serial} is older than the last accepted {floor} — refusing rollback"
        ));
    }
    Ok(())
}

/// Raise the channel's floor to `serial`, never lower it. `write` persists the whole file.
/// A file that cannot be read is left alone: the write would drop the other channels' floors.
pub fn raise(
    path: &Path,
    channel: &str,
    serial: u64,
    write: impl FnOnce(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let mut file = read(path)?;
    let slot = file.serial_floor.entry(channel.to_string()).or_insert(0);
    if serial <= *slot {
        return Ok(());
    }
    *slot = serial;
    write(path, &serde_json::to_vec_pretty(&file)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raise_plain(path: &Path, channel: &str, serial: u64) {
        raise(path, channel, serial, |p, b| std::fs::write(p, b)).unwrap();
    }

    #[test]
    fn floor_never_lowers_and_channels_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-state.json");

        assert_eq!(load(&path, "stable"), 0);
        raise_plain(&path, "stable", 100);
        assert_eq!(load(&path, "stable"), 100);
        raise_plain(&path, "stable", 50);
        assert_eq!(load(&path, "stable"), 100, "a replay must not lower it");
        raise_plain(&path, "canary", 7);
        assert_eq!(load(&path, "canary"), 7);
        assert_eq!(load(&path, "stable"), 100);
    }

    #[test]
    fn corrupt_floor_file_reads_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-state.json");
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load(&path, "stable"), 0);
        raise_plain(&path, "stable", 5);
        assert_eq!(load(&path, "stable"), 5);
    }

    #[test]
    fn a_read_error_does_not_lower_the_other_floors() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the file belongs: reading it fails, and is not "missing".
        let path = dir.path().join("update-state.json");
        std::fs::create_dir(&path).unwrap();
        let err = raise(&path, "canary", 7, |_, _| {
            panic!("no write over an unread file")
        });
        assert!(err.is_err());
    }

    #[test]
    fn an_older_serial_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-state.json");
        raise_plain(&path, "stable", 100);
        assert_eq!(check(&path, "stable", 100), Ok(()));
        assert_eq!(
            check(&path, "stable", 99).unwrap_err(),
            "manifest serial 99 is older than the last accepted 100 — refusing rollback"
        );
        assert_eq!(check(&path, "canary", 1), Ok(()));
    }

    #[test]
    fn an_unraised_floor_does_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-state.json");
        raise_plain(&path, "stable", 100);
        raise(&path, "stable", 100, |_, _| {
            panic!("no write for an equal serial")
        })
        .unwrap();
    }
}
