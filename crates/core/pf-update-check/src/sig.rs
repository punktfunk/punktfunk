//! Detached Ed25519 over exact document bytes (plugin-store index, update manifest).
//!
//! Key: `ed25519:<base64 of 32 raw bytes>`. Sig: base64 of 64 raw bytes, whitespace-tolerant.
//! Host re-exports this crate so both products share one verifier.

use anyhow::{bail, Context, Result};

/// Pinned key, spelled `ed25519:<base64 of the 32 raw bytes>`.
#[derive(Debug, Clone)]
pub struct PublicKey(Vec<u8>);

impl PublicKey {
    pub fn parse(s: &str) -> Result<Self> {
        use base64::Engine as _;
        let b64 = s
            .strip_prefix("ed25519:")
            .context("public key must be spelled `ed25519:<base64>`")?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .context("public key is not valid base64")?;
        if raw.len() != 32 {
            bail!("ed25519 public key must be 32 bytes, got {}", raw.len());
        }
        Ok(Self(raw))
    }
}

/// Exact bytes against any pinned key. Two slots so rotation is not a flag day.
///
/// `sig_text` is the `.sig` file: base64, whitespace-tolerant.
pub fn verify_signature(bytes: &[u8], sig_text: &str, keys: &[PublicKey]) -> Result<()> {
    use base64::Engine as _;
    if keys.is_empty() {
        bail!("no public key pinned for this source");
    }
    let sig = base64::engine::general_purpose::STANDARD
        .decode(sig_text.trim())
        .context("signature file is not valid base64")?;
    for key in keys {
        let pk =
            aws_lc_rs::signature::UnparsedPublicKey::new(&aws_lc_rs::signature::ED25519, &key.0);
        if pk.verify(bytes, &sig).is_ok() {
            return Ok(());
        }
    }
    bail!("signature does not verify against any pinned key")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn keypair() -> (String, aws_lc_rs::signature::Ed25519KeyPair) {
        use aws_lc_rs::signature::KeyPair as _;
        use base64::Engine as _;
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let key_str = format!(
            "ed25519:{}",
            base64::engine::general_purpose::STANDARD.encode(kp.public_key().as_ref())
        );
        (key_str, kp)
    }

    #[test]
    fn roundtrip_and_tamper() {
        use base64::Engine as _;
        let (key_str, kp) = keypair();
        let keys = vec![PublicKey::parse(&key_str).unwrap()];
        let body = b"the exact bytes";
        let sig = base64::engine::general_purpose::STANDARD.encode(kp.sign(body));

        assert!(verify_signature(body, &sig, &keys).is_ok());
        // A `.sig` file written by a shell redirect ends in a newline — tolerated.
        assert!(verify_signature(body, &format!("{sig}\n"), &keys).is_ok());
        assert!(verify_signature(b"other bytes", &sig, &keys).is_err());
        // No pinned key at all fails closed rather than skipping verification.
        assert!(verify_signature(body, &sig, &[]).is_err());
    }

    #[test]
    fn key_format_is_enforced() {
        assert!(PublicKey::parse("6rmlLg1aQ55cgB6icpC5BEpbMJxwPKdGaDQtDcJ0yLI=").is_err());
        assert!(PublicKey::parse("ed25519:not base64!!").is_err());
        assert!(PublicKey::parse("ed25519:AAAA").is_err());
    }

    /// A valid signature from an unpinned key is still a miss; pinning is the check.
    #[test]
    fn other_signer_refused() {
        use base64::Engine as _;
        let (ours, _) = keypair();
        let (_, theirs) = keypair();
        let keys = vec![PublicKey::parse(&ours).unwrap()];
        let body = b"the exact bytes";
        let sig = base64::engine::general_purpose::STANDARD.encode(theirs.sign(body));
        assert!(verify_signature(body, &sig, &keys).is_err());
    }
}
