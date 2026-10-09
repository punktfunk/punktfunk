//! Certificate-fingerprint hash and the fingerprint-pinning
//! [`ServerCertVerifier`](rustls::client::danger::ServerCertVerifier) (`PinVerify`).
//! Trust is the SHA-256 of the host's self-signed leaf (TOFU-pinned), not a CA
//! chain. QUIC connect, game-library HTTP, and the tray status poll share this
//! verifier. Behind the light `tls` feature (rustls + sha2, no QUIC runtime);
//! the heavier `quic` feature pulls it in.

use std::sync::{Arc, Mutex};

/// Blocking HTTP agent over a caller-built `rustls::ClientConfig`, so HTTP can
/// use [`PinVerify`].
#[cfg(feature = "ureq-tls")]
pub mod ureq_agent;

/// Install aws-lc-rs as this process's rustls provider. Call once, early, from `main`.
///
/// A second rustls backend in the tree makes `ClientConfig::builder()` panic
/// instead of inferring one; this call makes the choice explicit. Idempotent:
/// losing the race to another installer is expected — every caller here installs
/// the same provider.
pub fn install_default_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// A pinned client config up to its client-auth choice: the aws-lc-rs provider (mixing rustls
/// providers panics), rustls's safe protocol versions and `verify`. Finish it with
/// `with_no_client_auth` or `with_client_auth_cert`.
pub fn pinned_builder(
    verify: PinVerify,
) -> Result<
    rustls::ConfigBuilder<rustls::ClientConfig, rustls::client::WantsClientCert>,
    rustls::Error,
> {
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(verify)))
}

/// SHA-256 of the certificate DER — the fingerprint clients pin. Re-exported as
/// `crate::quic::endpoint::cert_fingerprint` for callers that already reach it there.
pub fn cert_fingerprint(cert_der: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(cert_der).into()
}

/// Fingerprint-pinning verifier. `pin = Some(sha256)` rejects a leaf that does
/// not hash to `sha256`; `pin = None` accepts any leaf (TOFU) — pair with
/// [`with_observed`](Self::with_observed) so the embedder can persist and pin later.
///
/// Handshake signatures are still verified: `CertificateVerify` proves the peer
/// holds the pinned cert's private key. Skip it and a MITM can replay the
/// (public) cert, match the pin, and finish with its own key.
#[derive(Debug)]
pub struct PinVerify {
    pin: Option<[u8; 32]>,
    observed: Option<Arc<Mutex<Option<[u8; 32]>>>>,
}

impl PinVerify {
    /// Pin `pin` (or accept any when `None`) without recording the leaf. HTTP
    /// clients use this: known pin or accept-any, nothing to persist.
    pub fn new(pin: Option<[u8; 32]>) -> Self {
        Self {
            pin,
            observed: None,
        }
    }

    /// Like [`new`](Self::new), and writes the observed leaf fingerprint into
    /// `slot` during the handshake so a TOFU caller can pin it on the next connect.
    pub fn with_observed(pin: Option<[u8; 32]>, slot: Arc<Mutex<Option<[u8; 32]>>>) -> Self {
        Self {
            pin,
            observed: Some(slot),
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for PinVerify {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        // Hash the leaf only when a pin must be checked or a slot recorded.
        // Accept-any without recording (unpinned HTTP) skips it.
        if self.pin.is_some() || self.observed.is_some() {
            let fp = cert_fingerprint(end_entity.as_ref());
            if let Some(slot) = &self.observed {
                *slot.lock().unwrap() = Some(fp);
            }
            if let Some(expected) = self.pin {
                if fp != expected {
                    return Err(rustls::Error::InvalidCertificate(
                        rustls::CertificateError::ApplicationVerificationFailure,
                    ));
                }
            }
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::aws_lc_rs::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

    /// Drive the pin check. `verify_server_cert` only hashes the leaf, so arbitrary
    /// bytes stand in for DER here.
    fn verify(v: &PinVerify, cert_bytes: &[u8]) -> std::result::Result<(), rustls::Error> {
        let der = CertificateDer::from(cert_bytes.to_vec());
        let name = ServerName::try_from("punktfunk").unwrap();
        v.verify_server_cert(
            &der,
            &[],
            &name,
            &[],
            UnixTime::since_unix_epoch(std::time::Duration::ZERO),
        )
        .map(|_| ())
    }

    #[test]
    fn matching_pin_accepts_and_mismatch_is_rejected() {
        let cert = b"the-host-leaf-cert";
        let good = cert_fingerprint(cert);
        assert!(verify(&PinVerify::new(Some(good)), cert).is_ok());

        let mut wrong = good;
        wrong[0] ^= 0xff;
        match verify(&PinVerify::new(Some(wrong)), cert) {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )) => {}
            other => panic!("a pin mismatch must be rejected, got {other:?}"),
        }
    }

    #[test]
    fn no_pin_accepts_any_and_records_the_observed_fingerprint() {
        let cert = b"whatever-the-host-presents";
        let slot = Arc::new(Mutex::new(None));
        let v = PinVerify::with_observed(None, slot.clone());
        assert!(verify(&v, cert).is_ok());
        assert_eq!(*slot.lock().unwrap(), Some(cert_fingerprint(cert)));
    }
}
