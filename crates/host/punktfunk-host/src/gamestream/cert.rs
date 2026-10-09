//! Host RSA-2048 identity: GameStream `plaincert` and the TLS server cert on
//! 47984 (Moonlight pins it). Pairing hashes bind the cert's X.509
//! `signatureValue` bytes, extracted here.
//!
//! Persist as `cert.pem` / `key.pem` under the config dir. Tests in this
//! file pin minting and the GameStream TLS handshake.

use anyhow::{anyhow, Context, Result};
use pf_paths::config_dir;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rsa::RsaPrivateKey;
// `rsa 0.9` re-export: `SigningKey`'s type param must be this `digest 0.10`
// `Sha256`, not crate-wide `sha2 0.11`. See Cargo.toml.
use rsa::sha2::Sha256;
use std::fs;

pub struct ServerIdentity {
    /// Pairing `plaincert` (hex of this PEM) and the TLS server cert.
    pub cert_pem: String,
    pub key_pem: String,
    /// X.509 `signatureValue` — bound into the pairing challenge hashes.
    pub signature: Vec<u8>,
    /// Pairing `sign256`.
    pub signing_key: SigningKey<Sha256>,
}

impl ServerIdentity {
    pub fn load_or_create() -> Result<ServerIdentity> {
        let dir = config_dir();
        let cert_path = dir.join("cert.pem");
        let key_path = dir.join("key.pem");
        // This never regenerates while both files are non-empty, so a pair planted before the
        // first elevated run would become the host identity. Disown it first: hardening the
        // dir does not, and `create_private_dir` re-owns contents, erasing the evidence.
        // Not `||`: both halves must be quarantined, and short-circuiting would skip the second.
        let planted = crate::planted::quarantine_planted_secret(&cert_path)
            | crate::planted::quarantine_planted_secret(&key_path);
        pf_paths::create_private_dir(&dir).ok();
        let (cert_pem, key_pem) = match (
            fs::read_to_string(&cert_path),
            fs::read_to_string(&key_path),
        ) {
            (Ok(c), Ok(k)) if !planted && !c.trim().is_empty() && !k.trim().is_empty() => (c, k),
            _ => {
                let (c, k) = generate()?;
                pf_paths::write_secret_file(&key_path, k.as_bytes())
                    .with_context(|| format!("write {}", key_path.display()))?;
                pf_paths::write_secret_file(&cert_path, c.as_bytes())
                    .with_context(|| format!("write {}", cert_path.display()))?;
                tracing::info!(path = %cert_path.display(), "generated punktfunk host certificate (RSA-2048, key 0600)");
                (c, k)
            }
        };
        Self::from_pems(cert_pem, key_pem)
    }

    /// Parse PEMs; no I/O.
    pub fn from_pems(cert_pem: String, key_pem: String) -> Result<ServerIdentity> {
        let priv_key = RsaPrivateKey::from_pkcs8_pem(&key_pem).context("parse host private key")?;
        let signing_key = SigningKey::<Sha256>::new(priv_key);
        let signature = cert_signature(&cert_pem)?;
        Ok(ServerIdentity {
            cert_pem,
            key_pem,
            signature,
            signing_key,
        })
    }

    /// In-memory identity; does not touch the config dir.
    #[cfg(test)]
    pub fn ephemeral() -> Result<ServerIdentity> {
        let (cert_pem, key_pem) = generate()?;
        Self::from_pems(cert_pem, key_pem)
    }
}

fn generate() -> Result<(String, String)> {
    // rcgen cannot generate RSA (`KeyGenerationUnavailable`); Moonlight needs RSA-2048.
    // Mint with `rsa`, self-sign via rcgen, return the same PKCS#8 PEM `from_pems` re-parses.
    // `RsaPrivateKey::new` wants rand_core 0.6's `CryptoRngCore`. Our `rand` 0.9 `ThreadRng`
    // is a different trait of the same name — use `rsa::rand_core::OsRng`.
    let priv_key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048)
        .context("generate RSA-2048 host key")?;
    let key_pem = priv_key
        .to_pkcs8_pem(LineEnding::LF)
        .context("encode host key as PKCS#8 PEM")?
        .to_string();
    let key = rcgen::KeyPair::from_pkcs8_pem_and_sign_algo(&key_pem, &rcgen::PKCS_RSA_SHA256)
        .context("load RSA host key into rcgen")?;
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).context("cert params")?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "punktfunk");
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2040, 1, 1);
    let cert = params.self_signed(&key).context("self-sign cert")?;
    Ok((cert.pem(), key_pem))
}

fn cert_signature(cert_pem: &str) -> Result<Vec<u8>> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(cert_pem.as_bytes())
        .map_err(|e| anyhow!("parse cert pem: {e}"))?;
    let x509 = pem.parse_x509().context("parse x509")?;
    Ok(x509.signature_value.data.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
    use rustls::{
        ClientConfig, ClientConnection, DigitallySignedStruct, ServerConnection, SignatureScheme,
    };
    use std::sync::Arc;

    /// Moonlight pins the host cert out of band; it does not chain-verify.
    /// Same shape as `AcceptAnyClientCert` the other way — not webpki-clean.
    #[derive(Debug)]
    struct PinsOutOfBand(Arc<CryptoProvider>);

    impl ServerCertVerifier for PinsOutOfBand {
        fn verify_server_cert(
            &self,
            _e: &CertificateDer,
            _i: &[CertificateDer],
            _s: &ServerName,
            _o: &[u8],
            _n: UnixTime,
        ) -> std::result::Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &CertificateDer,
            d: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &CertificateDer,
            d: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    fn parts(
        cert_pem: &str,
        key_pem: &str,
    ) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        let certs = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("cert pem")
            .into_iter()
            .map(|c| c.into_owned())
            .collect();
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
            .expect("key pem")
            .clone_key();
        (certs, key)
    }

    fn pump(client: &mut ClientConnection, server: &mut ServerConnection) {
        for _ in 0..40 {
            while client.wants_write() {
                let mut buf = Vec::new();
                client.write_tls(&mut buf).expect("client write");
                let mut cur = &buf[..];
                while !cur.is_empty() {
                    if server.read_tls(&mut cur).expect("server read") == 0 {
                        break;
                    }
                    server.process_new_packets().expect("server handshake");
                }
            }
            while server.wants_write() {
                let mut buf = Vec::new();
                server.write_tls(&mut buf).expect("server write");
                let mut cur = &buf[..];
                while !cur.is_empty() {
                    if client.read_tls(&mut cur).expect("client read") == 0 {
                        break;
                    }
                    client.process_new_packets().expect("client handshake");
                }
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                return;
            }
        }
        panic!("handshake did not converge");
    }

    /// Mutual handshake vs `tls::server_config`. Returns (version, kx group, client certs seen).
    fn handshake_against_host(
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> (String, String, usize) {
        let (host_cert, host_key) = generate().expect("host identity");
        // Moonlight client identity is the same RSA-2048 self-signed shape.
        let (peer_cert, peer_key) = generate().expect("peer identity");

        let server_cfg =
            crate::gamestream::tls::server_config(&host_cert, &host_key).expect("server config");
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let (pc, pk) = parts(&peer_cert, &peer_key);
        let client_cfg = ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(versions)
            .expect("client versions")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinsOutOfBand(provider)))
            .with_client_auth_cert(pc, pk)
            .expect("client auth cert");

        let mut server = ServerConnection::new(server_cfg).expect("server conn");
        let mut client = ClientConnection::new(
            Arc::new(client_cfg),
            ServerName::try_from("punktfunk").unwrap(),
        )
        .expect("client conn");
        pump(&mut client, &mut server);

        let version = format!("{:?}", client.protocol_version().expect("version"));
        let kx = client
            .negotiated_key_exchange_group()
            .map(|g| format!("{:?}", g.name()))
            .unwrap_or_default();
        let seen = server.peer_certificates().map(|c| c.len()).unwrap_or(0);
        (version, kx, seen)
    }

    #[test]
    fn generate_mints_a_loadable_rsa2048_identity() {
        let (cert_pem, key_pem) = generate().expect("generate");
        assert!(cert_pem.contains("BEGIN CERTIFICATE"), "cert is not PEM");
        assert!(
            key_pem.contains("BEGIN PRIVATE KEY"),
            "key is not PKCS#8 PEM"
        );

        let identity = ServerIdentity::from_pems(cert_pem, key_pem).expect("from_pems");
        // RSA-2048 PKCS#1 signature is 256 bytes. Pins key size without an `rsa` accessor.
        assert_eq!(
            identity.signature.len(),
            256,
            "host cert signature should be RSA-2048 (256 bytes)"
        );
    }

    #[test]
    fn moonlight_shaped_peer_completes_a_tls12_mutual_handshake() {
        let (version, _kx, client_certs_seen) = handshake_against_host(&[&rustls::version::TLS12]);
        assert_eq!(version, "TLSv1_2", "Moonlight negotiates TLS 1.2");
        assert_eq!(
            client_certs_seen, 1,
            "mutual TLS: the host must receive the peer's client cert"
        );
    }

    #[test]
    fn tls13_negotiates_the_post_quantum_group() {
        let (version, kx, client_certs_seen) = handshake_against_host(&[&rustls::version::TLS13]);
        assert_eq!(version, "TLSv1_3");
        assert_eq!(
            kx, "X25519MLKEM768",
            "post-quantum key exchange must be preferred"
        );
        assert_eq!(client_certs_seen, 1);
    }
}
