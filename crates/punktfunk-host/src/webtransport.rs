//! The browser plane: a WebTransport endpoint for `clients/web`
//! (`design/web-client-implementation-plan.md` Phase 1). Datagrams carry media, one bidirectional
//! stream carries control — the same split the native plane uses, because WebTransport *is* QUIC
//! and a browser cannot open ours.
//!
//! **Off unless asked for** (`--webtransport` / `PUNKTFUNK_WEBTRANSPORT=1`), the stance
//! `--gamestream` takes: a new externally-reachable transport does not appear on an upgrade.
//!
//! This plane mints its OWN ECDSA P-256 certificate, valid 13 days, and rotates it.
//! `serverCertificateHashes` refuses anything over two weeks, and the native identity
//! ([`crate::identity`]) is deliberately the opposite — long-lived, because clients pin it. The
//! key is never written to disk: minted at start-up, held in memory, replaced on rotation.
//!
//! [`published`] feeds an UNAUTHENTICATED management route, because a browser that has never
//! paired holds no client certificate. That route proves nothing to a first-time browser —
//! substitute both hash and certificate and it connects to you. PAKE pairing over the control
//! stream is what proves the peer, and it does not need the transport authenticated. A browser
//! that has already paired gets more: [`CertAttestation`] chains this plane's throwaway
//! certificate back to the host fingerprint it pinned.

mod datagrams;
mod session;

pub(crate) use datagrams::WebTransportPlane;
// The management API's device lane verifies the same key shape against the same digest, and
// must not grow a second opinion about either.
pub(crate) use session::{sha256, spki_p256_point};

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};
use wtransport::{Endpoint, Identity, ServerConfig};

/// Default listen port. Distinct from the native plane's 9777: this one speaks HTTP/3, and a
/// browser and a native client can be connected at the same time.
pub const DEFAULT_PORT: u16 = 9778;

/// Certificate lifetime. The spec's ceiling is two weeks and it is a hard refusal, so 13 days
/// leaves a day of margin for a client whose clock runs fast.
const VALIDITY_DAYS: u32 = 13;

/// Rotate a day before expiry, so a browser that fetched the hash a moment ago still finds the
/// certificate that hash names.
const ROTATE_AFTER: Duration = Duration::from_secs((VALIDITY_DAYS as u64 - 1) * 24 * 60 * 60);

/// What a browser needs before it can dial: the port, the hash to pin, and when that stops being
/// true. Published for the management route; contains nothing secret — a certificate hash is
/// learned by anyone who connects.
#[derive(Clone, Debug)]
pub struct Published {
    pub port: u16,
    /// Lowercase hex SHA-256 of the leaf DER, the form `serverCertificateHashes` wants.
    pub cert_hash: String,
    /// Unix seconds. A client past this must re-fetch before dialling.
    pub expires_at: u64,
    /// Proof that this plane belongs to the host a browser paired with. Absent on the legacy
    /// RSA identity ([`crate::identity`]), which the browser verifier does not implement.
    pub attestation: Option<CertAttestation>,
}

/// The long-lived native identity vouching for this plane's throwaway certificate.
///
/// A browser pins a durable host fingerprint at pairing, but this plane's certificate is
/// deliberately ephemeral and cannot be pinned across restarts. So the native key signs the hash
/// instead, and a paired browser checks the chain — hash the certificate, compare with its pin,
/// verify the signature — before it dials. Unpaired browsers ignore all of it, as they must:
/// nothing here authorises anything, and PAKE is still what proves the peer.
#[derive(Clone, Debug)]
pub struct CertAttestation {
    /// Hex ECDSA-P256-SHA256 signature, ASN.1 DER, over `CERT_SIG_CONTEXT` + [`Published::cert_hash`].
    pub cert_hash_sig: String,
    /// Base64 of the native identity's leaf certificate DER. A browser hashes this to check it
    /// against its pin, then verifies with the key inside it.
    pub host_cert_der: String,
}

/// Domain separation. The native key also signs X.509, so a signature it makes over a certificate
/// hash must not be replayable as one over anything else.
const CERT_SIG_CONTEXT: &str = "punktfunk-wt-cert-v1:";

/// Sign `cert_hash` with the native identity, and hand back the certificate that verifies it.
///
/// `None` rather than an error on the legacy RSA identity: a host still serving that pair has no
/// browser pairings to break, and failing start-up over it would be worse than not attesting.
fn attest(ident: &crate::identity::NativeIdentity, cert_hash: &str) -> Option<CertAttestation> {
    use base64::Engine as _;
    let key = rcgen::KeyPair::from_pem(&ident.key_pem).ok()?;
    if key.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
        tracing::warn!("native identity is not P-256, so the browser plane is not attested");
        return None;
    }
    let msg = format!("{CERT_SIG_CONTEXT}{cert_hash}");
    let sig = rcgen::SigningKey::sign(&key, msg.as_bytes()).ok()?;
    let (_, pem) = x509_parser::pem::parse_x509_pem(ident.cert_pem.as_bytes()).ok()?;
    Some(CertAttestation {
        cert_hash_sig: sig.iter().map(|b| format!("{b:02x}")).collect(),
        host_cert_der: base64::engine::general_purpose::STANDARD.encode(&pem.contents),
    })
}

/// Everything the plane needs to serve a browser.
///
/// A struct rather than six positional arguments through three call layers: half of them are
/// `Vec<String>` or `bool`, and a transposed pair would compile.
#[derive(Clone)]
pub struct Plane {
    pub bind: SocketAddr,
    /// Subject alternative names for the minted certificate — the addresses a browser may dial.
    pub sans: Vec<String>,
    /// Empty means any. See [`origin_allowed`].
    pub origins: Vec<String>,
    /// The long-lived host identity, which signs each minted certificate's hash.
    pub identity: crate::identity::NativeIdentity,
    pub pairing: std::sync::Arc<crate::native_pairing::NativePairing>,
    /// The native plane's flag, honoured identically here: off, a browser streams without
    /// proving anything, which is what `serve --open` asks for.
    pub require_pairing: bool,
}

/// A plane bound to one certificate. Sessions need the hash of the certificate *this* endpoint
/// presents, and a rotation replaces the endpoint — so it is captured here rather than read back
/// from [`published`], which would race the swap.
struct Serving {
    plane: Plane,
    /// Raw SHA-256 of the leaf DER. The SPAKE2 host identity and the channel binding both.
    cert_hash: [u8; 32],
    /// Last pairing attempt on this plane, for the same rate limit the native one applies.
    /// SPAKE2 caps a ceremony at one online guess; this caps the ceremonies.
    last_pairing: std::sync::Mutex<Option<std::time::Instant>>,
    /// The native plane's capturer, injector, mic and pairing store: an admitted browser runs
    /// the same session on the same state, which is why this plane is spawned from there.
    host: crate::native::SessionHost,
}

static PUBLISHED: RwLock<Option<Published>> = RwLock::new(None);

/// The live certificate's details, or `None` when the plane is not running.
pub fn published() -> Option<Published> {
    PUBLISHED.read().ok().and_then(|g| g.clone())
}

/// Stop advertising. The route answers 404 again, which is what a caller should see when the
/// plane is not listening.
fn withdraw() {
    if let Ok(mut g) = PUBLISHED.write() {
        *g = None;
    }
}

/// Mint a fresh short-lived identity. Returns it with what a browser would need to dial it —
/// the caller publishes that only once the endpoint is actually bound, so the route never
/// advertises a plane that is not listening.
fn mint(
    port: u16,
    sans: &[String],
    ident: &crate::identity::NativeIdentity,
) -> Result<(Identity, Published)> {
    let identity = Identity::self_signed_builder()
        .subject_alt_names(sans)
        .from_now_utc()
        .validity_days(VALIDITY_DAYS)
        .build()
        .context("mint the WebTransport identity")?;
    let leaf = identity
        .certificate_chain()
        .as_slice()
        .first()
        .context("minted identity has no leaf certificate")?;
    // `Sha256DigestFmt::DottedHex` is `aa:bb:…`; the browser wants raw bytes and our route
    // publishes plain hex, so strip the separators rather than hand-roll the digest.
    let cert_hash = leaf
        .hash()
        .fmt(wtransport::tls::Sha256DigestFmt::DottedHex)
        .replace(':', "");
    let expires_at =
        SystemTime::now() + Duration::from_secs(u64::from(VALIDITY_DAYS) * 24 * 60 * 60);
    let expires_at = expires_at
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    tracing::info!(
        port,
        cert_hash = %cert_hash,
        validity_days = VALIDITY_DAYS,
        "WebTransport identity minted (in memory, never persisted)"
    );
    let attestation = attest(ident, &cert_hash);
    Ok((
        identity,
        Published {
            port,
            cert_hash,
            expires_at,
            attestation,
        },
    ))
}

/// Run the plane until the process ends, re-minting the identity as it ages out.
///
/// A rotation rebuilds the endpoint, which drops whatever is connected. Once every twelve days,
/// and a browser reconnects on its own — cheaper than teaching quinn to swap a certificate under
/// live sessions, and the reconnect path has to work anyway.
///
/// `sem` is the native plane's session pool. A browser takes a slot from it like any client, so
/// `max_concurrent` means what it says whichever carrier the sessions arrive on.
pub(crate) async fn serve(
    plane: Plane,
    host: crate::native::SessionHost,
    sem: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    let port = plane.bind.port();
    loop {
        let (identity, publish) = mint(port, &plane.sans, &plane.identity)?;
        // Sessions bind to this exact certificate, so a rotation cannot leave one authenticating
        // against a hash the peer never saw.
        let cert_hash = unhex32(&publish.cert_hash)
            .context("the minted certificate hash is not 32 hex bytes")?;
        // Windows leaves an IPv6 socket v6-only, so `[::]` alone never hears an IPv4 browser.
        let config = match plane.bind {
            SocketAddr::V6(v6) => ServerConfig::builder()
                .with_bind_address_v6(v6, wtransport::config::Ipv6DualStackConfig::Allow),
            v4 => ServerConfig::builder().with_bind_address(v4),
        }
        .with_identity(identity)
        // A browser tab that is throttled in the background must not look like a dead peer.
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();
        // Bind first, publish second. The other order leaves the route advertising a hash for a
        // plane that never came up, which reads as the API lying.
        let endpoint = match Endpoint::server(config).context("bind the WebTransport endpoint") {
            Ok(endpoint) => endpoint,
            Err(e) => {
                withdraw();
                return Err(e);
            }
        };
        if let Ok(mut g) = PUBLISHED.write() {
            *g = Some(publish);
        }
        let bind = plane.bind;
        tracing::info!(%bind, "WebTransport plane listening");
        let serving = Arc::new(Serving {
            plane: plane.clone(),
            cert_hash,
            last_pairing: std::sync::Mutex::new(None),
            host: host.clone(),
        });
        // Accept until the certificate is due for replacement, then fall out and re-mint.
        tokio::select! {
            _ = accept_loop(endpoint, serving, sem.clone()) => {}
            () = tokio::time::sleep(ROTATE_AFTER) => {
                tracing::info!("WebTransport certificate due for rotation");
            }
        }
    }
}

async fn accept_loop(
    endpoint: Endpoint<wtransport::endpoint::endpoint_side::Server>,
    serving: Arc<Serving>,
    sem: Arc<tokio::sync::Semaphore>,
) {
    loop {
        let incoming = endpoint.accept().await;
        let serving = serving.clone();
        let sem = sem.clone();
        tokio::spawn(async move {
            if let Err(e) = session(incoming, &serving, sem).await {
                tracing::debug!(error = %e, "WebTransport session ended");
            }
        });
    }
}

/// Hex back to the 32 bytes SPAKE2 and the channel binding want. The published form is hex
/// because that is what `serverCertificateHashes` documentation and our API speak.
fn unhex32(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Is this page allowed to open a session?
///
/// WebTransport is **not** subject to CORS, so without this any page the user happens to have
/// open can reach the plane — the browser will not stop it. An empty allowlist means "anything",
/// which is what a host with no configured origins has to mean until the console can offer the
/// choice; the log line below is what tells an operator which value to set.
fn origin_allowed(origin: Option<&str>, allowed: &[String]) -> bool {
    allowed.is_empty() || origin.is_some_and(|o| allowed.iter().any(|a| a == o))
}

/// May this plane bind at all?
///
/// `serve --open` waives the device signature, so the origin list becomes the only thing between
/// the desktop and any page the user has open — and a page needs no certificate trust to dial,
/// because `serverCertificateHashes` never consults the trust store. Either choice alone is an
/// operator's to make. Together they are a machine any website can drive, and a startup refusal
/// is the honest answer: unpaired is what `--open` asked for, unreachable is not.
pub fn is_confined(require_pairing: bool, origins: &[String]) -> bool {
    require_pairing || !origins.is_empty()
}

/// One browser session: check the origin, take a session slot, then hand the connection to
/// [`session::run`]. `/echo` keeps Phase 1's loopback for the measurement pages.
async fn session(
    incoming: wtransport::endpoint::IncomingSession,
    serving: &Arc<Serving>,
    sem: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    let request = incoming.await.context("await session request")?;
    // The peer picks `:path` and nothing upstream value-checks it, so control characters would
    // reach the log ring verbatim and let an unauthenticated peer forge log lines.
    let path: String = request
        .path()
        .chars()
        .filter(|c| !c.is_control())
        .take(128)
        .collect();
    // Same treatment as `:path`: peer-chosen, and it reaches a log an operator reads back.
    let origin: Option<String> = request.origin().map(|o| {
        o.chars()
            .filter(|c| !c.is_control())
            .take(128)
            .collect::<String>()
    });
    if !origin_allowed(origin.as_deref(), &serving.plane.origins) {
        request.forbidden().await;
        tracing::warn!(
            origin = origin.as_deref().unwrap_or("<none>"),
            "WebTransport session refused: origin not in PUNKTFUNK_WEBTRANSPORT_ORIGINS"
        );
        return Ok(());
    }
    let connection = request.accept().await.context("accept session")?;
    tracing::info!(
        path = %path,
        origin = origin.as_deref().unwrap_or("<none>"),
        "WebTransport session accepted"
    );

    // `/echo` keeps Phase 1's behaviour so the measurement pages still work against a host that
    // streams on every other path.
    if path == "/echo" {
        return echo(connection).await;
    }

    // Slot after the handshake, as the native plane does: a full host still accepts, so the
    // browser sees a live path (keep-alive) instead of a silent dial timeout.
    let permit = sem
        .acquire_owned()
        .await
        .expect("session semaphore is never closed");
    let peer = connection.remote_address();
    match session::run(connection.clone(), serving.clone(), permit).await {
        Ok(crate::native::Served::Session) => tracing::info!(%peer, "browser session complete"),
        Ok(crate::native::Served::ProbeClose) => {}
        Err(e) => {
            // The typed close the native plane would send, and the same code and sentence on a
            // stream first: WebKit hands a page nothing about an application close, so the
            // stream is the only way a Safari user reads why.
            let code = e
                .downcast_ref::<session::Refusal>()
                .map_or(punktfunk_core::reject::SETUP_FAILED_CLOSE_CODE, |r| r.code);
            let detail = format!("{e:#}");
            // A browser renders this text, so prefer the user sentence; the chain is
            // the fallback only because there is nothing better to show yet.
            let said = crate::native::setup_failed_sentence(&e).unwrap_or(detail.clone());
            let mut cut = said.len().min(256);
            while !said.is_char_boundary(cut) {
                cut -= 1;
            }
            refuse(&connection, code, &said[..cut]).await;
            connection.close(wtransport::VarInt::from_u32(code), &said.as_bytes()[..cut]);
            tracing::warn!(%peer, code, error = %detail, "browser session ended with error");
        }
    }
    Ok(())
}

/// Say why on a fresh unidirectional stream, then give the browser a moment to read it. The
/// close that follows carries no retransmit, so the wait is what makes the message arrive.
async fn refuse(connection: &wtransport::Connection, code: u32, reason: &str) {
    let msg = punktfunk_core::quic::Refused {
        code,
        reason: reason.to_string(),
    }
    .encode();
    let sent = async {
        let mut uni = connection.open_uni().await?.await?;
        punktfunk_core::quic::io::write_msg(&mut uni, &msg).await?;
        uni.finish().await?;
        anyhow::Ok(())
    };
    if tokio::time::timeout(Duration::from_secs(1), sent)
        .await
        .is_ok()
    {
        let _ = tokio::time::timeout(Duration::from_millis(300), connection.closed()).await;
    }
}

/// Phase 1's echo, still reachable at `/echo`: the page connects, sends datagrams and control
/// bytes, and gets them back. It is what the seam's measurements run against.
async fn echo(connection: wtransport::Connection) -> Result<()> {
    loop {
        tokio::select! {
            datagram = connection.receive_datagram() => {
                let datagram = datagram.context("receive datagram")?;
                connection.send_datagram(&*datagram).context("echo datagram")?;
            }
            stream = connection.accept_bi() => {
                let (mut tx, mut rx) = stream.context("accept control stream")?;
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    while let Ok(Some(n)) = rx.read(&mut buf).await {
                        if tx.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two-week ceiling is a hard refusal in the browser, and a certificate that misses it
    /// fails at connect time with nothing useful to read. Check what we publish, and that the hash
    /// is the 64 lowercase hex characters `serverCertificateHashes` parses.
    #[test]
    fn minted_identity_is_short_lived_and_publishes_a_usable_hash() {
        let before = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ident = crate::identity::ephemeral().expect("mint a native identity");
        let (_identity, p) = mint(9778, &["localhost".to_string()], &ident).expect("mint");

        assert_eq!(p.port, 9778);
        assert_eq!(p.cert_hash.len(), 64, "SHA-256 as hex");
        assert!(
            p.cert_hash
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "lowercase hex only: {}",
            p.cert_hash
        );
        let lifetime = p.expires_at - before;
        assert!(
            lifetime < 14 * 24 * 60 * 60,
            "must stay under the spec's two weeks, got {lifetime}s"
        );
        assert!(lifetime > 12 * 24 * 60 * 60, "and not be pointlessly short");
        assert!(
            ROTATE_AFTER.as_secs() < lifetime,
            "rotation must come before expiry"
        );
    }

    /// The gate that stands in for the same-origin policy WebTransport does not get.
    #[test]
    fn origin_gate_admits_only_what_was_configured() {
        let allowed = vec!["https://host.local:47990".to_string()];
        assert!(origin_allowed(Some("https://host.local:47990"), &allowed));
        assert!(!origin_allowed(Some("https://evil.example"), &allowed));
        // A peer that sends no Origin at all must not slip past a configured list.
        assert!(!origin_allowed(None, &allowed));
        // Exact match only: a prefix or a suffix is a different origin.
        assert!(!origin_allowed(
            Some("https://host.local:47990.evil.example"),
            &allowed
        ));
        assert!(!origin_allowed(
            Some("https://evil.host.local:47990"),
            &allowed
        ));
        // Unconfigured means "any", which is what a host with no console setting has to mean.
        assert!(origin_allowed(Some("https://anything"), &[]));
        assert!(origin_allowed(None, &[]));
    }

    /// `--open` and an empty origin list are each an operator's choice; together they are a
    /// desktop any page can drive, because nothing else stands in front of the session.
    #[test]
    fn an_open_plane_must_name_the_pages_that_may_reach_it() {
        let named = vec!["https://web.punktfunk.io".to_string()];
        assert!(is_confined(true, &[]), "pairing carries it alone");
        assert!(
            is_confined(false, &named),
            "the origin list carries it alone"
        );
        assert!(is_confined(true, &named));
        assert!(!is_confined(false, &[]), "neither: refuse to bind");
    }

    /// The whole point of the attestation: a browser holding only a host fingerprint can decide
    /// whether this plane's throwaway certificate belongs to that host.
    #[test]
    fn attestation_verifies_against_the_pinned_host_certificate() {
        use base64::Engine as _;
        let ident = crate::identity::ephemeral().expect("mint a native identity");
        let cert_hash = "a".repeat(64);
        let a = attest(&ident, &cert_hash).expect("P-256 identity attests");

        // Step one of the browser's check: the certificate really is the one it pinned.
        let der = base64::engine::general_purpose::STANDARD
            .decode(&a.host_cert_der)
            .expect("host_cert_der is base64");
        let (_, expected) = x509_parser::pem::parse_x509_pem(ident.cert_pem.as_bytes()).unwrap();
        assert_eq!(
            der, expected.contents,
            "attested DER is the identity's leaf"
        );

        // Step two: the signature is that certificate's key over this hash, and nothing else.
        let cert = expected.parse_x509().unwrap();
        let key = cert.public_key().subject_public_key.data.to_vec();
        let sig: Vec<u8> = (0..a.cert_hash_sig.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&a.cert_hash_sig[i..i + 2], 16).unwrap())
            .collect();
        let verify = |msg: &str| {
            aws_lc_rs::signature::UnparsedPublicKey::new(
                &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
                &key,
            )
            .verify(msg.as_bytes(), &sig)
        };
        assert!(verify(&format!("{CERT_SIG_CONTEXT}{cert_hash}")).is_ok());
        // Domain separation and the hash itself both have to matter, or the signature proves
        // nothing about which certificate it names.
        assert!(
            verify(&cert_hash).is_err(),
            "context is part of the message"
        );
        assert!(
            verify(&format!("{CERT_SIG_CONTEXT}{}", "b".repeat(64))).is_err(),
            "a different hash must not verify"
        );
    }
}
