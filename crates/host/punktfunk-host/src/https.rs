//! TLS for the HTTPS nvhttp port (47984) and the management API.
//!
//! Moonlight does mutual TLS: it presents a client cert and expects the
//! server to request one. A server-auth-only config makes post-pairing
//! `pairchallenge` fail. This config requests the cert and verifies the
//! client owns its key, then accepts any well-formed cert at the handshake
//! — pairing is the identity proof. Authorization is per-request:
//! [`serve_https`] attaches [`PeerCertFingerprint`]; nvhttp/mgmt handlers
//! reject unpinned callers (Apollo's post-handshake `get_verified_cert`).
//!
//! [`spki_p256_point`] and [`sha256`] are the device-key helpers the browser plane and the
//! management API share.

use anyhow::{Context, Result};
use axum::Router;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use std::net::SocketAddr;
use std::sync::Arc;

/// SHA-256 of the peer client cert (hex). `None` on plain HTTP or a certless
/// browser (bearer token). Handlers authorize against the paired store.
#[derive(Clone)]
pub(crate) struct PeerCertFingerprint(pub Option<String>);

/// TCP source of an HTTPS request. `/launch` records which paired client
/// owns the session so the unauthenticated RTSP/UDP media plane can bind
/// to that IP.
#[derive(Clone, Copy)]
pub(crate) struct PeerAddr(pub SocketAddr);

/// The host address a request arrived on. `/serverinfo` reports that interface's IP and MAC,
/// which Moonlight stores for Wake-on-LAN; the default route may be a VPN or another NIC.
#[derive(Clone, Copy)]
#[cfg_attr(not(feature = "gamestream"), allow(dead_code, reason = "compat plane"))]
pub(crate) struct LocalAddr(pub SocketAddr);

/// The plugin whose own pipe, and whose own AppContainer, a request came in through: the
/// connection is the credential. Absent on every socket.
#[derive(Clone, Debug)]
pub(crate) struct PipePlugin(pub String);

/// A listening socket nothing else on the box can bind beside.
///
/// Windows lets a second socket bind a specific address on a port a wildcard socket holds,
/// and the specific one then takes that traffic; the cross-account refusal is the only thing
/// in the way. `SO_EXCLUSIVEADDRUSE` closes it for every account. Unix keeps std's
/// `SO_REUSEADDR` so a restart does not wait out TIME_WAIT.
pub(crate) fn bind_exclusive(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows::Win32::Networking::WinSock::{
            setsockopt, SOCKET, SOL_SOCKET, SO_EXCLUSIVEADDRUSE,
        };
        let on = 1i32.to_ne_bytes();
        // SAFETY: `socket` is a live, unbound socket we own; `on` is a 4-byte int the option
        // expects, borrowed for the call only.
        let rc = unsafe {
            setsockopt(
                SOCKET(socket.as_raw_socket() as usize),
                SOL_SOCKET,
                SO_EXCLUSIVEADDRUSE,
                Some(&on),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    Ok(socket.into())
}

/// Caps on the HTTP(S) acceptors. Without them a LAN peer holding sockets
/// (incomplete TLS, idle connections) exhausts fds with no authentication.
/// 256 / 32 is generous: a console browser holds a handful, a paired client a few.
const MAX_CONNS: usize = 256;
const MAX_CONNS_PER_IP: usize = 32;
/// Slots a LAN peer never takes: two full per-IP buckets (127.0.0.1, ::1) stay free for the
/// console, `ctl`, the tray and the plugin runner.
const LOOPBACK_RESERVE: usize = 2 * MAX_CONNS_PER_IP;
/// A LAN handshake completes in milliseconds; a peer still negotiating after
/// this is holding a slot, not pairing.
const TLS_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Bounds a connection's first request head on any protocol, every later HTTP/1 head
/// (slowloris), and the wait for each frame of a request body. Response streaming is unaffected.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// A quiet HTTP/2 peer is pinged after this, and dropped when the ack misses the same window.
const H2_PING: std::time::Duration = std::time::Duration::from_secs(30);
/// Requests in flight on one HTTP/2 connection. A client past it queues on that connection
/// rather than opening another, so a library's worth of covers stays one of `MAX_CONNS_PER_IP`.
const MAX_H2_STREAMS: u32 = 32;

/// Decrements the per-IP live count on drop so every exit path (handshake
/// failure, served connection, cancellation) releases the ceiling.
struct IpGuard(
    Arc<std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, usize>>>,
    std::net::IpAddr,
);

impl Drop for IpGuard {
    fn drop(&mut self) {
        let mut m = self.0.lock().unwrap();
        if let Some(n) = m.get_mut(&self.1) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.1);
            }
        }
    }
}

/// The first byte of a TLS record: a ClientHello opens with it, and no HTTP method does.
const TLS_HANDSHAKE: u8 = 0x16;

/// HTTPS server that surfaces the verified client cert to handlers.
/// `axum_server` cannot expose the peer cert, so this runs the rustls
/// handshake (tokio-rustls) and attaches [`PeerCertFingerprint`] on every
/// request. The nvhttp HTTPS listener; the management API takes [`serve_https_with_plain`].
#[cfg_attr(not(feature = "gamestream"), allow(dead_code, reason = "compat plane"))]
pub(crate) async fn serve_https(
    bind: SocketAddr,
    app: Router,
    tls: Arc<ServerConfig>,
) -> Result<()> {
    serve_governed(bind, app, Some(tls), None).await
}

/// [`serve_https`], and a peer that speaks plain HTTP on the same port is answered by `plain`
/// instead: the one route a packaged page reads before it can dial the browser plane
/// (`mgmt::bootstrap_app`). The first byte tells the two apart, and nothing else changes —
/// a TLS peer never sees the sniff, and `None` drops plaintext as [`serve_https`] does.
pub(crate) async fn serve_https_with_plain(
    bind: SocketAddr,
    app: Router,
    tls: Arc<ServerConfig>,
    plain: Option<Router>,
) -> Result<()> {
    serve_governed(bind, app, Some(tls), plain).await
}

/// Same acceptor without TLS — the plain nvhttp listener (47989). Pre-auth
/// by protocol; still needs the connection ceilings.
#[cfg_attr(not(feature = "gamestream"), allow(dead_code, reason = "compat plane"))]
pub(crate) async fn serve_plain(bind: SocketAddr, app: Router) -> Result<()> {
    serve_governed(bind, app, None, None).await
}

async fn serve_governed(
    bind: SocketAddr,
    app: Router,
    tls: Option<Arc<ServerConfig>>,
    plain: Option<Router>,
) -> Result<()> {
    let listener = bind_exclusive(bind)
        .and_then(|l| {
            l.set_nonblocking(true)?;
            tokio::net::TcpListener::from_std(l)
        })
        .with_context(|| format!("bind HTTP(S) {bind}"))?;
    serve_listener(listener, app, tls, plain).await
}

/// The accept loop on a bound socket. Split from the bind so a test serves a port it chose.
async fn serve_listener(
    listener: tokio::net::TcpListener,
    app: Router,
    tls: Option<Arc<ServerConfig>>,
    plain: Option<Router>,
) -> Result<()> {
    let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
    let conns = Arc::new(tokio::sync::Semaphore::new(MAX_CONNS));
    let per_ip: Arc<std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, usize>>> =
        Arc::default();
    loop {
        let (tcp, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                // A persistent accept() error (fd exhaustion / EMFILE) would otherwise hot-spin
                // this loop and storm the log; back off so a stuck accept can't burn a core.
                tracing::warn!(error = %e, "HTTP(S) accept failed — backing off 100ms");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        // Drop rather than queue when a ceiling is hit: a well-behaved client retries,
        // and queuing is the unbounded buildup the ceiling exists to prevent.
        // Both guards travel into the task so every exit path releases them.
        let floor = if peer.ip().to_canonical().is_loopback() {
            0
        } else {
            LOOPBACK_RESERVE
        };
        let permit = match conns.clone().try_acquire_owned() {
            Ok(permit) if conns.available_permits() >= floor => permit,
            _ => {
                tracing::warn!(%peer, "HTTP(S) connection ceiling reached — dropping new connection");
                continue;
            }
        };
        let ip_guard = {
            let mut m = per_ip.lock().unwrap();
            let n = m.entry(peer.ip()).or_insert(0);
            if *n >= MAX_CONNS_PER_IP {
                tracing::warn!(%peer, "per-IP connection ceiling reached — dropping new connection");
                continue;
            }
            *n += 1;
            IpGuard(per_ip.clone(), peer.ip())
        };
        let acceptor = acceptor.clone();
        let app = app.clone();
        let plain = plain.clone();
        let local = tcp.local_addr().ok().map(LocalAddr);
        tokio::spawn(async move {
            let _permit = permit;
            let _ip_guard = ip_guard;
            match &acceptor {
                Some(acceptor) => {
                    // One byte peeked and left in the socket, so the handshake that follows
                    // still reads it as its own first byte.
                    if let Some(plain) = plain {
                        let mut first = [0u8; 1];
                        let peeked =
                            tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, tcp.peek(&mut first)).await;
                        match peeked {
                            Ok(Ok(1)) if first[0] != TLS_HANDSHAKE => {
                                serve_conn(
                                    tcp,
                                    plain,
                                    PeerCertFingerprint(None),
                                    PeerAddr(peer),
                                    local,
                                    None,
                                )
                                .await;
                                return;
                            }
                            Ok(Ok(1)) => {}
                            // Closed before a byte, or dawdling: no slot for either.
                            _ => return,
                        }
                    }
                    let tls_stream =
                        match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(tcp))
                            .await
                        {
                            Ok(Ok(s)) => s,
                            // Failed or dawdling handshake is routine (port scan, browser
                            // bailing on the self-signed cert) — not fatal.
                            _ => return,
                        };
                    // Verifier accepts any well-formed cert; handlers authorize by fingerprint.
                    let fp = tls_stream
                        .get_ref()
                        .1
                        .peer_certificates()
                        .and_then(|c| c.first())
                        .map(|c| {
                            hex::encode(punktfunk_core::quic::endpoint::cert_fingerprint(
                                c.as_ref(),
                            ))
                        });
                    serve_conn(
                        tls_stream,
                        app,
                        PeerCertFingerprint(fp),
                        PeerAddr(peer),
                        local,
                        None,
                    )
                    .await;
                }
                None => {
                    serve_conn(
                        tcp,
                        app,
                        PeerCertFingerprint(None),
                        PeerAddr(peer),
                        local,
                        None,
                    )
                    .await
                }
            }
        });
    }
}

fn connection_builder() -> hyper_util::server::conn::auto::Builder<hyper_util::rt::TokioExecutor> {
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    builder
        .http2()
        .timer(hyper_util::rt::TokioTimer::new())
        .max_concurrent_streams(MAX_H2_STREAMS)
        .keep_alive_interval(H2_PING)
        .keep_alive_timeout(H2_PING);
    builder
}

/// A request body that fails once its peer has sent nothing for `quiet`. The handler's read
/// errors, and an HTTP/1 connection closes behind the answer: a stalled upload holds no slot.
struct QuietBody {
    inner: hyper::body::Incoming,
    quiet: std::time::Duration,
    /// Armed while a read waits on the peer.
    waiting: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl hyper::body::Body for QuietBody {
    type Data = hyper::body::Bytes;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        use std::future::Future;
        use std::task::Poll;
        let this = &mut *self;
        if let Poll::Ready(frame) = std::pin::Pin::new(&mut this.inner).poll_frame(cx) {
            this.waiting = None;
            return Poll::Ready(frame.map(|f| f.map_err(Into::into)));
        }
        let quiet = this.quiet;
        let waiting = this
            .waiting
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(quiet)));
        match waiting.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Some(Err("request body stalled".into()))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// Drive one accepted stream — a socket, a TLS stream or a pipe — through `app`, with the
/// peer facts the handlers read as request extensions. A peer that has not sent a whole
/// request head within [`HEADER_READ_TIMEOUT`], or goes that long without a frame of a
/// request body, is dropped.
pub(crate) async fn serve_conn<S>(
    stream: S,
    app: Router,
    fp: PeerCertFingerprint,
    addr: PeerAddr,
    local: Option<LocalAddr>,
    pipe_plugin: Option<PipePlugin>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    serve_conn_within(
        HEADER_READ_TIMEOUT,
        stream,
        app,
        fp,
        addr,
        local,
        pipe_plugin,
    )
    .await
}

/// [`serve_conn`] with its deadline as an argument: `quiet` bounds the first request head and
/// each wait for a request body frame. Version detection, a silent HTTP/2 peer and a body
/// have no timer of hyper's own; later heads and pings are under hyper's timers.
async fn serve_conn_within<S>(
    quiet: std::time::Duration,
    stream: S,
    app: Router,
    fp: PeerCertFingerprint,
    addr: PeerAddr,
    local: Option<LocalAddr>,
    pipe_plugin: Option<PipePlugin>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tower::ServiceExt;
    let arrived = Arc::new(tokio::sync::Notify::new());
    let seen = arrived.clone();
    let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        seen.notify_one();
        let app = app.clone();
        let fp = fp.clone();
        let pipe_plugin = pipe_plugin.clone();
        async move {
            let mut req = req.map(|inner| {
                axum::body::Body::new(QuietBody {
                    inner,
                    quiet,
                    waiting: None,
                })
            });
            req.extensions_mut().insert(fp);
            req.extensions_mut().insert(addr);
            if let Some(local) = local {
                req.extensions_mut().insert(local);
            }
            if let Some(plugin) = pipe_plugin {
                req.extensions_mut().insert(plugin);
            }
            app.oneshot(req).await
        }
    });
    let io = hyper_util::rt::TokioIo::new(stream);
    let builder = connection_builder();
    let conn = builder.serve_connection_with_upgrades(io, svc);
    tokio::pin!(conn);
    tokio::select! {
        _ = &mut conn => return,
        _ = arrived.notified() => {}
        _ = tokio::time::sleep(quiet) => return,
    }
    let _ = conn.await;
}

#[cfg(test)]
mod governed_tests {
    use super::*;
    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A client that offers `h2` gets it, and more requests than the per-IP connection ceiling
    /// all ride its one connection, `MAX_H2_STREAMS` at a time.
    #[tokio::test]
    async fn the_management_api_multiplexes_a_burst_over_http2() {
        punktfunk_core::tls::install_default_provider();
        let id = crate::identity::ephemeral().unwrap();
        let server_tls = server_config_optional_client(&id.cert_pem, &id.key_pem).unwrap();
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let app = Router::new().route("/", get(|| async { "ok" }));
        tokio::spawn(async move {
            let tls = tokio_rustls::TlsAcceptor::from(server_tls)
                .accept(server_io)
                .await
                .unwrap();
            let peer = PeerAddr("127.0.0.1:1".parse().unwrap());
            serve_conn(tls, app, PeerCertFingerprint(None), peer, None, None).await;
        });

        let mut client = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(punktfunk_core::tls::PinVerify::new(None)))
            .with_no_client_auth();
        client.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let tls = tokio_rustls::TlsConnector::from(Arc::new(client))
            .connect("localhost".try_into().unwrap(), client_io)
            .await
            .unwrap();
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));

        let (h2, connection) = h2::client::handshake(tls).await.unwrap();
        tokio::spawn(connection);
        let mut replies = tokio::task::JoinSet::new();
        for _ in 0..MAX_CONNS_PER_IP * 3 {
            let mut ready = h2.clone().ready().await.unwrap();
            let request = axum::http::Request::get("https://localhost/")
                .body(())
                .unwrap();
            let (response, _) = ready.send_request(request, true).unwrap();
            replies.spawn(async move { response.await.unwrap().status() });
        }
        let mut ok = 0;
        while let Some(status) = replies.join_next().await {
            assert_eq!(status.unwrap(), axum::http::StatusCode::OK);
            ok += 1;
        }
        assert_eq!(ok, MAX_CONNS_PER_IP * 3);
        assert_eq!(h2.current_max_send_streams(), MAX_H2_STREAMS as usize);
    }

    /// A GET on the TLS port in plain text reaches the plain router, and only where one is
    /// offered; a TLS peer on the same socket is served as before. The nvhttp listener offers
    /// none, so the sniff is the management port's alone.
    #[tokio::test]
    async fn plain_http_on_the_tls_port_reaches_the_plain_router_only_where_offered() {
        punktfunk_core::tls::install_default_provider();
        let id = crate::identity::ephemeral().unwrap();
        let server_tls = server_config_optional_client(&id.cert_pem, &id.key_pem).unwrap();
        let secure = Router::new().route("/", get(|| async { "over tls" }));
        let plain = Router::new()
            .route("/api/v1/webtransport", get(|| async { "the plane" }))
            .fallback(|| async { (axum::http::StatusCode::NOT_FOUND, "no") });
        let bind = |plain: Option<Router>, tls: Arc<ServerConfig>| {
            let secure = secure.clone();
            async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                tokio::spawn(serve_listener(listener, secure, Some(tls), plain));
                addr
            }
        };
        let http = |addr: SocketAddr, path: &'static str| async move {
            let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            tcp.write_all(
                format!("GET {path} HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
            let mut out = Vec::new();
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(5), tcp.read_to_end(&mut out))
                    .await;
            String::from_utf8_lossy(&out).into_owned()
        };

        let offered = bind(Some(plain), server_tls.clone()).await;
        let answer = http(offered, "/api/v1/webtransport").await;
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.ends_with("the plane"), "{answer}");
        let other = http(offered, "/").await;
        assert!(other.starts_with("HTTP/1.1 404"), "{other}");

        let mut client = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(punktfunk_core::tls::PinVerify::new(None)))
            .with_no_client_auth();
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tcp = tokio::net::TcpStream::connect(offered).await.unwrap();
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(client))
            .connect("localhost".try_into().unwrap(), tcp)
            .await
            .unwrap();
        tls.write_all(b"GET / HTTP/1.1\r\nHost: h\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        let _ = tls.read_to_end(&mut out).await;
        let over_tls = String::from_utf8_lossy(&out);
        assert!(over_tls.starts_with("HTTP/1.1 200"), "{over_tls}");
        assert!(over_tls.ends_with("over tls"), "{over_tls}");

        // No plain router: the TLS acceptor meets the GET and answers with its alert, as it
        // always has. Nothing HTTP comes back.
        let unoffered = bind(None, server_tls).await;
        let refused = http(unoffered, "/api/v1/webtransport").await;
        assert!(!refused.starts_with("HTTP/"), "{refused:?}");
    }

    #[tokio::test]
    async fn configured_header_timeout_has_a_timer() {
        let (mut client, server) = tokio::io::duplex(4096);
        let app = Router::new().route("/", get(|| async { "ok" }));
        let served = tokio::spawn(serve_conn(
            server,
            app,
            PeerCertFingerprint(None),
            PeerAddr("127.0.0.1:1234".parse().unwrap()),
            None,
            None,
        ));
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        served.await.unwrap();
        assert!(String::from_utf8_lossy(&response).contains("200 OK"));
    }

    /// A peer that never sends a request is dropped at the deadline; an answer that takes
    /// longer than the deadline still arrives.
    #[tokio::test]
    async fn a_silent_connection_is_dropped_but_a_slow_answer_is_not() {
        let limit = std::time::Duration::from_millis(200);
        let app = Router::new().route(
            "/",
            get(move || async move {
                tokio::time::sleep(limit * 3).await;
                "late"
            }),
        );
        let serve = |io| {
            let peer = PeerAddr("127.0.0.1:1".parse().unwrap());
            tokio::spawn(serve_conn_within(
                limit,
                io,
                app.clone(),
                PeerCertFingerprint(None),
                peer,
                None,
                None,
            ))
        };

        let (mut silent, server) = tokio::io::duplex(4096);
        let served = serve(server);
        let mut out = Vec::new();
        tokio::time::timeout(limit * 5, silent.read_to_end(&mut out))
            .await
            .expect("a silent peer is dropped")
            .unwrap();
        assert!(out.is_empty());
        served.await.unwrap();

        let (mut client, server) = tokio::io::duplex(4096);
        let _served = serve(server);
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).ends_with("late"));
    }

    /// A request body that stops arriving is refused and its connection closed; one that keeps
    /// arriving, each frame inside the deadline, is read whole however long it takes.
    #[tokio::test]
    async fn a_stalled_request_body_frees_its_connection() {
        let limit = std::time::Duration::from_millis(200);
        let app = Router::new().route(
            "/",
            axum::routing::post(|body: axum::body::Bytes| async move { body.len().to_string() }),
        );
        let serve = |io| {
            let peer = PeerAddr("127.0.0.1:1".parse().unwrap());
            tokio::spawn(serve_conn_within(
                limit,
                io,
                app.clone(),
                PeerCertFingerprint(None),
                peer,
                None,
                None,
            ))
        };
        let head = b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4\r\n\r\n";

        let (mut stalled, server) = tokio::io::duplex(4096);
        let served = serve(server);
        stalled.write_all(head).await.unwrap();
        stalled.write_all(b"a").await.unwrap();
        let mut out = Vec::new();
        tokio::time::timeout(limit * 5, stalled.read_to_end(&mut out))
            .await
            .expect("a stalled body ends the connection")
            .unwrap();
        let answer = String::from_utf8_lossy(&out);
        assert!(answer.starts_with("HTTP/1.1 4"), "{answer}");
        served.await.unwrap();

        let (mut slow, server) = tokio::io::duplex(4096);
        let _served = serve(server);
        slow.write_all(head).await.unwrap();
        for byte in [b"a", b"b", b"c", b"d"] {
            tokio::time::sleep(limit / 2).await;
            slow.write_all(byte).await.unwrap();
        }
        let mut out = vec![0u8; 4096];
        let n = slow.read(&mut out).await.unwrap();
        let answer = String::from_utf8_lossy(&out[..n]);
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        assert!(answer.ends_with('4'), "{answer}");
    }
}

/// Requests the client cert and verifies its `CertificateVerify` signature,
/// but does not judge the certificate. Authorization is after the handshake
/// against the pinned allow-list (`nvhttp::peer_is_paired`).
///
/// Pinning here is not expressible: the handshake finishes before the
/// request line, and the protocol fixes the ports, so post-pair routes
/// cannot use a different listener. `/serverinfo` on 47984 must answer
/// unpaired peers (`PairStatus=0`) or pairing has no entry point. The
/// management API shares this verifier and admits certless browsers
/// (`mandatory: false`) that authenticate by bearer token.
///
/// A peer that reaches a handler has proved possession of the presented
/// key (webpki, or [`accept_legacy_moonlight_cert`] for pre-v3 RSA).
/// `peer_is_paired` then pins that cert's SHA-256. Skipping the signature
/// check would let anyone replay a paired client's public certificate.
#[derive(Debug)]
struct AcceptAnyClientCert {
    provider: Arc<CryptoProvider>,
    /// nvhttp/pairing requires the client cert; the mgmt API requests it but
    /// lets a certless peer (browser + bearer token) through.
    mandatory: bool,
}

impl ClientCertVerifier for AcceptAnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        self.mandatory
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer,
        _intermediates: &[CertificateDer],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let verdict = verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        );
        // Moonlight-client-cert leniency only when the compat planes are on;
        // native clients present webpki-clean certs and never need it.
        #[cfg(feature = "gamestream")]
        let verdict = verdict.or_else(|e| accept_legacy_moonlight_cert(message, cert, dss, e));
        verdict
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        let verdict = verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        );
        // Moonlight-client-cert leniency only when the compat planes are on;
        // native clients present webpki-clean certs and never need it.
        #[cfg(feature = "gamestream")]
        let verdict = verdict.or_else(|e| accept_legacy_moonlight_cert(message, cert, dss, e));
        verdict
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Fallback `CertificateVerify` for pre-v3 (X.509 v2) certificates, used
/// only after rustls-webpki has already rejected the cert.
///
/// rustls-webpki 0.103 accepts only X.509 v3 and returns
/// `UnsupportedCertVersion` before looking at the signature.
/// moonlight-embedded still mints self-signed v2 certs with no `keyUsage`,
/// so pairing fails here while Sunshine's OpenSSL callback never inspects
/// version. The version is the gap; a v3 cert with no extensions passes.
///
/// X.509 version and extensions carry no GameStream security weight: the
/// cert is self-signed and later pinned by SHA-256 (`nvhttp::peer_is_paired`).
/// The property that matters is possession of the private key. We re-run
/// that check with a version-agnostic parser (x509-parser + RSA), the same
/// primitive `pairing::verify256` uses. A bad signature still fails; any
/// non-RSA scheme returns `webpki_err`. Not a bypass.
#[cfg(feature = "gamestream")]
fn accept_legacy_moonlight_cert(
    message: &[u8],
    cert: &CertificateDer,
    dss: &DigitallySignedStruct,
    webpki_err: rustls::Error,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    use rsa::pkcs8::DecodePublicKey;
    use rsa::signature::Verifier;
    use rsa::{pkcs1v15, pss, RsaPublicKey};
    // `rsa`'s own re-export — these are `pkcs1v15`/`pss` type parameters on `rsa 0.9`
    // (`digest 0.10`), not the crate-wide `sha2 0.11`. See Cargo.toml.
    use rsa::sha2::{Sha256, Sha384, Sha512};

    let Ok((_, x509)) = x509_parser::parse_x509_certificate(cert.as_ref()) else {
        return Err(webpki_err);
    };
    let Ok(key) = RsaPublicKey::from_public_key_der(x509.public_key().raw) else {
        return Err(webpki_err); // not RSA — keep webpki's error
    };
    let sig = dss.signature();

    // `VerifyingKey`/`Signature` hash `message` internally; each arm moves
    // `key`, but exactly one arm runs.
    let ok = match dss.scheme {
        SignatureScheme::RSA_PKCS1_SHA256 => pkcs1v15::Signature::try_from(sig)
            .map(|s| {
                pkcs1v15::VerifyingKey::<Sha256>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        SignatureScheme::RSA_PKCS1_SHA384 => pkcs1v15::Signature::try_from(sig)
            .map(|s| {
                pkcs1v15::VerifyingKey::<Sha384>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        SignatureScheme::RSA_PKCS1_SHA512 => pkcs1v15::Signature::try_from(sig)
            .map(|s| {
                pkcs1v15::VerifyingKey::<Sha512>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        SignatureScheme::RSA_PSS_SHA256 => pss::Signature::try_from(sig)
            .map(|s| {
                pss::VerifyingKey::<Sha256>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        SignatureScheme::RSA_PSS_SHA384 => pss::Signature::try_from(sig)
            .map(|s| {
                pss::VerifyingKey::<Sha384>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        SignatureScheme::RSA_PSS_SHA512 => pss::Signature::try_from(sig)
            .map(|s| {
                pss::VerifyingKey::<Sha512>::new(key)
                    .verify(message, &s)
                    .is_ok()
            })
            .unwrap_or(false),
        _ => return Err(webpki_err),
    };

    if ok {
        tracing::debug!(
            "accepted a legacy (pre-v3) Moonlight client cert via version-agnostic RSA verify"
        );
        Ok(HandshakeSignatureValid::assertion())
    } else {
        Err(webpki_err)
    }
}

/// Mutual-TLS `ServerConfig` with the host cert/key. nvhttp/pairing:
/// client cert is mandatory.
#[cfg_attr(not(feature = "gamestream"), allow(dead_code, reason = "compat plane"))]
pub fn server_config(cert_pem: &str, key_pem: &str) -> Result<Arc<ServerConfig>> {
    build_server_config(cert_pem, key_pem, true)
}

/// Like [`server_config`] but the client cert is optional — a certless
/// peer (browser + bearer token) still completes the handshake. Mgmt
/// API: paired clients present a cert; everyone else falls back to the token.
///
/// Offers HTTP/2. Over HTTP/1.1 a library screen spends one connection per cover, and past
/// `MAX_CONNS_PER_IP` the acceptor drops the rest — the covers that never load.
pub fn server_config_optional_client(cert_pem: &str, key_pem: &str) -> Result<Arc<ServerConfig>> {
    let mut config = Arc::unwrap_or_clone(build_server_config(cert_pem, key_pem, false)?);
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

fn build_server_config(
    cert_pem: &str,
    key_pem: &str,
    mandatory: bool,
) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    // rustls-pki-types `PemObject` (same path as punktfunk-core/quic.rs) so
    // we don't pull the unmaintained `rustls-pemfile`.
    let certs = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse host cert PEM")?;
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes()).context("parse host key PEM")?;

    let verifier = Arc::new(AcceptAnyClientCert {
        provider: provider.clone(),
        mandatory,
    });
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("rustls protocol versions")?
        .with_client_cert_verifier(verifier)
        .with_single_cert(certs, key)
        .context("rustls server cert")?;
    Ok(Arc::new(config))
}

/// The 65-byte uncompressed point inside a P-256 SPKI.
///
/// Every P-256 SPKI starts with the same 26-byte header — the SEQUENCE, the two OIDs and the BIT
/// STRING tag are all fixed by the key type — so matching it whole both locates the point and
/// rejects any other key type, which is what we want: the verifier is P-256 only.
pub(crate) fn spki_p256_point(spki: &[u8]) -> Option<&[u8]> {
    const P256_SPKI_HEADER: [u8; 26] = [
        0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
        0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
    ];
    let (head, point) = spki.split_at_checked(P256_SPKI_HEADER.len())?;
    (head == P256_SPKI_HEADER && point.len() == 65 && point[0] == 0x04).then_some(point)
}

/// SHA-256 of `bytes`: a certificate DER, a device-key SPKI or a PEM body. The browser plane and
/// the management API's device lane key a device by this one digest.
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{KeyPair, PublicKeyData as _, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384};

    /// A P-256 SPKI is a fixed shape, so locating the point is exact rather than a guess — and
    /// anything that is not one has to be refused, not misread.
    #[test]
    fn only_a_p256_spki_yields_a_key() {
        let p256 = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let spki = p256.subject_public_key_info();
        let point = spki_p256_point(&spki).expect("a P-256 SPKI has a point");
        assert_eq!(point.len(), 65);
        assert_eq!(point[0], 0x04, "uncompressed");

        let p384 = KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).unwrap();
        assert!(spki_p256_point(&p384.subject_public_key_info()).is_none());
        assert!(spki_p256_point(&[]).is_none());
        assert!(
            spki_p256_point(&spki[..spki.len() - 1]).is_none(),
            "truncated"
        );
        let mut trailing = spki.clone();
        trailing.push(0);
        assert!(spki_p256_point(&trailing).is_none(), "over-long");
    }
}
