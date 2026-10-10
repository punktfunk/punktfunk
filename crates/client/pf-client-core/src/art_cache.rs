//! On-disk cache of library cover art, behind the shells' decoded-texture maps.
//!
//! Keyed by the SHA-256 of the absolute URL, so a host art-proxy path and a store
//! CDN URL share one directory without colliding and a changed URL simply misses.
//! The file name is the hash alone — nothing from the URL becomes a path component,
//! so no candidate can name a file outside the cache.
//!
//! Sits beside [`crate::library_cache`] under the platform cache dir: every byte is
//! re-derivable from the host, so the OS may evict it and so may [`clear`]. Size is
//! bounded by [`MAX_BYTES`], oldest mtime first; [`load`] touches on a hit, which
//! makes that order least-recently-*used*.
//!
//! [`crate::library::spawn_art_fetch`]'s workers own every call here. The UI thread
//! never touches disk for a poster.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::library::ART_MAX_BYTES;

/// 128 MiB. A 1 000-title library of ~100 KB posters lands well under it.
const MAX_BYTES: u64 = 128 * 1024 * 1024;

/// New bytes that earn a directory walk. An eighth of the budget: eight walks to
/// turn the cache over, against one per poster if every store pruned.
const PRUNE_INTERVAL: u64 = MAX_BYTES / 8;

/// Bytes written since the last prune, across all art workers.
static WRITTEN: AtomicU64 = AtomicU64::new(0);

/// Distinguishes the temp files of concurrent workers within one process.
static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

/// `$XDG_CACHE_HOME/punktfunk/art` / `%LOCALAPPDATA%\punktfunk\cache\art`.
fn dir() -> Option<PathBuf> {
    crate::library_cache::cache_subdir("art")
}

fn file_for(dir: &Path, url: &str) -> PathBuf {
    use sha2::Digest;
    let digest: [u8; 32] = sha2::Sha256::digest(url.as_bytes()).into();
    dir.join(crate::trust::hex(&digest))
}

/// Our own entries: 64 lowercase hex. Everything else in the directory — a temp
/// file a killed worker left, a stray — is neither served nor evicted.
fn is_key(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Cached bytes for `url`, or `None` for anything we would not have written —
/// absent, empty, or past the fetch path's ceiling.
pub fn load(url: &str) -> Option<Vec<u8>> {
    load_in(&dir()?, url)
}

fn load_in(dir: &Path, url: &str) -> Option<Vec<u8>> {
    let file = file_for(dir, url);
    let len = std::fs::metadata(&file).ok()?.len();
    if len == 0 || len > ART_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(&file).ok()?;
    touch(&file);
    Some(bytes)
}

/// Stamps last use, so eviction drops what no library has opened in longest.
fn touch(file: &Path) {
    let times = std::fs::FileTimes::new().set_modified(SystemTime::now());
    if let Ok(f) = std::fs::File::options().write(true).open(file) {
        let _ = f.set_times(times);
    }
}

/// Best-effort: a cache that cannot write is a slower client, not a broken one.
pub fn store(url: &str, bytes: &[u8]) {
    if let Some(dir) = dir() {
        store_in(&dir, url, bytes);
    }
}

/// Temp-then-rename in the same directory, so a kill mid-write cannot leave a
/// truncated image that renders as a broken poster on every later launch.
fn store_in(dir: &Path, url: &str, bytes: &[u8]) -> bool {
    let len = bytes.len() as u64;
    if len == 0 || len > ART_MAX_BYTES || std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let tmp = dir.join(format!(
        "{}.{}.tmp",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed)
    ));
    if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, file_for(dir, url)).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    // Amortised: the budget is a steady-state ceiling, not a per-write invariant.
    if WRITTEN.fetch_add(len, Ordering::Relaxed) + len >= PRUNE_INTERVAL {
        WRITTEN.store(0, Ordering::Relaxed);
        prune(dir, MAX_BYTES);
    }
    true
}

/// Drop entries oldest-mtime first until the directory fits `budget`. Taking the
/// budget keeps the test off a 128 MiB write.
fn prune(dir: &Path, budget: u64) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    let mut total = 0u64;
    for entry in read.flatten() {
        if !is_key(&entry.file_name()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        total += meta.len();
        let at = meta.modified().unwrap_or(UNIX_EPOCH);
        entries.push((at, meta.len(), entry.path()));
    }
    if total <= budget {
        return;
    }
    entries.sort_by_key(|(at, _, _)| *at);
    for (_, len, path) in entries {
        if total <= budget {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
}

/// The settings action. A missing directory is already cleared.
pub fn clear() -> std::io::Result<()> {
    let Some(dir) = dir() else {
        return Ok(());
    };
    match std::fs::remove_dir_all(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pf-art-cache-{tag}-{}-{}",
            std::process::id(),
            NEXT_TMP.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_key_is_the_url_hash_and_nothing_else() {
        let dir = Path::new("/cache");
        let evil = file_for(dir, "https://cdn/../../etc/passwd");
        assert_eq!(evil.parent(), Some(dir), "no URL text becomes a component");
        assert!(is_key(evil.file_name().unwrap()));
        assert_ne!(
            file_for(dir, "https://a/1.jpg"),
            file_for(dir, "https://a/2.jpg")
        );
        // The same URL keys the same file in every process — that is the whole cache.
        assert_eq!(
            file_for(dir, "https://a/1.jpg"),
            file_for(dir, "https://a/1.jpg")
        );
    }

    #[test]
    fn a_stored_poster_reads_back() {
        let dir = scratch("roundtrip");
        let url = "https://image.api.playstation.com/poster.png";
        assert!(store_in(&dir, url, b"\x89PNG-ish"));
        assert_eq!(load_in(&dir, url).as_deref(), Some(&b"\x89PNG-ish"[..]));
        assert!(load_in(&dir, "https://other/x.png").is_none());
        // Nothing half-written survives the rename.
        let leftovers = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(leftovers, 1, "one entry, no temp file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_fetch_ceiling_is_refused_on_both_sides() {
        let dir = scratch("ceiling");
        let url = "https://cdn/huge.png";
        assert!(!store_in(&dir, url, &[]), "empty is not art");
        assert!(!store_in(&dir, url, &vec![7u8; ART_MAX_BYTES as usize + 1]));
        assert!(load_in(&dir, url).is_none(), "nothing was written");
        // A file that grew past the ceiling behind our back is a miss, not a huge read.
        std::fs::write(file_for(&dir, url), vec![7u8; ART_MAX_BYTES as usize + 1]).unwrap();
        assert!(load_in(&dir, url).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eviction_takes_the_oldest_mtime_first() {
        let dir = scratch("evict");
        let names = ["oldest", "middle", "newest"];
        for (i, name) in names.iter().enumerate() {
            let path = file_for(&dir, name);
            std::fs::write(&path, b"poster").unwrap();
            let at = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000 + i as u64 * 60);
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(at))
                .unwrap();
        }
        // Budget for two of the three.
        prune(&dir, 12);
        assert!(load_in(&dir, "oldest").is_none(), "oldest goes first");
        assert!(file_for(&dir, "newest").exists(), "newest survives");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
