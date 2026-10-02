//! The management API for a page that cannot `fetch` the host: `/mgmt` on the browser plane.
//!
//! A packaged TV app runs from `file://`, and nothing there completes a TLS connection to a
//! self-signed certificate — not `fetch`, not an `<img>`, and nobody clicks through a warning on
//! a TV. The plane it CAN reach, pinned by hash, so the API rides the plane: one HTTP request
//! per bidirectional stream, dispatched into the router the HTTPS listener serves.
//!
//! The same router *instance*, not a second `app()`: each one mints its own `DeviceAuth`, and a
//! token earned over HTTPS would be refused here. Every request carries the QUIC peer as
//! [`PeerAddr`], because the auth gate reads a request with none as loopback and loopback is
//! admin; and a [`PeerCertFingerprint`] of `None`, never one a peer could name. A tunnelled
//! request has exactly the authority a browser's `fetch` has: the public routes, the device
//! exchange, and the paired-device lane behind a token it earned.
//!
//! Framing is four bytes of big-endian length, a JSON head, then the body until FIN, both ways
//! ([`Head`], [`Reply`]). Nothing here parses HTTP, so there is no parser to smuggle past.

use super::Serving;
use crate::gamestream::tls::{LocalAddr, PeerAddr, PeerCertFingerprint};
use crate::mgmt::shared::api_error;
use anyhow::{Context, Result};
use axum::body::{Body, HttpBody as _};
use axum::http::{header, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::response::Response;
use axum::Router;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tower::ServiceExt as _;
use wtransport::{Connection, VarInt};

/// A request head: the JSON a page writes after the length prefix.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct Head {
    /// Method. `GET` or `POST`; the page sends no other.
    pub(crate) m: String,
    /// Path and query as `fetch` would send them: `/api/v1/library/page?limit=50`.
    pub(crate) p: String,
    /// Headers as pairs. Only [`PASSED`] reach the router.
    #[serde(default)]
    pub(crate) h: Vec<(String, String)>,
}

/// A response head: status and headers. The body follows until FIN.
#[derive(serde::Serialize)]
struct Reply {
    s: u16,
    h: Vec<(String, String)>,
}

/// A head is a bearer and a path; 16 KiB is a hundred of them.
const MAX_HEAD: usize = 16 * 1024;
/// A log upload is the largest body the paired lane takes, and the store caps it under this.
const MAX_BODY: usize = 1024 * 1024;
/// Requests in flight on one tunnel. HTTP/2 gives a page 32; a library's covers queue behind it.
const MAX_STREAMS: usize = 16;
/// Tunnels one address may hold open. A page needs one.
const MAX_SESSIONS_PER_IP: usize = 4;
/// A tunnel with nothing in flight for this long is closed; the page reopens on its next call.
const IDLE: Duration = Duration::from_secs(60);
/// A request must arrive whole within this, or its stream is answered 408 and released. Without
/// it a stream opened and left silent would hold a slot, and the idle close would never come.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The request headers that reach the router. `host` and `forwarded` never do: the router sees
/// this request as what it is, a LAN peer on the plane.
const PASSED: [HeaderName; 4] = [
    header::AUTHORIZATION,
    header::CONTENT_TYPE,
    header::ACCEPT,
    header::IF_NONE_MATCH,
];
/// Application close code for a tunnel refused before its first stream.
const REFUSED: u32 = 0x4d;

/// The router `mgmt::run` built, handed over before it serves. Unset, a tunnel is refused.
static ROUTER: OnceLock<Router> = OnceLock::new();

/// Hand the live management router to the plane. Once per process; a second call is ignored.
pub(crate) fn publish_router(app: Router) {
    let _ = ROUTER.set(app);
}

static SESSIONS: LazyLock<Mutex<HashMap<IpAddr, usize>>> = LazyLock::new(Default::default);

/// One address's share of [`MAX_SESSIONS_PER_IP`], released on drop.
struct IpSlot(IpAddr);

impl IpSlot {
    fn take(ip: IpAddr) -> Option<IpSlot> {
        let mut m = SESSIONS.lock().expect("tunnel slots");
        let n = m.entry(ip).or_insert(0);
        if *n >= MAX_SESSIONS_PER_IP {
            return None;
        }
        *n += 1;
        Some(IpSlot(ip))
    }
}

impl Drop for IpSlot {
    fn drop(&mut self) {
        let mut m = SESSIONS.lock().expect("tunnel slots");
        if let Some(n) = m.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                m.remove(&self.0);
            }
        }
    }
}

/// A `/mgmt` session on the plane: dispatch into the published router until the page goes.
pub(super) async fn serve(connection: Connection, serving: &Serving) -> Result<()> {
    let peer = connection.remote_address();
    let Some(router) = ROUTER.get() else {
        connection.close(
            VarInt::from_u32(REFUSED),
            b"the management API is not up yet",
        );
        return Ok(());
    };
    let Some(_slot) = IpSlot::take(peer.ip()) else {
        tracing::warn!(%peer, "management tunnel refused: too many from this address");
        connection.close(
            VarInt::from_u32(REFUSED),
            b"too many management tunnels from this address",
        );
        return Ok(());
    };
    tunnel(connection, router.clone(), peer, serving.plane.bind).await
}

/// Serve every stream the page opens, [`MAX_STREAMS`] at a time, until the page closes the
/// session or leaves it idle. The router is a parameter so a test hands in its own.
pub(crate) async fn tunnel(
    connection: Connection,
    router: Router,
    peer: SocketAddr,
    local: SocketAddr,
) -> Result<()> {
    let streams = Arc::new(tokio::sync::Semaphore::new(MAX_STREAMS));
    tracing::info!(%peer, "management tunnel open");
    loop {
        let (tx, rx) = match tokio::time::timeout(IDLE, connection.accept_bi()).await {
            Ok(stream) => stream.context("accept a tunnel stream")?,
            Err(_) if streams.available_permits() == MAX_STREAMS => {
                tracing::info!(%peer, "management tunnel idle, closing");
                connection.close(VarInt::from_u32(0), b"idle");
                return Ok(());
            }
            Err(_) => continue,
        };
        let permit = streams.clone().try_acquire_owned();
        let router = router.clone();
        tokio::spawn(async move {
            let response = match permit {
                Ok(_held) => {
                    let mut rx = rx;
                    match tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut rx)).await {
                        Ok(Ok((head, body))) => dispatch(&router, peer, local, head, body).await,
                        Ok(Err(refusal)) => refusal,
                        Err(_) => api_error(
                            StatusCode::REQUEST_TIMEOUT,
                            "the request did not arrive whole in time",
                        ),
                    }
                }
                Err(_) => api_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    "too many requests in flight on this tunnel",
                ),
            };
            if let Err(e) = write_response(tx, response).await {
                tracing::debug!(%peer, error = %e, "tunnel response not delivered");
            }
        });
    }
}

/// Length-prefixed head, then the body until FIN, both capped. `Err` is the response to send.
async fn read_request<R: AsyncRead + Unpin>(rx: &mut R) -> Result<(Head, Vec<u8>), Response> {
    let truncated = |_| api_error(StatusCode::BAD_REQUEST, "the request is truncated");
    let mut len = [0u8; 4];
    rx.read_exact(&mut len).await.map_err(truncated)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_HEAD {
        return Err(api_error(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "the request head is over 16 KiB",
        ));
    }
    let mut head = vec![0u8; len];
    rx.read_exact(&mut head).await.map_err(truncated)?;
    let head: Head = serde_json::from_slice(&head)
        .map_err(|_| api_error(StatusCode::BAD_REQUEST, "the request head is not one"))?;
    let mut body = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = rx.read(&mut buf).await.map_err(truncated)?;
        if n == 0 {
            break;
        }
        if body.len() + n > MAX_BODY {
            return Err(api_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "the request body is over 1 MiB",
            ));
        }
        body.extend_from_slice(&buf[..n]);
    }
    Ok((head, body))
}

/// One request into the router, as the LAN peer it came from.
///
/// Refused here, before the router: a method other than `GET` or `POST`, a path outside
/// `/api/v1/` or under `/api/v1/local/` (the position-authenticated lane, which a LAN peer must
/// never reach), and any header a browser's `fetch` could not have sent. What passes is
/// stamped like a request off the plain listener — the peer, the bound address, no client
/// certificate — so `require_auth` judges it as the LAN request it is.
pub(crate) async fn dispatch(
    router: &Router,
    peer: SocketAddr,
    local: SocketAddr,
    head: Head,
    body: Vec<u8>,
) -> Response {
    let method = match head.m.as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        _ => {
            return api_error(
                StatusCode::METHOD_NOT_ALLOWED,
                "the tunnel carries GET and POST only",
            )
        }
    };
    if !path_reachable(&head.p) {
        return api_error(
            StatusCode::FORBIDDEN,
            "that path is not reachable through the tunnel",
        );
    }
    let mut req = Request::builder().method(method).uri(&head.p);
    for (name, value) in &head.h {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            return api_error(StatusCode::BAD_REQUEST, "a header name is not one");
        };
        if !PASSED.contains(&name) {
            continue;
        }
        let Ok(value) = HeaderValue::from_str(value) else {
            return api_error(StatusCode::BAD_REQUEST, "a header value is not one");
        };
        req = req.header(name, value);
    }
    let Ok(mut req) = req.body(Body::from(body)) else {
        return api_error(StatusCode::BAD_REQUEST, "the request path is not one");
    };
    req.extensions_mut().insert(PeerAddr(peer));
    req.extensions_mut().insert(LocalAddr(local));
    req.extensions_mut().insert(PeerCertFingerprint(None));
    let status_of = |r: &Response| r.status().as_u16();
    let response = router.clone().oneshot(req).await.expect("infallible");
    tracing::debug!(%peer, method = %head.m, path = %head.p, status = status_of(&response), "tunnelled");
    response
}

/// `/api/v1/` and nothing under `/api/v1/local/`, raw and percent-decoded alike: the auth gate
/// compares raw paths, and a route's own decoding must not land where the raw path said it
/// would not. A dot segment or an empty one is refused rather than resolved.
fn path_reachable(p: &str) -> bool {
    let path = p.split(['?', '#']).next().unwrap_or("");
    let clean = |s: &str| {
        s.starts_with("/api/v1/")
            && !s.starts_with("/api/v1/local/")
            && !s[1..]
                .split('/')
                .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    };
    clean(path) && clean(&percent_decode(path))
}

/// `%41` → `A`. A malformed escape is kept as it is; the router refuses it on its own.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok())
            .flatten()
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The reply head, then the body streamed frame by frame, then FIN. Covers run to megabytes,
/// so the body is never collected first.
async fn write_response<W: AsyncWrite + Unpin>(mut tx: W, response: Response) -> Result<()> {
    let (parts, mut body) = response.into_parts();
    let h = parts
        .headers
        .iter()
        .filter_map(|(n, v)| {
            v.to_str()
                .ok()
                .map(|v| (n.as_str().to_string(), v.to_string()))
        })
        .collect();
    let head = serde_json::to_vec(&Reply {
        s: parts.status.as_u16(),
        h,
    })
    .context("encode the reply head")?;
    tx.write_all(&(head.len() as u32).to_be_bytes()).await?;
    tx.write_all(&head).await?;
    while let Some(frame) =
        std::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx)).await
    {
        let frame = frame.map_err(|e| anyhow::anyhow!("read the response body: {e}"))?;
        if let Ok(data) = frame.into_data() {
            tx.write_all(&data).await?;
        }
    }
    tx.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::{get, post};
    use http_body_util::BodyExt as _;
    use tokio::io::duplex;

    fn frame(head: &str, body: &[u8]) -> Vec<u8> {
        let mut v = (head.len() as u32).to_be_bytes().to_vec();
        v.extend_from_slice(head.as_bytes());
        v.extend_from_slice(body);
        v
    }

    #[tokio::test]
    async fn an_oversized_head_is_refused_before_it_is_read() {
        let (mut page, mut host) = duplex(1 << 16);
        page.write_all(&((MAX_HEAD as u32) + 1).to_be_bytes())
            .await
            .unwrap();
        drop(page);
        let refused = read_request(&mut host).await.unwrap_err();
        assert_eq!(
            refused.status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn an_oversized_body_is_refused() {
        let (mut page, mut host) = duplex(1 << 16);
        let writer = tokio::spawn(async move {
            let head = r#"{"m":"POST","p":"/api/v1/client-logs"}"#;
            page.write_all(&frame(head, b"")).await.unwrap();
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..17 {
                if page.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        let refused = read_request(&mut host).await.unwrap_err();
        assert_eq!(refused.status(), StatusCode::PAYLOAD_TOO_LARGE);
        drop(host);
        let _ = writer.await;
    }

    #[tokio::test]
    async fn a_truncated_or_malformed_head_is_refused() {
        let (mut page, mut host) = duplex(1 << 16);
        page.write_all(&[0, 0, 0, 9, b'{']).await.unwrap();
        drop(page);
        assert_eq!(
            read_request(&mut host).await.unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
        let (mut page, mut host) = duplex(1 << 16);
        page.write_all(&frame("not json", b"")).await.unwrap();
        drop(page);
        assert_eq!(
            read_request(&mut host).await.unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn a_whole_request_is_read_with_its_body() {
        let (mut page, mut host) = duplex(1 << 16);
        let head =
            r#"{"m":"POST","p":"/api/v1/game/end","h":[["content-type","application/json"]]}"#;
        page.write_all(&frame(head, b"{\"app_id\":\"x\"}"))
            .await
            .unwrap();
        drop(page);
        let (head, body) = read_request(&mut host).await.unwrap();
        assert_eq!(head.m, "POST");
        assert_eq!(head.p, "/api/v1/game/end");
        assert_eq!(head.h.len(), 1);
        assert_eq!(body, b"{\"app_id\":\"x\"}");
    }

    /// The one lane the tunnel must never open is the position-authenticated one, raw or
    /// encoded; and nothing outside the versioned API at all.
    #[test]
    fn only_the_versioned_api_is_reachable_and_never_the_local_lane() {
        assert!(path_reachable("/api/v1/health"));
        assert!(path_reachable("/api/v1/library/page?limit=50&cursor=abc"));
        assert!(path_reachable("/api/v1/library/art/steam%3A570"));
        assert!(!path_reachable("/api/v1/local/summary"));
        assert!(!path_reachable("/api/v1/local%2Fsummary"));
        assert!(!path_reachable("/api/v1/%6cocal/summary"));
        assert!(!path_reachable("/api/v1/../local/summary"));
        assert!(!path_reachable("/api/v1/x/%2e%2e/local/summary"));
        assert!(!path_reachable("/api/v1//host"));
        assert!(!path_reachable("/api/docs"));
        assert!(path_reachable("/api/v1/host?x=/api/v1/local/summary"));
        assert!(!path_reachable("api/v1/host"));
        assert!(!path_reachable("https://host:47990/api/v1/host"));
        assert!(!path_reachable(""));
    }

    #[test]
    fn a_fifth_tunnel_from_one_address_is_refused() {
        let ip: IpAddr = "10.9.8.7".parse().unwrap();
        let held: Vec<IpSlot> = (0..MAX_SESSIONS_PER_IP)
            .map(|_| IpSlot::take(ip).expect("within the cap"))
            .collect();
        assert!(IpSlot::take(ip).is_none(), "one over the cap");
        let other: IpAddr = "10.9.8.8".parse().unwrap();
        assert!(
            IpSlot::take(other).is_some(),
            "another address is not charged"
        );
        drop(held);
        assert!(IpSlot::take(ip).is_some(), "released on drop");
    }

    /// Only the headers a browser's `fetch` could have sent reach the router. `host` and the
    /// forwarding family never do, whatever a page or a tool wrote in the head.
    #[tokio::test]
    async fn only_the_allowlisted_headers_reach_the_router() {
        let router = Router::new().route(
            "/api/v1/seen",
            get(|headers: axum::http::HeaderMap| async move {
                let mut names: Vec<String> =
                    headers.keys().map(|k| k.as_str().to_string()).collect();
                names.sort();
                names.join(",")
            }),
        );
        let lan: SocketAddr = "192.168.1.44:52000".parse().unwrap();
        let plane: SocketAddr = "0.0.0.0:9778".parse().unwrap();
        let head = Head {
            m: "GET".into(),
            p: "/api/v1/seen".into(),
            h: [
                ("authorization", "Bearer x"),
                ("Host", "127.0.0.1:47990"),
                ("x-forwarded-for", "127.0.0.1"),
                ("forwarded", "for=127.0.0.1"),
                ("cookie", "a=b"),
                ("accept", "*/*"),
                ("content-type", "text/plain"),
                ("if-none-match", "\"etag\""),
            ]
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect(),
        };
        let res = dispatch(&router, lan, plane, head, Vec::new()).await;
        assert_eq!(res.status(), StatusCode::OK);
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            &body[..],
            b"accept,authorization,content-type,if-none-match"
        );

        let bad = Head {
            m: "GET".into(),
            p: "/api/v1/seen".into(),
            h: vec![("authorization".into(), "Bearer\r\nX: y".into())],
        };
        let res = dispatch(&router, lan, plane, bad, Vec::new()).await;
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "a value no header can hold"
        );
    }

    /// A page's whole conversation over a real loopback WebTransport session: a read, a post
    /// with a body, a refused method, and the in-flight cap.
    #[tokio::test]
    async fn a_page_reaches_the_router_through_a_loopback_tunnel() {
        let router = Router::new()
            .route("/api/v1/health", get(|| async { "ok" }))
            .route(
                "/api/v1/echo",
                post(|body: String| async move { format!("echo:{body}") }),
            );
        let identity = wtransport::Identity::self_signed(["localhost"]).unwrap();
        let cert = identity.certificate_chain().as_slice()[0].hash();
        let loopback: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = wtransport::Endpoint::server(
            wtransport::ServerConfig::builder()
                .with_bind_address(loopback)
                .with_identity(identity)
                .build(),
        )
        .unwrap();
        let bind = server.local_addr().unwrap();
        let url = format!("https://127.0.0.1:{}/mgmt", bind.port());
        tokio::spawn(async move {
            let request = server.accept().await.await.unwrap();
            assert_eq!(request.path(), "/mgmt");
            let conn = request.accept().await.unwrap();
            let peer = conn.remote_address();
            let _ = tunnel(conn, router, peer, bind).await;
        });
        let page = wtransport::Endpoint::client(
            wtransport::ClientConfig::builder()
                .with_bind_address(loopback)
                .with_server_certificate_hashes([cert])
                .build(),
        )
        .unwrap()
        .connect(url)
        .await
        .unwrap();

        let (status, headers, body) = call(&page, r#"{"m":"GET","p":"/api/v1/health"}"#, b"").await;
        assert_eq!(status, 200);
        assert_eq!(body, b"ok");
        assert!(headers.iter().any(|(n, _)| n == "content-type"));

        let (status, _, body) = call(
            &page,
            r#"{"m":"POST","p":"/api/v1/echo"}"#,
            b"the page's log",
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body, b"echo:the page's log");

        let (status, _, _) = call(&page, r#"{"m":"DELETE","p":"/api/v1/health"}"#, b"").await;
        assert_eq!(status, 405);

        // Sixteen streams opened and left without FIN hold every slot; the seventeenth is told
        // so at once rather than queued behind them.
        let mut held = Vec::new();
        for _ in 0..MAX_STREAMS {
            let (mut tx, rx) = page.open_bi().await.unwrap().await.unwrap();
            tx.write_all(&frame(r#"{"m":"GET","p":"/api/v1/health"}"#, b""))
                .await
                .unwrap();
            held.push((tx, rx));
        }
        let (status, _, _) = call(&page, r#"{"m":"GET","p":"/api/v1/health"}"#, b"").await;
        assert_eq!(status, 429, "one over the in-flight cap");
        for (mut tx, mut rx) in held {
            tx.finish().await.unwrap();
            let (status, _, _) = read_reply(&mut rx).await;
            assert_eq!(status, 200, "the held streams are served once they finish");
        }
    }

    /// One request as the page sends it: open, write, finish, read the reply.
    async fn call(
        page: &Connection,
        head: &str,
        body: &[u8],
    ) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let (mut tx, mut rx) = page.open_bi().await.unwrap().await.unwrap();
        tx.write_all(&frame(head, body)).await.unwrap();
        // A refusal answers before the request is read to its end, and the host stops reading;
        // the page's own `close()` of its writer sees the same and must not mind.
        let _ = tx.finish().await;
        read_reply(&mut rx).await
    }

    async fn read_reply(rx: &mut wtransport::RecvStream) -> (u16, Vec<(String, String)>, Vec<u8>) {
        let mut all = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(Some(n)) = rx.read(&mut buf).await {
            all.extend_from_slice(&buf[..n]);
        }
        let len = u32::from_be_bytes(all[..4].try_into().unwrap()) as usize;
        let reply: serde_json::Value = serde_json::from_slice(&all[4..4 + len]).unwrap();
        let headers = reply["h"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p[0].as_str().unwrap().to_string(),
                    p[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
        (
            reply["s"].as_u64().unwrap() as u16,
            headers,
            all[4 + len..].to_vec(),
        )
    }
}
