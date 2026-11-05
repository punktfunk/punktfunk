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
pub(crate) mod mgmt;
mod session;

pub(crate) use datagrams::WebTransportPlane;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};
use wtransport::{Endpoint, Identity, ServerConfig};

/// Default listen port. Distinct from the native plane's 9777: this one speaks HTTP/3, and a
/// browser and a native client can be connected at the same time.
pub const DEFAULT_PORT: u16 = pf_host_config::listen::BROWSER_PORT;

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
    /// Proof that this plane belongs to the host a browser paired with. Absent when the native
    /// identity ([`crate::identity`]) is not P-256, which the browser verifier does not implement.
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
    /// Hex ECDSA-P256-SHA256 signature, ASN.1 DER, over
    /// [`punktfunk_core::tls::PLANE_ATTESTATION_CONTEXT`] + [`Published::cert_hash`].
    pub cert_hash_sig: String,
    /// Base64 of the native identity's leaf certificate DER. A browser hashes this to check it
    /// against its pin, then verifies with the key inside it.
    pub host_cert_der: String,
}

/// Sign `cert_hash` with the native identity, and hand back the certificate that verifies it.
/// [`punktfunk_core::tls::verify_plane_attestation`] is the check.
///
/// `None` rather than an error when the identity is not P-256: failing start-up over it would be
/// worse than not attesting.
fn attest(ident: &crate::identity::NativeIdentity, cert_hash: &str) -> Option<CertAttestation> {
    use base64::Engine as _;
    let key = rcgen::KeyPair::from_pem(&ident.key_pem).ok()?;
    if key.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
        tracing::warn!("native identity is not P-256, so the browser plane is not attested");
        return None;
    }
    let msg = format!(
        "{}{cert_hash}",
        punktfunk_core::tls::PLANE_ATTESTATION_CONTEXT
    );
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
    /// This plane's pairing clocks, the native plane's rule. SPAKE2 caps a ceremony at one online
    /// guess; these cap the ceremonies.
    pairing_clocks: crate::native::PairingClocks,
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
/// A rotation swaps the certificate on the bound endpoint: live sessions keep the one they
/// authenticated against, new ones get the new one. A rebind would fail while any session
/// still holds the socket.
///
/// `sem` is the native plane's session pool. A browser takes a slot from it like any client, so
/// `max_concurrent` means what it says whichever carrier the sessions arrive on.
pub(crate) async fn serve(
    plane: Plane,
    host: crate::native::SessionHost,
    sem: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    let port = plane.bind.port();
    let mut bound = None;
    loop {
        let (identity, publish) = mint(port, &plane.sans, &plane.identity)?;
        // Sessions bind to this exact certificate, so a rotation cannot leave one authenticating
        // against a hash the peer never saw.
        let cert_hash = <[u8; 32] as hex::FromHex>::from_hex(&publish.cert_hash)
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
        let endpoint = match &bound {
            None => Endpoint::server(config)
                .context("bind the WebTransport endpoint")
                .map(|e| &*bound.insert(e)),
            Some(endpoint) => endpoint
                .reload_config(config, false)
                .context("swap the WebTransport certificate")
                .map(|()| endpoint),
        };
        let endpoint = match endpoint {
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
            pairing_clocks: Default::default(),
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

/// The plane for the life of the process, or on a seat host as the box says ([`follow_box`]).
pub(crate) async fn run(
    plane: Plane,
    host: crate::native::SessionHost,
    sem: Arc<tokio::sync::Semaphore>,
) {
    if pf_paths::seat::role().is_seat_host() {
        return follow_box(plane, host, sem).await;
    }
    let bind = plane.bind;
    if let Err(e) = serve(plane, host, sem).await {
        tracing::error!(%bind, error = %e, "WebTransport plane stopped");
    }
}

/// How often a seat host reads the box's Seat defaults.
const FOLLOW_EVERY: Duration = Duration::from_secs(1);

/// How long a seat's plane waits to bind again: its last sessions may still hold the port.
const REBIND_AFTER: Duration = Duration::from_secs(5);

/// A seat host's plane: up while the box streams to browsers, under the box's origins, and closed
/// once it stops, so a seat that was running when the switch moved follows it within seconds. A
/// seat host the box wrote no Seat defaults for follows its own switch. Live sessions outlast a
/// close.
async fn follow_box(
    base: Plane,
    host: crate::native::SessionHost,
    sem: Arc<tokio::sync::Semaphore>,
) {
    let mut up: Option<(Vec<String>, tokio::task::JoinHandle<()>)> = None;
    let mut retry_at = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(FOLLOW_EVERY);
    loop {
        tick.tick().await;
        let want = wanted_origins(
            pf_host_config::seat_defaults::of_box(),
            pf_host_config::config(),
        )
        .filter(|origins| is_confined(base.require_pairing, origins));
        if let Some((origins, task)) = &up {
            if !task.is_finished() && want.as_ref() == Some(origins) {
                continue;
            }
        }
        if let Some((_, task)) = up.take() {
            if task.is_finished() {
                retry_at = tokio::time::Instant::now() + REBIND_AFTER;
            } else {
                tracing::info!(bind = %base.bind, "seat browser plane closed: the box's browser streaming changed");
            }
            task.abort();
            // Awaited before the withdraw: a task still running could publish after it.
            let _ = task.await;
            withdraw();
        }
        let Some(origins) = want else { continue };
        if tokio::time::Instant::now() < retry_at {
            continue;
        }
        let plane = Plane {
            origins: origins.clone(),
            ..base.clone()
        };
        let (host, sem, bind) = (host.clone(), sem.clone(), plane.bind);
        let task = tokio::spawn(async move {
            if let Err(e) = serve(plane, host, sem).await {
                tracing::warn!(%bind, error = %e, "seat browser plane did not bind");
            }
        });
        up = Some((origins, task));
    }
}

/// The origins a seat's plane serves, or `None` while it should be closed: the box's Seat
/// defaults when it wrote them, else the seat host's own switch.
fn wanted_origins(
    defaults: Option<pf_host_config::seat_defaults::SeatDefaults>,
    own: &pf_host_config::HostConfig,
) -> Option<Vec<String>> {
    match defaults {
        Some(d) => d.browser.then_some(d.browser_origins),
        None => own.webtransport.then(|| own.webtransport_origins.clone()),
    }
}

/// Sessions one peer address may hold before admission.
const UNADMITTED_PER_ADDR: usize = 8;
/// Sessions the plane holds before admission, from every address together.
const UNADMITTED_TOTAL: usize = 64;

/// Sessions accepted and not admitted yet, by peer address. A slot is held from accept until the
/// session is admitted, parks a knock (the pending caps bound those) or ends.
#[derive(Default)]
struct Unadmitted(std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, usize>>);

/// One session's place in [`Unadmitted`], given back when dropped.
pub(crate) struct UnadmittedSlot {
    pool: Arc<Unadmitted>,
    ip: std::net::IpAddr,
}

impl Unadmitted {
    /// A slot for a session from `ip`, or `None` when that address or the plane is full.
    fn take(self: &Arc<Self>, ip: std::net::IpAddr) -> Option<UnadmittedSlot> {
        let ip = ip.to_canonical();
        let mut held = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let mine = held.get(&ip).copied().unwrap_or(0);
        if mine >= UNADMITTED_PER_ADDR || held.values().sum::<usize>() >= UNADMITTED_TOTAL {
            return None;
        }
        held.insert(ip, mine + 1);
        Some(UnadmittedSlot {
            pool: self.clone(),
            ip,
        })
    }
}

impl Drop for UnadmittedSlot {
    fn drop(&mut self) {
        let mut held = self.pool.0.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = held.get_mut(&self.ip) {
            *n -= 1;
            if *n == 0 {
                held.remove(&self.ip);
            }
        }
    }
}

async fn accept_loop(
    endpoint: &Endpoint<wtransport::endpoint::endpoint_side::Server>,
    serving: Arc<Serving>,
    sem: Arc<tokio::sync::Semaphore>,
) {
    let unadmitted = Arc::new(Unadmitted::default());
    loop {
        let incoming = endpoint.accept().await;
        // As on the native plane: a source that has not shown it receives at its address gets a
        // Retry, not a task, so a spoofed-source flood cannot make this host amplify.
        if !incoming.remote_address_validated() {
            incoming.retry();
            continue;
        }
        let Some(slot) = unadmitted.take(incoming.remote_address().ip()) else {
            tracing::debug!(peer = %incoming.remote_address(), "WebTransport session refused: too many not admitted yet");
            incoming.refuse();
            continue;
        };
        let serving = serving.clone();
        let sem = sem.clone();
        tokio::spawn(async move {
            if let Err(e) = session(incoming, &serving, sem, slot).await {
                tracing::debug!(error = %e, "WebTransport session ended");
            }
        });
    }
}

/// The page server and page a session came through. A page that is public and that nothing on
/// this host knows makes the session's knocks strict.
fn via(
    relay: crate::native_pairing::Relay,
    origin: Option<String>,
    serving: &Serving,
) -> crate::native_pairing::Via {
    let unfamiliar_origin = origin.as_deref().is_some_and(|o| {
        !local_origin(o)
            && !serving.plane.origins.iter().any(|listed| listed == o)
            && !serving.plane.pairing.knows_origin(o)
    });
    crate::native_pairing::Via {
        relay,
        origin,
        unfamiliar_origin,
    }
}

/// An origin no public website has: a local file (a packaged TV app sends `file://`), or a host
/// that is a private address or a name no public zone holds. `null`, which any page can send from
/// a sandboxed frame, is not one.
fn local_origin(origin: &str) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    if scheme.eq_ignore_ascii_case("file") {
        return true;
    }
    let host = match rest.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => rest.split(':').next().unwrap_or_default(),
    };
    match host.parse() {
        Ok(ip) => {
            crate::native_pairing::classify_source(Some(ip))
                == crate::native_pairing::KnockSource::Lan
        }
        Err(_) => punktfunk_core::provenance::is_local_name(host),
    }
}

/// Is this page allowed to open a session?
///
/// WebTransport is **not** subject to CORS, so without this any page the user happens to have
/// open can reach the plane — the browser will not stop it. An empty allowlist means "anything",
/// which is what a host with no configured origins has to mean until the console can offer the
/// choice; the log line below is what tells an operator which value to set.
///
/// A session with no `Origin` passes whatever the list says. A browser stamps the header on
/// every page's dial and no page can strip it, so the list still stops every hostile page; what
/// omits it is not a page — a packaged TV app, a native tool — and a client like that could
/// send any origin it chose, so refusing it keeps out nobody. Pairing is what admits a device.
fn origin_allowed(origin: Option<&str>, allowed: &[String]) -> bool {
    match origin {
        None => true,
        Some(o) => allowed.is_empty() || allowed.iter().any(|a| a == o),
    }
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

/// One browser session: check the origin, then hand the connection to [`session::run`], which
/// takes the session slot. `unadmitted` is held until then. A debug build keeps Phase 1's `/echo`
/// for the measurement pages.
async fn session(
    incoming: wtransport::endpoint::IncomingSession,
    serving: &Arc<Serving>,
    sem: Arc<tokio::sync::Semaphore>,
    unadmitted: UnadmittedSlot,
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
    // A page server marks what it relays. Present at all, the session is strict.
    let relay = crate::native_pairing::Relay::from_header(
        request
            .headers()
            .get(punktfunk_core::provenance::CLIENT_ADDR_HEADER)
            .map(String::as_str),
    );
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

    // Phase 1's echo answers anyone with no slot and no timeout, so a release host has no such path.
    #[cfg(debug_assertions)]
    if path == "/echo" {
        return echo(connection).await;
    }
    // The management API for a page that cannot `fetch` the host: no streaming slot, no
    // handshake clock, its own caps (`mgmt`).
    if path == "/mgmt" {
        drop(unadmitted);
        return mgmt::serve(connection, serving).await;
    }

    // An older page dials `/stream` on `punktfunk/1`. It reads a refusal in that framing.
    if path != "/pf2" {
        let reason = punktfunk_core::reject::RejectReason::WireVersionMismatch;
        refuse_older_page(&connection, reason.close_code(), OLDER_PAGE).await;
        connection.close(
            wtransport::VarInt::from_u32(reason.close_code()),
            OLDER_PAGE.as_bytes(),
        );
        tracing::info!(path = %path, "punktfunk/1 browser refused — the page needs an update");
        return Ok(());
    }
    let peer = connection.remote_address();
    let via = Arc::new(via(relay, origin, serving));
    match session::run(connection.clone(), serving.clone(), sem, via, unadmitted).await {
        Ok(crate::native::Served::Session) => tracing::info!(%peer, "browser session complete"),
        Ok(
            crate::native::Served::ProbeClose
            | crate::native::Served::Management
            | crate::native::Served::Access,
        ) => {}
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

/// What a page that dials `punktfunk/1` is told.
const OLDER_PAGE: &str =
    "This page is older than the host. Reload it or update the app, then try again.";

/// Say why on a fresh unidirectional stream, then give the browser a moment to read it. The
/// close that follows carries no retransmit, so the wait is what makes the message arrive. The
/// stream carries the `Refused` frame alone.
pub(crate) async fn refuse(connection: &wtransport::Connection, code: u32, reason: &str) {
    use punktfunk_core::quic::v2::msg::V2Message;
    let refused = punktfunk_core::quic::Refused {
        code,
        reason: reason.to_string(),
    };
    say_then_wait(connection, &refused.encode_v2()).await;
}

/// [`refuse`] in `punktfunk/1`'s framing, the only refusal a page from before `/pf2` reads.
async fn refuse_older_page(connection: &wtransport::Connection, code: u32, reason: &str) {
    let refused = punktfunk_core::quic::Refused {
        code,
        reason: reason.to_string(),
    };
    say_then_wait(
        connection,
        &punktfunk_core::quic::pkf1::frame(&refused.encode_pkf1()),
    )
    .await;
}

async fn say_then_wait(connection: &wtransport::Connection, bytes: &[u8]) {
    let sent = async {
        let mut uni = connection.open_uni().await?.await?;
        tokio::io::AsyncWriteExt::write_all(&mut uni, bytes).await?;
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

/// Phase 1's echo, at `/echo` in a debug build: the page connects, sends datagrams and control
/// bytes, and gets them back. It is what the seam's measurements run against.
#[cfg(debug_assertions)]
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
    /// The box's Seat defaults decide a seat's plane over its own switch; a seat host the box
    /// wrote none for keeps its own.
    #[test]
    fn a_seat_s_plane_follows_the_box_over_its_own_switch() {
        let box_says = |browser: bool| pf_host_config::seat_defaults::SeatDefaults {
            end_on_game_exit: true,
            max_mode: None,
            library_writable: true,
            dualsense_usbip: false,
            browser,
            browser_origins: vec!["https://box:47992".into()],
            background_input: true,
        };
        let own = |on: bool| pf_host_config::HostConfig {
            webtransport: on,
            webtransport_origins: vec!["https://own".into()],
            ..Default::default()
        };
        let boxed = Some(vec!["https://box:47992".to_string()]);
        assert_eq!(wanted_origins(Some(box_says(true)), &own(false)), boxed);
        assert_eq!(wanted_origins(Some(box_says(false)), &own(true)), None);
        assert_eq!(
            wanted_origins(None, &own(true)),
            Some(vec!["https://own".into()])
        );
        assert_eq!(wanted_origins(None, &own(false)), None);
    }

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
        // No Origin is not a page: a packaged TV app dials without one, and a browser cannot.
        // The list is a rule for pages, so it does not apply.
        assert!(origin_allowed(None, &allowed));
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

    /// Eight sessions from one address wait for admission; the ninth is refused while another
    /// address still gets in, and a slot given back is free again. Sixty-four in all.
    #[test]
    fn unadmitted_sessions_are_capped_per_address_and_in_all() {
        let pool = Arc::new(Unadmitted::default());
        let one: std::net::IpAddr = "192.168.1.20".parse().unwrap();
        let other: std::net::IpAddr = "192.168.1.21".parse().unwrap();
        let mut held: Vec<_> = (0..UNADMITTED_PER_ADDR)
            .map(|_| pool.take(one).expect("under the per-address cap"))
            .collect();
        assert!(pool.take(one).is_none(), "the ninth from one address");
        let mapped: std::net::IpAddr = "::ffff:192.168.1.20".parse().unwrap();
        assert!(pool.take(mapped).is_none(), "the same address, v4-mapped");
        assert!(pool.take(other).is_some(), "another address still connects");
        held.pop();
        assert!(pool.take(one).is_some(), "a slot given back is free again");

        let pool = Arc::new(Unadmitted::default());
        let _all: Vec<_> = (0..UNADMITTED_TOTAL)
            .map(|i| pool.take(std::net::Ipv4Addr::new(10, 0, 0, i as u8).into()))
            .collect::<Option<_>>()
            .expect("under the total cap");
        assert!(pool.take(other).is_none(), "the plane is full");
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
        use punktfunk_core::tls::{cert_fingerprint, verify_plane_attestation};
        let leaf = |ident: &crate::identity::NativeIdentity| {
            let (_, pem) = x509_parser::pem::parse_x509_pem(ident.cert_pem.as_bytes()).unwrap();
            pem.contents
        };
        let ident = crate::identity::ephemeral().expect("mint a native identity");
        let cert_hash = "a".repeat(64);
        let a = attest(&ident, &cert_hash).expect("P-256 identity attests");
        let der = base64::engine::general_purpose::STANDARD
            .decode(&a.host_cert_der)
            .expect("host_cert_der is base64");
        let sig = hex::decode(&a.cert_hash_sig).expect("cert_hash_sig is hex");

        // The fingerprint that comes back is the one the browser pinned at pairing.
        assert_eq!(
            verify_plane_attestation(&cert_hash, &sig, &der).expect("attestation verifies"),
            cert_fingerprint(&leaf(&ident)),
        );
        assert!(
            verify_plane_attestation(&"b".repeat(64), &sig, &der).is_err(),
            "a different hash must not verify"
        );
        let other = crate::identity::ephemeral().expect("mint a second identity");
        assert!(
            verify_plane_attestation(&cert_hash, &sig, &leaf(&other)).is_err(),
            "another host's key must not verify"
        );
        // The same key over the bare hash: the context is part of the message.
        let key = rcgen::KeyPair::from_pem(&ident.key_pem).unwrap();
        let bare = rcgen::SigningKey::sign(&key, cert_hash.as_bytes()).unwrap();
        assert!(verify_plane_attestation(&cert_hash, &bare, &der).is_err());
    }
}
