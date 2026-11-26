//! Detached signatures over exact document bytes (plugin-store index, update manifest).
//!
//! Key: `ed25519:<base64 of 32 raw bytes>` or `mldsa65:<base64 of 1952 raw bytes>`. Each
//! algorithm has a signature file of its own, base64 with whitespace allowed around it:
//! `<url>.sig` holds the 64 bytes of an Ed25519 signature, `<url>.mldsa65.sig` the 3309 bytes
//! of an ML-DSA-65 signature.
//!
//! A document needs a signature from every algorithm the pinned keys name. With both
//! algorithms pinned, forging a document takes breaking both. ML-DSA signs the bytes as they
//! are, with an empty context string. Host re-exports this crate so both products share one
//! verifier.

use anyhow::{bail, Context, Result};
use aws_lc_rs::signature::{UnparsedPublicKey, ED25519, ML_DSA_65};

/// A signature algorithm a key pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alg {
    Ed25519,
    MlDsa65,
}

impl Alg {
    const ALL: [Alg; 2] = [Alg::Ed25519, Alg::MlDsa65];

    /// What a key's spelling starts with, before the colon.
    fn name(self) -> &'static str {
        match self {
            Alg::Ed25519 => "ed25519",
            Alg::MlDsa65 => "mldsa65",
        }
    }

    fn key_len(self) -> usize {
        match self {
            Alg::Ed25519 => 32,
            Alg::MlDsa65 => 1952,
        }
    }

    /// Appended to a document's URL, it names the signature file of this algorithm.
    pub fn sig_suffix(self) -> &'static str {
        match self {
            Alg::Ed25519 => ".sig",
            Alg::MlDsa65 => ".mldsa65.sig",
        }
    }
}

/// Pinned key, spelled `<algorithm>:<base64 of the raw public key>`.
#[derive(Debug, Clone)]
pub struct PublicKey {
    alg: Alg,
    raw: Vec<u8>,
}

impl PublicKey {
    pub fn parse(s: &str) -> Result<Self> {
        use base64::Engine as _;
        let (name, b64) = s
            .split_once(':')
            .context("public key must be spelled `<algorithm>:<base64>`")?;
        let alg = (Alg::ALL.into_iter().find(|a| a.name() == name))
            .context("public key algorithm must be `ed25519` or `mldsa65`")?;
        let raw = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .context("public key is not valid base64")?;
        if raw.len() != alg.key_len() {
            let (name, want) = (alg.name(), alg.key_len());
            bail!("{name} public key must be {want} bytes, got {}", raw.len());
        }
        Ok(Self { alg, raw })
    }

    pub fn alg(&self) -> Alg {
        self.alg
    }

    fn verifies(&self, bytes: &[u8], sig: &[u8]) -> bool {
        let checked = match self.alg {
            Alg::Ed25519 => UnparsedPublicKey::new(&ED25519, &self.raw).verify(bytes, sig),
            Alg::MlDsa65 => UnparsedPublicKey::new(&ML_DSA_65, &self.raw).verify(bytes, sig),
        };
        checked.is_ok()
    }
}

/// The algorithms `keys` pins, each once.
pub fn pinned_algs(keys: &[PublicKey]) -> Vec<Alg> {
    let pinned = |alg: &Alg| keys.iter().any(|k| k.alg == *alg);
    Alg::ALL.into_iter().filter(pinned).collect()
}

/// Exact bytes against the pinned keys. For each algorithm `keys` names, `sigs` must hold a
/// signature of that algorithm that verifies against one of the keys of that algorithm. Two
/// keys of one algorithm are two slots, so rotation is not a flag day.
///
/// The text of a signature is its file: base64, with whitespace allowed around it.
pub fn verify_signatures(
    bytes: &[u8],
    sigs: &[(Alg, impl AsRef<str>)],
    keys: &[PublicKey],
) -> Result<()> {
    use base64::Engine as _;
    if keys.is_empty() {
        bail!("no public key pinned for this source");
    }
    for alg in pinned_algs(keys) {
        let name = alg.name();
        let (_, text) = (sigs.iter().find(|(a, _)| *a == alg))
            .with_context(|| format!("no {name} signature"))?;
        let sig = base64::engine::general_purpose::STANDARD
            .decode(text.as_ref().trim())
            .with_context(|| format!("{name} signature file is not valid base64"))?;
        if !keys.iter().any(|k| k.alg == alg && k.verifies(bytes, &sig)) {
            bail!("{name} signature does not verify against any pinned key");
        }
    }
    Ok(())
}

/// [`verify_signatures`] for a document published with an Ed25519 signature alone. The check
/// fails when a key of another algorithm is pinned.
pub fn verify_signature(bytes: &[u8], sig_text: &str, keys: &[PublicKey]) -> Result<()> {
    verify_signatures(bytes, &[(Alg::Ed25519, sig_text)], keys)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;

    pub(crate) fn keypair() -> (String, aws_lc_rs::signature::Ed25519KeyPair) {
        use aws_lc_rs::signature::KeyPair as _;
        let rng = aws_lc_rs::rand::SystemRandom::new();
        let pkcs8 = aws_lc_rs::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = aws_lc_rs::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let key_str = format!("ed25519:{}", STANDARD.encode(kp.public_key().as_ref()));
        (key_str, kp)
    }

    /// An ML-DSA-65 key as it is pinned, and what signs with it: base64 of one signature.
    fn mldsa_keypair() -> (String, impl Fn(&[u8]) -> String) {
        use aws_lc_rs::signature::{KeyPair as _, PqdsaKeyPair, ML_DSA_65_SIGNING};
        let kp = PqdsaKeyPair::generate(&ML_DSA_65_SIGNING).unwrap();
        let key_str = format!("mldsa65:{}", STANDARD.encode(kp.public_key().as_ref()));
        let sign = move |body: &[u8]| {
            let mut sig = vec![0; ML_DSA_65_SIGNING.signature_len()];
            let len = kp.sign(body, &mut sig).unwrap();
            STANDARD.encode(&sig[..len])
        };
        (key_str, sign)
    }

    #[test]
    fn roundtrip_and_tamper() {
        let (key_str, kp) = keypair();
        let keys = vec![PublicKey::parse(&key_str).unwrap()];
        let body = b"the exact bytes";
        let sig = STANDARD.encode(kp.sign(body));

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
        assert!(PublicKey::parse("rsa:AAAA").is_err());
        // An Ed25519 key under the other algorithm's name is the wrong length for it.
        let (ed, _) = keypair();
        assert!(PublicKey::parse(&ed.replace("ed25519:", "mldsa65:")).is_err());
    }

    /// A valid signature from an unpinned key is still a miss; pinning is the check.
    #[test]
    fn other_signer_refused() {
        let (ours, _) = keypair();
        let (_, theirs) = keypair();
        let keys = vec![PublicKey::parse(&ours).unwrap()];
        let body = b"the exact bytes";
        let sig = STANDARD.encode(theirs.sign(body));
        assert!(verify_signature(body, &sig, &keys).is_err());
    }

    /// With both algorithms pinned, a document passes only with both signatures.
    #[test]
    fn every_pinned_algorithm_must_verify() {
        let (ed_key, ed) = keypair();
        let (pq_key, pq_sign) = mldsa_keypair();
        let (_, stranger_sign) = mldsa_keypair();
        let keys: Vec<_> = [&ed_key, &pq_key]
            .map(|k| PublicKey::parse(k).unwrap())
            .into();
        assert_eq!(pinned_algs(&keys), [Alg::Ed25519, Alg::MlDsa65]);

        let body = b"the exact bytes";
        let ed_sig = STANDARD.encode(ed.sign(body));
        let both = |pq_sig: &str| {
            let sigs = [(Alg::Ed25519, ed_sig.as_str()), (Alg::MlDsa65, pq_sig)];
            verify_signatures(body, &sigs, &keys)
        };
        assert!(both(&pq_sign(body)).is_ok());
        assert!(both(&pq_sign(b"other bytes")).is_err());
        assert!(both(&stranger_sign(body)).is_err());
        // The Ed25519 signature does not stand in for the missing one.
        assert!(both(&ed_sig).is_err());
        assert!(verify_signature(body, &ed_sig, &keys).is_err());

        // One algorithm pinned asks for that algorithm's signature alone.
        let pq_sig = pq_sign(body);
        let pq_only = [(Alg::MlDsa65, pq_sig.as_str())];
        assert!(verify_signatures(body, &pq_only, &keys[1..]).is_ok());
        assert!(verify_signatures(body, &pq_only, &keys).is_err());
    }

    /// A key and a signature as Bun's `node:crypto` writes them, the index repo's signer.
    #[test]
    fn mldsa65_verifies_a_node_crypto_signature() {
        let key = PublicKey::parse(include_str!("../testdata/mldsa65-interop.pub")).unwrap();
        let sig = include_str!("../testdata/mldsa65-interop.sig");
        let body = b"punktfunk mldsa65 interop vector\n";
        let sigs = [(Alg::MlDsa65, sig)];
        let keys = [key];
        assert!(verify_signatures(body, &sigs, &keys).is_ok());
        assert!(verify_signatures(b"other bytes", &sigs, &keys).is_err());
    }
}
