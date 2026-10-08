//! Signed per-channel JSON that answers "is a newer build out?"
//!
//! Keys live in the consuming binary and are checked by [`crate::sig`]. TLS and
//! the serving registry are transport, never trust. Host and Linux client share
//! `version`/`ci_run`/`commit`; ignore a payload leg you do not need
//! (`windows_host` today, `windows_host_arm64` for an ARM64 host).
//!
//! Fail closed: signature over the exact bytes, then strict JSON — HTML stubs
//! never parse. `channel` must match the URL we fetched (canary cannot replay
//! onto stable). `serial` is the anti-downgrade floor the consumer persists.
//! `notes_url` must start with [`NOTES_ORIGIN`].

use crate::sig::{verify_signature, PublicKey};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Bump on a breaking change. Unknown values fail closed; this build does not guess.
pub const SCHEMA: u32 = 1;

/// Cap on fetched bytes. The real document is <1 KB; 64 KiB is a DoS floor.
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;

/// `notes_url` must start with this origin or parse fails.
const NOTES_ORIGIN: &str = "https://git.unom.io/";

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Manifest {
    pub schema: u32,
    /// Must equal the channel we fetched; a signed canary replayed onto stable is refused.
    pub channel: String,
    /// Unix seconds, strictly increasing per channel. Freshness uses this, not `published_at`.
    pub serial: u64,
    /// RFC-3339 publish time. Display only.
    #[serde(default)]
    pub published_at: String,
    pub version: String,
    #[serde(default)]
    pub notes_url: String,
    /// Canary "newer" axis. Packaging channels spell the same build differently.
    #[serde(default)]
    pub ci_run: Option<u64>,
    /// Full commit SHA of the build. Each workflow numbers its own run, so a canary package
    /// on this commit carries a different `ci_run` and is still current.
    #[serde(default)]
    pub commit: Option<String>,
    /// Other consumers ignore this leg.
    #[serde(default)]
    pub windows_host: Option<WindowsHostAsset>,
    /// The ARM64 host's installer. Absent until the arm64 leg publishes one; an ARM64 host
    /// never falls back to `windows_host`, which would hand it the x64 exe.
    #[serde(default)]
    pub windows_host_arm64: Option<WindowsHostAsset>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WindowsHostAsset {
    /// Per-version URL, never a `latest/` alias — an alias re-upload would race the hash.
    pub url: String,
    pub sha256: String,
    /// Authenticode leaf SHA-256 pins. In the manifest so a pin rotation is not a host release.
    #[serde(default)]
    pub authenticode_sha256: Vec<String>,
    /// Signing-certificate CN. Empty skips the subject check. Survives Azure leaf rotation.
    #[serde(default)]
    pub authenticode_subject: String,
    /// Minimum Windows build (display/preflight only).
    #[serde(default)]
    pub min_os: String,
}

/// Public constructor: signature over the exact bytes, then parse.
pub fn verify_and_parse(
    bytes: &[u8],
    sig_text: &str,
    keys: &[PublicKey],
    expected_channel: &str,
) -> Result<Manifest> {
    verify_signature(bytes, sig_text, keys).context("update manifest signature")?;
    parse_verified(bytes, expected_channel)
}

/// Validate a document already signature-checked. Tests call this without minting keys.
pub fn parse_verified(bytes: &[u8], expected_channel: &str) -> Result<Manifest> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        bail!("manifest is larger than the {MAX_MANIFEST_BYTES}-byte cap");
    }
    let m: Manifest = serde_json::from_slice(bytes).context("update manifest is not valid JSON")?;
    if m.schema != SCHEMA {
        bail!(
            "unsupported manifest schema {} (this build understands {SCHEMA})",
            m.schema
        );
    }
    if m.channel != expected_channel {
        bail!(
            "manifest is for channel `{}` but this build asked for `{expected_channel}`",
            m.channel
        );
    }
    if m.version.is_empty() || m.version.len() > 64 {
        bail!("manifest version is empty or implausibly long");
    }
    if m.serial == 0 {
        bail!("manifest serial is zero");
    }
    if !m.notes_url.is_empty() && !m.notes_url.starts_with(NOTES_ORIGIN) {
        bail!("manifest notes_url is not on {NOTES_ORIGIN}");
    }
    for (key, asset) in [
        ("windows_host", &m.windows_host),
        ("windows_host_arm64", &m.windows_host_arm64),
    ] {
        if let Some(w) = asset {
            validate_asset(key, w, expected_channel)?;
        }
    }
    Ok(m)
}

/// The per-asset rules; `key` names the leg in the error.
fn validate_asset(key: &str, w: &WindowsHostAsset, expected_channel: &str) -> Result<()> {
    if !w.url.starts_with("https://") {
        bail!("{key}.url must be https");
    }
    if w.sha256.len() != 64 || !w.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("{key}.sha256 is not a hex SHA-256");
    }
    if w.authenticode_sha256
        .iter()
        .any(|pin| pin.len() != 64 || !pin.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        bail!("{key}.authenticode_sha256 contains an invalid pin");
    }
    if w.authenticode_subject.len() > 256 || w.authenticode_subject.chars().any(char::is_control) {
        bail!("{key}.authenticode_subject is invalid");
    }
    if expected_channel == "stable"
        && (w.authenticode_sha256.is_empty() || w.authenticode_subject.is_empty())
    {
        bail!("stable {key} requires an Authenticode leaf pin and subject");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "channel": "stable",
            "serial": 1785400000u64,
            "published_at": "2026-07-30T12:00:00Z",
            "version": "0.23.0",
            "notes_url": "https://git.unom.io/unom/punktfunk/releases/tag/v0.23.0",
            "windows_host": {
                "url": "https://git.unom.io/unom/punktfunk/releases/download/v0.23.0/punktfunk-host-setup-0.23.0.exe",
                "sha256": "aa".repeat(32),
                "authenticode_sha256": ["bb".repeat(32)],
                "authenticode_subject": "unom UG",
                "min_os": "10.0.22621"
            }
        })
    }

    fn bytes(v: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(v).unwrap()
    }

    /// Format contract with the CI signer: raw 64-byte sig as base64; key `ed25519:<base64>`.
    #[test]
    fn signed_roundtrip_and_tamper() {
        use base64::Engine as _;
        let (key_str, kp) = crate::sig::tests::keypair();
        let keys = vec![PublicKey::parse(&key_str).unwrap()];

        let body = bytes(&doc());
        let sig = base64::engine::general_purpose::STANDARD.encode(kp.sign(&body));

        let m = verify_and_parse(&body, &sig, &keys, "stable").unwrap();
        assert_eq!(m.version, "0.23.0");
        assert_eq!(
            m.windows_host.as_ref().unwrap().authenticode_sha256.len(),
            1
        );
        assert_eq!(
            m.windows_host.as_ref().unwrap().authenticode_subject,
            "unom UG"
        );

        let mut tampered = body.clone();
        tampered[10] ^= 1;
        assert!(verify_and_parse(&tampered, &sig, &keys, "stable").is_err());

        assert!(verify_and_parse(&body, &sig, &keys, "canary").is_err());
    }

    #[test]
    fn html_error_page_is_not_a_manifest() {
        let html = b"<a href=\"https://objects.example/x\">See Other</a>.";
        assert!(parse_verified(html, "stable").is_err());
    }

    #[test]
    fn truncated_json_refused() {
        let body = bytes(&doc());
        assert!(parse_verified(&body[..body.len() - 5], "stable").is_err());
    }

    #[test]
    fn wrong_schema_refused() {
        let mut v = doc();
        v["schema"] = serde_json::json!(2);
        assert!(parse_verified(&bytes(&v), "stable").is_err());
    }

    #[test]
    fn zero_serial_refused() {
        let mut v = doc();
        v["serial"] = serde_json::json!(0);
        assert!(parse_verified(&bytes(&v), "stable").is_err());
    }

    #[test]
    fn offsite_notes_url_refused() {
        let mut v = doc();
        v["notes_url"] = serde_json::json!("https://evil.example/notes");
        assert!(parse_verified(&bytes(&v), "stable").is_err());
    }

    #[test]
    fn windows_asset_validation() {
        let mut v = doc();
        v["windows_host"]["sha256"] = serde_json::json!("nothex");
        assert!(parse_verified(&bytes(&v), "stable").is_err());
        let mut v = doc();
        v["windows_host"]["url"] = serde_json::json!("http://git.unom.io/x.exe");
        assert!(parse_verified(&bytes(&v), "stable").is_err());
        let mut v = doc();
        v["windows_host"]["authenticode_sha256"] = serde_json::json!(["nothex"]);
        assert!(parse_verified(&bytes(&v), "stable").is_err());
        let mut v = doc();
        v["windows_host"]["authenticode_subject"] = serde_json::json!("bad\nsubject");
        assert!(parse_verified(&bytes(&v), "stable").is_err());
        // Missing windows_host is valid; not every consumer uses that leg.
        let mut v = doc();
        v.as_object_mut().unwrap().remove("windows_host");
        assert!(parse_verified(&bytes(&v), "stable").is_ok());
    }

    /// The arm64 leg is optional and validated by the same rules as the x64 one.
    #[test]
    fn arm64_asset_is_optional_and_validated() {
        let m = parse_verified(&bytes(&doc()), "stable").unwrap();
        assert!(m.windows_host_arm64.is_none());
        let mut v = doc();
        v["windows_host_arm64"] = v["windows_host"].clone();
        let m = parse_verified(&bytes(&v), "stable").unwrap();
        assert_eq!(
            m.windows_host_arm64.unwrap().url,
            m.windows_host.unwrap().url
        );
        v["windows_host_arm64"]["sha256"] = serde_json::json!("nothex");
        let err = parse_verified(&bytes(&v), "stable")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("windows_host_arm64."), "{err}");
    }

    #[test]
    fn stable_windows_asset_requires_both_publisher_bindings() {
        for field in ["authenticode_sha256", "authenticode_subject"] {
            let mut v = doc();
            v["windows_host"].as_object_mut().unwrap().remove(field);
            assert!(parse_verified(&bytes(&v), "stable").is_err());
            v["channel"] = serde_json::json!("canary");
            assert!(parse_verified(&bytes(&v), "canary").is_ok());
        }
    }

    #[test]
    fn oversized_refused() {
        let mut v = doc();
        v["published_at"] = serde_json::json!("x".repeat(MAX_MANIFEST_BYTES));
        assert!(parse_verified(&bytes(&v), "stable").is_err());
    }
}
