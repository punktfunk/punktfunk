//! Shared update-check core for the host and the Linux client.
//!
//! Both products ask whether a newer build exists for this box's channel,
//! from one Ed25519-signed per-channel manifest. Being wrong here is a
//! security bug, so the check lives once:
//!
//! * [`sig`] — detached Ed25519 against pinned keys.
//! * [`floor`] — the per-channel serial that refuses a replayed older manifest.
//! * [`manifest`] — schema and fail-closed validation.
//! * [`feed`] — fetch; verify the post-redirect bytes.
//! * [`version`] — channel and "newer" across packaging formats.
//! * [`detect`] — install-kind ladder, parameterised by [`detect::Product`].
//!
//! No apply code. Apply is privileged and per-product (`punktfunk-host::update`,
//! `pf-client-core::update`, the root helper in `pf-update`).

// Signed network bytes. A bounds bug in the parser would void the signature check.
#![forbid(unsafe_code)]

/// Pinned Ed25519 keys for update manifests. Two slots: sign with the new key,
/// ship builds that trust both, then retire the old. Private half is the
/// `UPDATE_MANIFEST_KEY` CI secret plus the operator's offline backup.
///
/// Lives here, not in either binary: host and client consume the same manifest,
/// so one pin list. `scripts/ci/publish-update-manifest.sh` checks the signing
/// key against this file before it signs.
pub const OFFICIAL_UPDATE_KEYS: [&str; 2] = [
    "ed25519:6rmlLg1aQ55cgB6icpC5BEpbMJxwPKdGaDQtDcJ0yLI=",
    "", // rotation slot
];

/// [`OFFICIAL_UPDATE_KEYS`] parsed, the empty rotation slot skipped.
#[cfg(feature = "verify")]
pub fn pinned_keys() -> Vec<PublicKey> {
    OFFICIAL_UPDATE_KEYS
        .iter()
        .filter(|k| !k.is_empty())
        .filter_map(|k| PublicKey::parse(k).ok())
        .collect()
}

pub mod detect;
#[cfg(feature = "verify")]
pub mod feed;
pub mod floor;
#[cfg(feature = "verify")]
pub mod manifest;
#[cfg(feature = "verify")]
pub mod sig;
pub mod version;

pub use detect::{InstallKind, Product};
#[cfg(feature = "verify")]
pub use feed::FeedError;
#[cfg(feature = "verify")]
pub use manifest::{Manifest, MAX_MANIFEST_BYTES, SCHEMA};
#[cfg(feature = "verify")]
pub use sig::{verify_signature, PublicKey};
pub use version::{canary_run, is_newer, triple, Channel};

#[cfg(all(test, feature = "verify"))]
mod tests {
    #[test]
    fn pinned_keys_skip_the_empty_rotation_slot() {
        assert_eq!(
            super::pinned_keys().len(),
            1,
            "one live key, one empty slot"
        );
    }
}
