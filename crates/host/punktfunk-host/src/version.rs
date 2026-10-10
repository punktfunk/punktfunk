//! The version this binary reports: `--version`, logs, the management API, update checks.
//!
//! The build fills a fixed slot with build.rs's `PUNKTFUNK_VERSION`. Packaging may overwrite
//! the slot in the finished binary (`packaging/windows/stamp-version.ps1`), so a per-run
//! version never reaches the compiler and an unchanged tree stays a cargo no-op.

use std::sync::OnceLock;

/// The stamper finds the slot by this marker. The payload after it is NUL-padded UTF-8.
const MARKER: [u8; 16] = *b"pf-version-slot\0";
const CAP: usize = 64;
const LEN: usize = MARKER.len() + CAP;

#[used]
static SLOT: [u8; LEN] = slot(env!("PUNKTFUNK_VERSION"));

const fn slot(version: &str) -> [u8; LEN] {
    let v = version.as_bytes();
    assert!(
        v.len() < CAP,
        "PUNKTFUNK_BUILD_VERSION does not fit the version slot"
    );
    let mut out = [0u8; LEN];
    let mut i = 0;
    while i < MARKER.len() {
        out[i] = MARKER[i];
        i += 1;
    }
    let mut j = 0;
    while j < v.len() {
        out[MARKER.len() + j] = v[j];
        j += 1;
    }
    out
}

pub(crate) fn get() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        // SAFETY: `SLOT` is a live static of plain bytes, so the pointer is valid and aligned.
        // Volatile keeps the optimizer from folding in the payload packaging may have replaced.
        let raw = unsafe { std::ptr::read_volatile(&SLOT) };
        let payload = &raw[MARKER.len()..];
        let end = payload.iter().position(|&b| b == 0).unwrap_or(CAP);
        String::from_utf8_lossy(&payload[..end]).into_owned()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn unstamped_slot_reads_the_build_version() {
        assert_eq!(super::get(), env!("PUNKTFUNK_VERSION"));
    }
}
