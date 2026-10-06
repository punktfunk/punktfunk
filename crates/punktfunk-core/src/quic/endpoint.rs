//! Shared QUIC endpoint construction (host + client).
//!
//! Transport: keep-alive is on (quinn defaults it off); idle is
//! [`DEFAULT_IDLE_TIMEOUT`], host-tunable. Datagram send buffer is 4 KiB so
//! audio/HID stay latest-wins under congestion. MTU discovery probes to the
//! sealed video datagram size (1472), or the sealed jumbo size when opted in.
//!
//! TLS: the host offers optional client auth; the client pins the host leaf via
//! [`crate::tls::PinVerify`]. This module only wires the verifier in.
//! Fingerprint helpers stay here so `quic::endpoint::cert_fingerprint` compiles.
//!
//! Jumbo (`PUNKTFUNK_JUMBO` / `PUNKTFUNK_WIRE_MTU`): both the host probe ceiling
//! and the client `max_udp_payload_size` advertisement must opt in, or discovery
//! cannot settle above 1472. See `design/shard-payload-reneg.md`.
use std::sync::{Arc, Mutex};

/// 8 s disconnect-detection window. Short enough that a reconnect does not join
/// a lingering session; long enough that a wifi roam does not false-close.
pub const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);

fn stream_transport() -> Arc<quinn::TransportConfig> {
    stream_transport_idle(DEFAULT_IDLE_TIMEOUT)
}

/// Idle clamped to 1 s..1 h so the QUIC VarInt millisecond conversion cannot
/// fail. Keep-alive is `min(idle/2, 4s)`: two PINGs per window, one lost PING
/// does not false-close.
fn stream_transport_idle(idle: std::time::Duration) -> Arc<quinn::TransportConfig> {
    use std::time::Duration;
    let idle = idle.clamp(Duration::from_secs(1), Duration::from_secs(3600));
    let keep_alive = (idle / 2).min(Duration::from_secs(4));
    let mut t = quinn::TransportConfig::default();
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(idle).expect("clamped idle timeout is a valid QUIC value"),
    ));
    t.keep_alive_interval(Some(keep_alive));
    // 4 KiB ≈ 200 ms of stereo Opus, ~14 ms of 48 kHz/24-bit PCM. Latest-wins
    // under congestion; do not raise for PCM — TransportConfig is built before
    // the plane is known, so a PCM-sized buffer would give Opus ~800 ms of lag.
    // Eviction is silent (`send_datagram` drop=true returns Ok); counters miss it.
    t.datagram_send_buffer_size(4 * 1024);
    // Probe to the sealed IPv4 video datagram (1472), not quinn's 1452: settle at
    // the ceiling proves the path carries full-size video; settle below proves it
    // cannot. Stock 1452 made a healthy path and a constrained one look the same.
    let mut mtud = quinn::MtuDiscoveryConfig::default();
    // Jumbo opt-in: probe to the sealed jumbo datagram so settle can prove a
    // jumbo path; grow stays client-ack-gated (`native/wire_mtu.rs`). Ceiling is
    // per-endpoint (IPv4 overhead, which covers v6). Extra failed probes only
    // when opted in.
    let probe_ceiling = match crate::config::jumbo_wire_mtu() {
        Some(mtu) => {
            let shard = crate::config::jumbo_shard_payload_for(
                mtu,
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            );
            crate::config::sealed_datagram_bytes(shard) as u16
        }
        None => crate::config::video_datagram_udp_ceiling() as u16,
    };
    mtud.upper_bound(probe_ceiling);
    t.mtu_discovery_config(Some(mtud));
    Arc::new(t)
}

/// Client `max_udp_payload_size` advertisement. A peer's MTU search is
/// `min(probe ceiling, the other side's advertisement)`, so raising only the
/// host probe can never settle above quinn's default 1472. Jumbo opt-in raises
/// this to the sealed jumbo size; without it the stock config is unchanged.
///
/// quinn's receive buffer scales with this (~2.9 MiB at 1472, ~18 MiB at jumbo
/// with GRO). Same gate as [`crate::config::jumbo_wire_mtu`].
fn endpoint_config() -> quinn::EndpointConfig {
    let mut cfg = quinn::EndpointConfig::default();
    if let Some(mtu) = crate::config::jumbo_wire_mtu() {
        let shard = crate::config::jumbo_shard_payload_for(
            mtu,
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        );
        let accept = crate::config::sealed_datagram_bytes(shard).clamp(1200, 65_527) as u16;
        if cfg.max_udp_payload_size(accept).is_ok() {
            tracing::info!(
                max_udp_payload_size = accept,
                wire_mtu = mtu,
                "jumbo opt-in: this endpoint advertises a jumbo QUIC receive ceiling, so the \
                 peer's MTU discovery can prove a jumbo path (it is capped by this value)"
            );
        }
    }
    cfg
}

/// Fresh self-signed cert. Tests/dev only; a host persists its identity and serves through
/// [`server_shared`].
pub fn server(addr: std::net::SocketAddr) -> anyhow_result::Result<quinn::Endpoint> {
    let cert = rcgen::generate_simple_self_signed(vec!["punktfunk".into()])
        .map_err(|e| anyhow_result::Error::msg(format!("self-signed cert: {e}")))?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    server_from_der(cert_der, key_der.into(), addr, DEFAULT_IDLE_TIMEOUT)
}

/// `pkf1`, the protocol before `punktfunk/2`. A host lists it only to answer an older client's
/// PIN ceremony and to refuse everything else that client sends with the wire-version code.
pub const QUIC_ALPN: &[u8] = b"pkf1";

fn server_from_der(
    cert_der: rustls::pki_types::CertificateDer<'static>,
    key_der: rustls::pki_types::PrivateKeyDer<'static>,
    addr: std::net::SocketAddr,
    idle: std::time::Duration,
) -> anyhow_result::Result<quinn::Endpoint> {
    let server_config = server_config(cert_der, key_der, idle, &[super::v2::registry::ALPN])?;
    Ok(quinn::Endpoint::server(server_config, addr)?)
}

/// A host endpoint answering `alpns`, and a clone of its socket for
/// [`MediaSender`](crate::transport::shared::MediaSender). TLS takes the first of the host's
/// ALPNs the client offers, so listing `pkf2` first picks it wherever both ends speak it.
pub fn server_shared(
    addr: std::net::SocketAddr,
    cert_pem: &str,
    key_pem: &str,
    idle: std::time::Duration,
    alpns: &[&[u8]],
) -> anyhow_result::Result<(quinn::Endpoint, std::net::UdpSocket)> {
    use rustls::pki_types::pem::PemObject;
    let cert_der = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
        .map_err(|e| anyhow_result::Error::msg(format!("cert pem: {e}")))?;
    let key_der = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|e| anyhow_result::Error::msg(format!("key pem: {e}")))?;
    let server_config = server_config(cert_der, key_der, idle, alpns)?;
    let socket = std::net::UdpSocket::bind(addr)?;
    crate::transport::grow_socket_buffers(&socket);
    let media = socket.try_clone()?;
    let runtime = quinn::default_runtime()
        .ok_or_else(|| anyhow_result::Error::msg("no async runtime found".into()))?;
    let ep = quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        Some(server_config),
        socket,
        runtime,
    )?;
    Ok((ep, media))
}

fn server_config(
    cert_der: rustls::pki_types::CertificateDer<'static>,
    key_der: rustls::pki_types::PrivateKeyDer<'static>,
    idle: std::time::Duration,
    alpns: &[&[u8]],
) -> anyhow_result::Result<quinn::ServerConfig> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    // Client auth is offered, not required: a missing cert still handshakes;
    // pairing decides at the app layer. Presented certs are fingerprinted after.
    let mut rustls_cfg = rustls::ServerConfig::builder()
        .with_client_cert_verifier(Arc::new(AcceptAnyClientCert))
        .with_single_cert(vec![cert_der], key_der)
        .map_err(|e| anyhow_result::Error::msg(format!("server config: {e}")))?;
    rustls_cfg.alpn_protocols = alpns.iter().map(|a| a.to_vec()).collect();
    let quic_cfg = quinn::crypto::rustls::QuicServerConfig::try_from(rustls_cfg)
        .map_err(|e| anyhow_result::Error::msg(format!("quic server config: {e}")))?;
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_cfg));
    server_config.transport_config(stream_transport_idle(idle));
    Ok(server_config)
}

/// Fresh self-signed PEM identity for a client to persist and present on connect.
pub fn generate_identity() -> anyhow_result::Result<(String, String)> {
    let cert = rcgen::generate_simple_self_signed(vec!["punktfunk-client".into()])
        .map_err(|e| anyhow_result::Error::msg(format!("self-signed cert: {e}")))?;
    Ok((cert.cert.pem(), cert.signing_key.serialize_pem()))
}

/// Client-leaf SHA-256 on the host side of `conn`, if the peer presented one.
pub fn peer_fingerprint(conn: &quinn::Connection) -> Option<[u8; 32]> {
    let identity = conn.peer_identity()?;
    let certs = identity
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .ok()?;
    certs.first().map(|c| cert_fingerprint(c.as_ref()))
}

/// Re-export of [`crate::tls::cert_fingerprint`] so `quic::endpoint::cert_fingerprint` stays.
pub use crate::tls::cert_fingerprint;

/// SHA-256 of the PEM's DER. Must match the client's on-wire hash (pairing UX).
pub fn fingerprint_of_pem(cert_pem: &str) -> anyhow_result::Result<[u8; 32]> {
    use rustls::pki_types::pem::PemObject;
    let der = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
        .map_err(|e| anyhow_result::Error::msg(format!("cert pem: {e}")))?;
    Ok(cert_fingerprint(der.as_ref()))
}

/// Skip host-cert verification. For TOFU that persists the observed pin, use [`client_pinned`].
pub fn client_insecure() -> anyhow_result::Result<quinn::Endpoint> {
    client_pinned(None).0
}

/// Endpoint plus the slot [`crate::tls::PinVerify`] writes the observed host fingerprint into.
pub type PinnedClient = (
    anyhow_result::Result<quinn::Endpoint>,
    Arc<Mutex<Option<[u8; 32]>>>,
);

/// Host-leaf pin. `Some(sha256)` rejects a mismatch; `None` is TOFU. Either way
/// the observed fingerprint is written to the returned slot during the handshake.
pub fn client_pinned(pin: Option<[u8; 32]>) -> PinnedClient {
    client_pinned_with_identity(pin, None)
}

/// [`client_pinned`] plus optional PEM client identity (TLS client auth).
pub fn client_pinned_with_identity(
    pin: Option<[u8; 32]>,
    identity: Option<(&str, &str)>,
) -> PinnedClient {
    client_pinned_offering(pin, identity, &[super::v2::registry::ALPN])
}

/// [`client_pinned_with_identity`] offering `alpns`, in order.
pub fn client_pinned_offering(
    pin: Option<[u8; 32]>,
    identity: Option<(&str, &str)>,
    alpns: &[&[u8]],
) -> PinnedClient {
    let observed = Arc::new(Mutex::new(None));
    let ep = (|| {
        let client_cfg = client_config(pin, identity, &observed, alpns)?;
        // `Endpoint::client` hardcodes `EndpointConfig::default()` (1472-byte
        // `max_udp_payload_size`), which would cap the host's MTU search. Build by
        // hand so [`endpoint_config`] can advertise jumbo.
        let socket = std::net::UdpSocket::bind("0.0.0.0:0")?;
        let runtime = quinn::default_runtime()
            .ok_or_else(|| anyhow_result::Error::msg("no async runtime found".into()))?;
        let mut ep = quinn::Endpoint::new(endpoint_config(), None, socket, runtime)?;
        ep.set_default_client_config(client_cfg);
        Ok(ep)
    })();
    (ep, observed)
}

/// A client endpoint and the media it receives, on one shared socket
/// ([`crate::transport::shared`]).
pub type SharedClient = (
    anyhow_result::Result<(quinn::Endpoint, crate::transport::shared::ClientMedia)>,
    Arc<Mutex<Option<[u8; 32]>>>,
);

/// [`client_pinned_with_identity`] on one socket for QUIC and media, offering `alpns` in
/// order. The endpoint never lets the host grease the QUIC fixed bit, which is what keeps a
/// QUIC packet from reading as media. The media side is the session's transport once the host
/// answers `pkf2`.
pub fn client_shared(
    pin: Option<[u8; 32]>,
    identity: Option<(&str, &str)>,
    alpns: &[&[u8]],
) -> SharedClient {
    let observed = Arc::new(Mutex::new(None));
    let ep = (|| {
        let client_cfg = client_config(pin, identity, &observed, alpns)?;
        let (socket, media) =
            crate::transport::shared::client_socket("0.0.0.0:0".parse().unwrap())?;
        let runtime = quinn::default_runtime()
            .ok_or_else(|| anyhow_result::Error::msg("no async runtime found".into()))?;
        let mut cfg = endpoint_config();
        cfg.grease_quic_bit(false);
        let mut ep = quinn::Endpoint::new_with_abstract_socket(cfg, None, socket, runtime)?;
        ep.set_default_client_config(client_cfg);
        Ok((ep, media))
    })();
    (ep, observed)
}

fn client_config(
    pin: Option<[u8; 32]>,
    identity: Option<(&str, &str)>,
    observed: &Arc<Mutex<Option<[u8; 32]>>>,
    alpns: &[&[u8]],
) -> anyhow_result::Result<quinn::ClientConfig> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let builder = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(crate::tls::PinVerify::with_observed(
            pin,
            observed.clone(),
        )));
    let mut rustls_cfg = match identity {
        None => builder.with_no_client_auth(),
        Some((cert_pem, key_pem)) => {
            use rustls::pki_types::pem::PemObject;
            let cert = rustls::pki_types::CertificateDer::from_pem_slice(cert_pem.as_bytes())
                .map_err(|e| anyhow_result::Error::msg(format!("client cert pem: {e}")))?;
            let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
                .map_err(|e| anyhow_result::Error::msg(format!("client key pem: {e}")))?;
            builder
                .with_client_auth_cert(vec![cert], key)
                .map_err(|e| anyhow_result::Error::msg(format!("client auth: {e}")))?
        }
    };
    rustls_cfg.alpn_protocols = alpns.iter().map(|a| a.to_vec()).collect();
    let quic_cfg = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_cfg)
        .map_err(|e| anyhow_result::Error::msg(format!("quic client config: {e}")))?;
    let mut client_cfg = quinn::ClientConfig::new(Arc::new(quic_cfg));
    client_cfg.transport_config(stream_transport());
    Ok(client_cfg)
}

/// The `punktfunk/2` media secrets of session `session_id` on `conn`, from its TLS exporter.
/// Both ends call this with the same arguments and hold the same keys; none crosses the wire.
pub fn media_keys(
    conn: &quinn::Connection,
    session_id: &[u8; 16],
    suite: crate::crypto::MediaSuite,
) -> Option<crate::crypto::MediaKeys> {
    use zeroize::Zeroize;
    let mut out = [0u8; 32];
    conn.export_keying_material(
        &mut out,
        super::v2::registry::MEDIA_EXPORTER_LABEL,
        session_id,
    )
    .ok()?;
    let keys = crate::crypto::MediaKeys::derive(&out, suite);
    out.zeroize();
    Some(keys)
}

/// Whether a dial failed because the peer speaks none of the offered protocols: TLS alert 120
/// (`no_application_protocol`), from either end.
pub fn refused_alpn(e: &quinn::ConnectionError) -> bool {
    let alert = quinn::TransportErrorCode::crypto(120);
    match e {
        quinn::ConnectionError::ConnectionClosed(c) => c.error_code == alert,
        quinn::ConnectionError::TransportError(t) => t.code == alert,
        _ => false,
    }
}

/// The ALPN a connection settled on, `None` before the handshake or without one.
pub fn negotiated_alpn(conn: &quinn::Connection) -> Option<Vec<u8>> {
    conn.handshake_data()?
        .downcast::<quinn::crypto::rustls::HandshakeData>()
        .ok()?
        .protocol
}

/// Minimal error plumbing without pulling anyhow into punktfunk-core's public API.
pub mod anyhow_result {
    pub type Result<T> = std::result::Result<T, Error>;
    #[derive(Debug)]
    pub struct Error(String);
    impl Error {
        pub fn msg(s: String) -> Self {
            Error(s)
        }
    }
    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for Error {}
    impl From<std::io::Error> for Error {
        fn from(e: std::io::Error) -> Self {
            Error(e.to_string())
        }
    }
}

/// Accept any client cert, but verify the handshake signature. Possession of the
/// key is what makes [`peer_fingerprint`] meaningful; pairing is application-layer.
#[derive(Debug)]
struct AcceptAnyClientCert;

impl rustls::server::danger::ClientCertVerifier for AcceptAnyClientCert {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn verify_client_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
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
    use crate::quic::endpoint;

    #[test]
    fn fingerprint_is_sha256_of_der() {
        let a = endpoint::cert_fingerprint(b"cert-a");
        assert_eq!(a, endpoint::cert_fingerprint(b"cert-a"));
        assert_ne!(a, endpoint::cert_fingerprint(b"cert-b"));
    }

    #[test]
    fn absurd_idle_timeout_is_clamped_not_a_panic() {
        let _ = super::stream_transport_idle(std::time::Duration::MAX);
        let _ = super::stream_transport_idle(std::time::Duration::ZERO);
    }

    /// Loopback MTU is 64 KiB, so only endpoint config can cap the search.
    /// Server-jumbo / client-stock must settle at 1472; both-jumbo must reach the
    /// sealed jumbo datagram. Sets process env: run ignored, `--test-threads=1`.
    #[tokio::test]
    #[ignore = "measurement: sets process env and takes ~15 s of wall clock"]
    // `set_var` is unsafe in edition 2024; this ignored test is the single-threaded
    // env-knob case documented on the fn.
    #[allow(unsafe_code)]
    async fn mtu_discovery_climbs_only_as_high_as_the_peer_advertises() {
        async fn climb(server_jumbo: bool, client_jumbo: bool) -> (u16, u128) {
            let set = |on: bool| {
                // SAFETY: this `#[ignore]`d measurement is documented above to run alone with
                // `--test-threads=1`, so no concurrent thread reads or writes the environment.
                unsafe {
                    if on {
                        std::env::set_var("PUNKTFUNK_JUMBO", "1");
                    } else {
                        std::env::remove_var("PUNKTFUNK_JUMBO");
                    }
                }
            };
            set(server_jumbo);
            let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
            let addr = server.local_addr().unwrap();
            set(client_jumbo);
            let client = endpoint::client_insecure().unwrap();
            set(false);
            let accept = tokio::spawn(async move {
                let incoming = server.accept().await.expect("incoming");
                let conn = incoming.await.expect("host side connects");
                (server, conn)
            });
            let client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
            let (_server_ep, host_conn) = accept.await.unwrap();
            // Probes ride `poll_transmit`; a stream write is what starts the search.
            let mut s = host_conn.open_uni().await.unwrap();
            s.write_all(b"go").await.unwrap();
            let want = crate::config::sealed_datagram_bytes(crate::config::jumbo_shard_payload_for(
                9000,
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            )) as u16;
            let t0 = std::time::Instant::now();
            let mut mtu = host_conn.stats().path.current_mtu;
            while t0.elapsed() < std::time::Duration::from_secs(6) && mtu < want {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                mtu = host_conn.stats().path.current_mtu;
            }
            let elapsed = t0.elapsed().as_millis();
            drop(client_conn);
            drop(client);
            (mtu, elapsed)
        }

        let (capped, _) = climb(true, false).await;
        println!("leg A (server opted in, client not): settled at {capped} B UDP payload");
        assert_eq!(
            capped, 1472,
            "a peer that advertises the stock max_udp_payload_size caps the search at 1472 — \
             the whole point of raising it on the client endpoint"
        );

        let (grown, ms) = climb(true, true).await;
        println!("leg B (both opted in): reached {grown} B UDP payload in {ms} ms");
        assert!(
            grown >= 8972,
            "both sides opted in, loopback MTU is 64 KiB — discovery should reach the sealed \
             jumbo datagram, got {grown}"
        );
    }
}
