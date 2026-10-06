//! Leaf-certificate fingerprinting and pinned rustls verification for the RDP keeper.
//!
//! The keeper reaches TermService over loopback TLS and trusts exactly one leaf: the one
//! `seats rdp-trust` recorded. The leaf SHA-256 uses the same aws-lc-rs provider as rustls.
//! CertificateVerify is still checked, so replaying a public certificate without its key
//! cannot pass.

use std::sync::{Arc, Mutex};

pub fn cert_fingerprint(cert_der: &[u8]) -> [u8; 32] {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, cert_der)
        .as_ref()
        .try_into()
        .expect("SHA-256 output is 32 bytes")
}

#[derive(Debug)]
pub struct PinVerify {
    pin: Option<[u8; 32]>,
    observed: Arc<Mutex<Option<[u8; 32]>>>,
}

impl PinVerify {
    pub fn with_observed(pin: Option<[u8; 32]>, observed: Arc<Mutex<Option<[u8; 32]>>>) -> Self {
        Self { pin, observed }
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
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let fingerprint = cert_fingerprint(end_entity.as_ref());
        *self
            .observed
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(fingerprint);
        if self.pin.is_some_and(|pin| pin != fingerprint) {
            return Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
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
    use rustls::client::danger::ServerCertVerifier as _;

    #[test]
    fn pin_mismatch_records_the_leaf_before_rejecting() {
        let observed = Arc::new(Mutex::new(None));
        let verifier = PinVerify::with_observed(Some([7; 32]), observed.clone());
        let certificate = rustls::pki_types::CertificateDer::from(vec![1, 2, 3]);
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let result = verifier.verify_server_cert(
            &certificate,
            &[],
            &name,
            &[],
            rustls::pki_types::UnixTime::since_unix_epoch(std::time::Duration::ZERO),
        );
        assert!(result.is_err());
        assert_eq!(
            *observed.lock().unwrap(),
            Some(cert_fingerprint(&[1, 2, 3]))
        );
    }
}
