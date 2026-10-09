//! On-disk cache of a host's library catalog (titles, not cover art).
//!
//! One JSON file per host under the platform cache dir. Every byte is re-derivable
//! from the host, so the OS may evict it. No size budget: a catalog is a few hundred KB.
//! Shared by every client that fetches through [`crate::library`].
//!
//! The key is the pinned certificate fingerprint, not `addr:port`. A host that
//! moves DHCP lease is the same library; keying on address would miss exactly
//! when a cold-booted box needs the last-seen list.
//!
//! Load is best-effort and silent: a schema mismatch or a missing file is a miss,
//! not an error. The UI renders the snapshot immediately and reconciles when the
//! host answers.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::library::GameEntry;

#[derive(Debug, Deserialize, Serialize)]
pub struct CachedLibrary {
    pub games: Vec<GameEntry>,
    /// Unix seconds. `u64` so a clock that stepped backwards yields a silly age, not a decode failure.
    pub fetched_at: u64,
}

impl CachedLibrary {
    /// `None` if the stamp is in the future. The UI words a staleness note from
    /// this; a year-old list still beats empty.
    pub fn age(&self) -> Option<Duration> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        now.checked_sub(self.fetched_at).map(Duration::from_secs)
    }
}

/// One resolution for every client-side cache: `$XDG_CACHE_HOME/punktfunk/<name>`,
/// or `%LOCALAPPDATA%\punktfunk\cache\<name>`. `None` when neither the cache dir
/// nor a home is named — the caller then runs uncached rather than picking `.`.
pub(crate) fn cache_subdir(name: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let local = std::env::var("LOCALAPPDATA")
            .ok()
            .filter(|s| !s.is_empty())?;
        Some(
            PathBuf::from(local)
                .join("punktfunk")
                .join("cache")
                .join(name),
        )
    }
    #[cfg(not(windows))]
    {
        let base = crate::paths::xdg_home("XDG_CACHE_HOME", ".cache")?;
        Some(base.join("punktfunk").join(name))
    }
}

fn cache_dir() -> Option<PathBuf> {
    cache_subdir("library")
}

/// Re-validated because `fp_hex` becomes a path component (64 lowercase hex).
/// An unpinned host has no key and runs uncached — we do not remember a game
/// list against TOFU.
fn path_for(fp_hex: &str) -> Option<PathBuf> {
    let ok = fp_hex.len() == 64
        && fp_hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ok.then(|| cache_dir().map(|d| d.join(format!("{fp_hex}.json"))))
        .flatten()
}

/// Decode failure (older `GameEntry` shape) is a miss, never an error:
/// one extra fetch, same as no cache.
pub fn load(fp_hex: &str) -> Option<CachedLibrary> {
    let raw = std::fs::read_to_string(path_for(fp_hex)?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Best-effort write. A cache that cannot write is a slower client, not a broken one.
pub fn store(fp_hex: &str, games: &[GameEntry]) {
    // Empty is indistinguishable from a miss; caching it would pin a blank list over titles.
    if games.is_empty() {
        return;
    }
    let Some(path) = path_for(fp_hex) else { return };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return;
    };
    let snapshot = CachedLibrary {
        games: games.to_vec(),
        fetched_at: now.as_secs(),
    };
    let Ok(json) = serde_json::to_vec(&snapshot) else {
        return;
    };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // Rename over write: a kill mid-write must not leave a half-file the next launch reads.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, &json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Part of forgetting the host: the title list must leave disk with it.
pub fn forget(fp_hex: &str) {
    if let Some(path) = path_for(fp_hex) {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_real_fingerprint_becomes_a_path() {
        assert!(path_for("../../etc/passwd").is_none());
        assert!(path_for("").is_none());
        assert!(path_for(&"a".repeat(63)).is_none(), "too short");
        assert!(
            path_for(&"A".repeat(64)).is_none(),
            "uppercase is not our hex"
        );
        assert!(path_for(&"g".repeat(64)).is_none(), "not hex at all");
        if cache_dir().is_some() {
            let p = path_for(&"0123456789abcdef".repeat(4)).expect("64 lowercase hex is a key");
            assert!(p.to_string_lossy().ends_with(".json"));
        }
    }

    #[test]
    fn a_future_stamp_yields_no_age_rather_than_panicking() {
        let ahead = CachedLibrary {
            games: Vec::new(),
            fetched_at: u64::MAX,
        };
        assert!(ahead.age().is_none());
    }
}
