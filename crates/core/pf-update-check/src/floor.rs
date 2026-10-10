//! Anti-rollback: the highest manifest serial each channel has accepted.
//!
//! A validly signed older manifest is a replay, never a downgrade. The floor file maps channel
//! to serial; an unreadable one reads as 0. The caller supplies the durable write, since host
//! and client persist their state differently.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct FloorFile {
    #[serde(default)]
    serial_floor: BTreeMap<String, u64>,
}

fn read(path: &Path) -> FloorFile {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The channel's floor, or 0 when none was ever accepted.
pub fn load(path: &Path, channel: &str) -> u64 {
    read(path).serial_floor.get(channel).copied().unwrap_or(0)
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
pub fn raise(
    path: &Path,
    channel: &str,
    serial: u64,
    write: impl FnOnce(&Path, &[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let mut file = read(path);
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
