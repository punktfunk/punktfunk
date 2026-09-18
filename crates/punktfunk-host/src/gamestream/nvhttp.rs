//! GameStream nvhttp: plain HTTP on 47989 and mutual-TLS on 47984.
//!
//! Routes: `/serverinfo`, `/pair`, `/applist`, `/appasset`, `/launch`, `/resume`, `/cancel`.
//! HTTPS is mTLS; `/serverinfo` reports `PairStatus=1` only for a pinned client cert.
//!
//! The PIN is out of band via bearer `POST /api/v1/pair/pin`. Do not add an nvhttp PIN
//! route — a client could submit the PIN it already displays and pin its own cert.
//!
//! Pairing and grants: `design/per-client-access.md`.

use super::tls::{PeerAddr, PeerCertFingerprint};
use super::{serverinfo, AppState, LaunchSession, HTTPS_PORT, HTTP_PORT, RTSP_PORT};
use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, MethodRouter},
    Extension, Router,
};
use punktfunk_core::quic::{GRANT_ALL, GRANT_LAUNCH};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// HTTPS listener: the client presented a cert (mTLS). HTTP is never paired.
#[derive(Clone, Copy)]
struct Https(bool);

pub async fn run(state: Arc<AppState>) -> Result<()> {
    // Request and verify the client cert; Moonlight presents one after pairing.
    let tls = super::tls::server_config(&state.identity.cert_pem, &state.identity.key_pem)?;

    let http_addr = SocketAddr::from(([0, 0, 0, 0], HTTP_PORT));
    let https_addr = SocketAddr::from(([0, 0, 0, 0], HTTPS_PORT));
    tracing::info!(%http_addr, %https_addr, "nvhttp listening (serverinfo + pair + launch)");

    // HTTPS handshake attaches `PeerCertFingerprint`; post-pair routes gate on the allow-list.
    tokio::try_join!(
        super::tls::serve_plain(http_addr, router(state.clone(), false)),
        super::tls::serve_https(https_addr, router(state, true), tls),
    )?;
    Ok(())
}

/// Pinned SHA-256 of the HTTPS client cert. HTTP has no cert, so never paired.
fn peer_is_paired(peer: &Option<Extension<PeerCertFingerprint>>, st: &AppState) -> bool {
    let Some(Extension(PeerCertFingerprint(Some(fp)))) = peer else {
        return false;
    };
    st.paired
        .lock()
        .unwrap()
        .iter()
        .any(|der| hex::encode(punktfunk_core::quic::endpoint::cert_fingerprint(der)) == *fp)
}

/// Hex fingerprint as the 32-byte form [`LaunchSession::owner_fp`] stores.
fn peer_fp(peer: &Option<Extension<PeerCertFingerprint>>) -> Option<[u8; 32]> {
    match peer {
        Some(Extension(PeerCertFingerprint(Some(fp)))) => hex::decode(fp)
            .ok()
            .and_then(|v| <[u8; 32]>::try_from(v).ok()),
        _ => None,
    }
}

/// Effective grant mask for this HTTPS peer. `None` is expired — fail closed like unpaired.
/// No registry row is ungoverned (`GRANT_ALL`); a console-created row governs. Certless
/// peers also return `None` (they never pass [`peer_is_paired`]). See
/// `design/per-client-access.md`.
fn peer_grants(peer: &Option<Extension<PeerCertFingerprint>>, st: &AppState) -> Option<u32> {
    let Some(Extension(PeerCertFingerprint(Some(fp)))) = peer else {
        return None;
    };
    match st.access.get() {
        Some(np) => np.moonlight_effective(fp, super::wall_unix_now()),
        // No registry (tests / embedders that skip `serve`): pre-grants = full control.
        None => Some(GRANT_ALL),
    }
}

/// Resume/cancel: true if there is no session, fingerprints match, or either side is unknown.
/// Unknown fingerprints fail open so a same-client control is never locked out.
fn peer_may_control_session(peer: &Option<Extension<PeerCertFingerprint>>, st: &AppState) -> bool {
    match st.launch.lock().unwrap().as_ref() {
        None => true,
        Some(session) => match (session.owner_fp, peer_fp(peer)) {
            (Some(owner), Some(caller)) => owner == caller,
            // Unknown fp: pairing already passed; fail open.
            _ => true,
        },
    }
}

/// What a route demands of its caller. Every route is built from [`routes`], so one cannot
/// exist without a row here; the classification test drives each row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gate {
    /// Either listener, any peer: the answer is what pairing itself needs.
    Open,
    /// A pinned HTTPS client cert, else the nvhttp error XML.
    Paired,
    /// Same gate, refused with a bare 403: the body would be read as an image.
    PairedAsset,
}

/// The whole nvhttp surface, each route with its gate.
fn routes() -> [(&'static str, Gate, MethodRouter<Arc<AppState>>); 7] {
    [
        ("/serverinfo", Gate::Open, get(h_serverinfo)),
        ("/pair", Gate::Open, get(h_pair)),
        ("/applist", Gate::Paired, get(h_applist)),
        ("/appasset", Gate::PairedAsset, get(h_appasset)),
        ("/launch", Gate::Paired, get(h_launch)),
        ("/resume", Gate::Paired, get(h_resume)),
        ("/cancel", Gate::Paired, get(h_cancel)),
    ]
}

fn gate_for(path: &str) -> Option<Gate> {
    routes()
        .iter()
        .find(|(p, _, _)| *p == path)
        .map(|(_, g, _)| *g)
}

/// The open routes plus the paired ones behind one [`require_paired`] layer.
fn router(state: Arc<AppState>, https: bool) -> Router {
    let mut open = Router::new();
    let mut paired = Router::new();
    for (path, gate, handler) in routes() {
        match gate {
            Gate::Open => open = open.route(path, handler),
            Gate::Paired | Gate::PairedAsset => paired = paired.route(path, handler),
        }
    }
    let paired = paired.route_layer(middleware::from_fn_with_state(
        state.clone(),
        require_paired,
    ));
    open.merge(paired)
        .layer(Extension(Https(https)))
        .with_state(state)
}

/// The paired gate, once, ahead of every [`Gate::Paired`] handler: a pinned HTTPS client cert
/// ([`peer_is_paired`]) or the route's rejection. Handlers behind it still read the
/// fingerprint for grants and ownership.
async fn require_paired(State(st): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<PeerCertFingerprint>()
        .cloned()
        .map(Extension);
    if peer_is_paired(&peer, &st) {
        return next.run(req).await;
    }
    let path = req.uri().path();
    tracing::warn!(path, "nvhttp request rejected — client is not paired");
    if gate_for(path) == Some(Gate::PairedAsset) {
        StatusCode::FORBIDDEN.into_response()
    } else {
        xml(error_xml()).into_response()
    }
}

fn xml(body: String) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/xml")], body)
}

async fn h_serverinfo(
    State(st): State<Arc<AppState>>,
    Extension(Https(https)): Extension<Https>,
    peer: Option<Extension<PeerCertFingerprint>>,
) -> impl IntoResponse {
    let paired = https && peer_is_paired(&peer, &st);
    // Owner-only `currentgame`: Moonlight uses it to show Resume/Quit.
    let current_game = if paired {
        owner_current_game(&st.launch.lock().unwrap(), peer_fp(&peer))
    } else {
        0
    };
    xml(serverinfo::serverinfo_xml(
        &st.host,
        https,
        paired,
        current_game,
    ))
}

/// `currentgame` for this caller: the live appid only when both fingerprints are known and
/// equal, else 0. Stricter than [`peer_may_control_session`]: unknown fps fail closed here
/// (advertisement) but open there (control).
fn owner_current_game(launch: &Option<LaunchSession>, caller: Option<[u8; 32]>) -> u32 {
    match (launch, caller) {
        (Some(s), Some(fp)) if s.owner_fp == Some(fp) => s.appid,
        _ => 0,
    }
}

async fn h_applist() -> impl IntoResponse {
    xml(super::apps::applist_xml())
}

/// Cover or mark tile for `appid`. Fetch is disk+network, so `spawn_blocking`. 404 (no art, no
/// known mark, or fetch failure) is Moonlight's title-only placeholder.
async fn h_appasset(Query(q): Query<HashMap<String, String>>) -> Response {
    let Some(appid) = q.get("appid").and_then(|s| s.parse::<u32>().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match tokio::task::spawn_blocking(move || super::apps::appasset_bytes(appid)).await {
        Ok(Some((bytes, ctype))) => ([(header::CONTENT_TYPE, ctype)], bytes).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn h_launch(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
    addr: Option<Extension<PeerAddr>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    // GameStream holds one session, so the default `separate` steals it (`gamestream_admission`).
    // GameStream has no per-device overlay to apply: its peer identity is the
    // pairing cert, not the native fingerprint the overlay is keyed by.
    let conflict = crate::vdisplay::admission::effective_conflict(None);
    launch_under(st, peer, addr, q, conflict).await
}

/// `/launch` under an explicit mode-conflict policy.
async fn launch_under(
    st: Arc<AppState>,
    peer: Option<Extension<PeerCertFingerprint>>,
    addr: Option<Extension<PeerAddr>>,
    q: HashMap<String, String>,
    conflict: crate::vdisplay::policy::ModeConflict,
) -> Response {
    // GRANT_LAUNCH + unexpired, besides pairing. GameStream has no join of an owner-launched
    // session, and no reject vocabulary — the client sees the generic error XML.
    match peer_grants(&peer, &st) {
        Some(g) if g & GRANT_LAUNCH != 0 => {}
        Some(_) => {
            tracing::warn!("launch rejected — this client's access grants do not include Launch");
            return xml(error_xml()).into_response();
        }
        None => {
            tracing::warn!("launch rejected — this client's access has expired");
            return xml(error_xml()).into_response();
        }
    }
    let req_fp: Option<[u8; 32]> = peer_fp(&peer);

    // Snapshot owner + mode (Copy) so the launch lock is not held over admission.
    let mut steal = false;
    {
        let live = st
            .launch
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| (s.owner_fp, (s.width, s.height, s.fps)));
        match gamestream_admission(live, req_fp, conflict) {
            GsDecision::Serve => {}
            GsDecision::Steal => steal = true,
            GsDecision::Reject(why) => {
                tracing::warn!(
                    why,
                    "GameStream launch REJECTED — the session belongs to another client"
                );
                return (StatusCode::SERVICE_UNAVAILABLE, xml(error_xml())).into_response();
            }
        }
    }

    match launch(&st, &q) {
        Ok(mut session) => {
            if steal {
                // A drop, not a quit — the same as a native steal victim. `launch` clears
                // first, so the control tick tells the old client while its media stop.
                tracing::info!("GameStream launch STEAL — ending the live session");
                use std::sync::atomic::Ordering::SeqCst;
                let before = st.media_exited.load(SeqCst);
                let live = u64::from(st.streaming.load(SeqCst))
                    + u64::from(st.audio_streaming.load(SeqCst));
                st.end_session("another client took the session");
                wait_media_exit(&st, before, live).await;
            }
            // Bind unauthenticated RTSP/UDP to this paired client's source IP.
            session.peer_ip = addr.map(|Extension(PeerAddr(a))| a.ip());
            session.owner_fp = req_fp;
            // New session: last quit reason does not apply (`AppState::quit`).
            st.quit.store(false, std::sync::atomic::Ordering::SeqCst);
            // Mint ping before RTSP SETUP. Media planes use it to tell this client's first
            // datagram from others at the same address.
            st.mint_av_ping();
            *st.launch.lock().unwrap() = Some(session);
            tracing::info!(
                w = session.width,
                h = session.height,
                fps = session.fps,
                rikeyid = session.rikeyid,
                "launch — session created; RTSP at rtsp://{}:{RTSP_PORT}",
                st.host.local_ip()
            );
            xml(session_url_xml(&st, "gamesession")).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "launch failed");
            xml(error_xml()).into_response()
        }
    }
}

async fn h_resume(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
    addr: Option<Extension<PeerAddr>>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    // Resume re-attaches input and media, so the same GRANT_LAUNCH + expiry gate as `/launch`.
    match peer_grants(&peer, &st) {
        Some(g) if g & GRANT_LAUNCH != 0 => {}
        Some(_) => {
            tracing::warn!("resume rejected — this client's access grants do not include Launch");
            return xml(error_xml());
        }
        None => {
            tracing::warn!("resume rejected — this client's access has expired");
            return xml(error_xml());
        }
    }
    if !peer_may_control_session(&peer, &st) {
        tracing::warn!("resume rejected — caller does not own the session");
        return xml(error_xml());
    }
    // PLAY skips if `streaming` is still true, so clear the flags and wait for exit.
    let before = st.media_exited.load(std::sync::atomic::Ordering::SeqCst);
    let expected = u64::from(
        st.streaming
            .swap(false, std::sync::atomic::Ordering::SeqCst),
    ) + u64::from(
        st.audio_streaming
            .swap(false, std::sync::atomic::Ordering::SeqCst),
    );
    wait_media_exit(&st, before, expected).await;
    // Resume mints new rikey; control-GCM and audio-CBC derive from it. Present → replace;
    // malformed → refuse (streaming on keys the client does not hold is worse); absent → keep.
    // Teardown during the wait can clear `launch`. ANNOUNCE re-negotiates audio — ignore
    // surroundAudioInfo here.
    {
        let mut launch = st.launch.lock().unwrap();
        let Some(session) = launch.as_mut() else {
            return xml(error_xml());
        };
        if q.contains_key("rikey") {
            match parse_rikey(&q) {
                Ok((gcm_key, rikeyid)) => {
                    session.gcm_key = gcm_key;
                    session.rikeyid = rikeyid;
                    tracing::info!(
                        rikeyid,
                        "resume — session re-keyed with the client's fresh rikey"
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "resume rejected — malformed rikey");
                    return xml(error_xml());
                }
            }
        }
        // Re-bind RTSP/media IP filters to the resume source; the client may have changed networks.
        if let Some(Extension(PeerAddr(a))) = addr {
            session.peer_ip = Some(a.ip());
        }
    }
    // New ping: RTSP is plaintext until `SS_ENC_CONTROL_V2`, so the previous payload may
    // already be on the wire.
    st.mint_av_ping();
    xml(session_url_xml(&st, "resume"))
}

async fn h_cancel(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
) -> impl IntoResponse {
    // Expiry fails closed; GRANT_LAUNCH does not apply. Cancel is Quit App for the owner
    // (`peer_may_control_session`); denying it wedges the session they are ending.
    if peer_grants(&peer, &st).is_none() {
        tracing::warn!("cancel rejected — this client's access has expired");
        return xml(error_xml());
    }
    if !peer_may_control_session(&peer, &st) {
        tracing::warn!("cancel rejected — caller does not own the session");
        return xml(error_xml());
    }
    // `/cancel` is Quit App, not a drop: `quit_session` sets the quit flag so the virtual
    // display skips keep-alive linger and end-game policy treats it as operator intent.
    st.quit_session("client /cancel");
    xml("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><cancel>1</cancel></root>\n".to_string())
}

/// Wait for `expected` stopped media threads to exit, so their teardown cannot stomp a
/// successor's capturer or flags. 2 s bound: a timeout proceeds, media-less at worst.
async fn wait_media_exit(st: &AppState, before: u64, expected: u64) {
    if expected == 0 {
        return;
    }
    tracing::info!(
        threads = expected,
        "stopping the previous connection's media threads"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while st.media_exited.load(std::sync::atomic::Ordering::SeqCst) < before + expected {
        if std::time::Instant::now() >= deadline {
            tracing::warn!("previous media threads still exiting after 2 s; proceeding");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// `rikey` (16-byte AES hex) and signed `rikeyid` (negative values wrap to a BE u32 IV).
fn parse_rikey(q: &HashMap<String, String>) -> Result<([u8; 16], i32)> {
    let rikey = q.get("rikey").ok_or_else(|| anyhow!("missing rikey"))?;
    let key_bytes = hex::decode(rikey).context("rikey hex")?;
    if key_bytes.len() < 16 {
        return Err(anyhow!("rikey too short"));
    }
    let mut gcm_key = [0u8; 16];
    gcm_key.copy_from_slice(&key_bytes[..16]);
    let rikeyid: i32 = q.get("rikeyid").and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((gcm_key, rikeyid))
}

fn launch(_st: &AppState, q: &HashMap<String, String>) -> Result<LaunchSession> {
    let (gcm_key, rikeyid) = parse_rikey(q)?;
    let (width, height, fps) = q
        .get("mode")
        .and_then(|m| parse_mode(m))
        .unwrap_or((1920, 1080, 60));
    let appid = q.get("appid").and_then(|s| s.parse().ok()).unwrap_or(1);
    Ok(LaunchSession {
        gcm_key,
        rikeyid,
        width,
        height,
        fps,
        appid,
        peer_ip: None,  // `h_launch` fills from the verified HTTPS peer
        owner_fp: None, // `h_launch` fills from the client cert
    })
}

/// GameStream `mode`: `"WxHxFPS"`.
fn parse_mode(mode: &str) -> Option<(u32, u32, u32)> {
    let mut it = mode.split('x');
    let w = it.next()?.parse().ok()?;
    let h = it.next()?.parse().ok()?;
    let fps = it.next()?.parse().ok()?;
    Some((w, h, fps))
}

/// `(owner_fp, mode)` snapshot for [`gamestream_admission`].
type LiveGs = (Option<[u8; 32]>, (u32, u32, u32));

enum GsDecision {
    /// No session, or the same client again.
    Serve,
    /// End the live session, then serve (`steal`/`separate`: there is one session).
    Steal,
    /// 503, and why — the line an operator reads when a second client is turned away.
    Reject(&'static str),
}

/// Single-session mode-conflict. No session or same client → Serve. A different client
/// applies `policy`; GameStream has no `separate`, so `steal`/`separate` both Steal.
///
/// `join` cannot be honored here: this plane holds one launch on fixed media ports, so two
/// clients cannot share a display the way the native one lets them. Refusing keeps the
/// client that is already streaming, which is what asking for `join` asked for — stealing
/// its session would be the opposite.
fn gamestream_admission(
    live: Option<LiveGs>,
    req_fp: Option<[u8; 32]>,
    policy: crate::vdisplay::policy::ModeConflict,
) -> GsDecision {
    use crate::vdisplay::policy::ModeConflict;
    let Some((owner, _mode)) = live else {
        return GsDecision::Serve;
    };
    let different = match (owner, req_fp) {
        (Some(o), Some(r)) => o != r,
        _ => true, // unknown owner or anonymous requester: treat as a different client
    };
    if !different {
        return GsDecision::Serve;
    }
    match policy {
        ModeConflict::Reject => GsDecision::Reject("mode_conflict=reject"),
        ModeConflict::Join => {
            GsDecision::Reject("mode_conflict=join, which this plane cannot do — one session")
        }
        ModeConflict::Steal | ModeConflict::Separate => GsDecision::Steal,
    }
}

fn session_url_xml(st: &AppState, tag: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\">\n<sessionUrl0>rtsp://{}:{RTSP_PORT}</sessionUrl0>\n<{tag}>1</{tag}>\n</root>\n",
        st.host.local_ip()
    )
}

async fn h_pair(
    State(st): State<Arc<AppState>>,
    peer: Option<Extension<PeerCertFingerprint>>,
    addr: Option<Extension<PeerAddr>>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let uniqueid = q.get("uniqueid").cloned().unwrap_or_default();
    let phrase = q.get("phrase").map(String::as_str);

    let step = phrase
        .filter(|p| *p == "getservercert" || *p == "pairchallenge")
        .or_else(|| {
            [
                "clientchallenge",
                "serverchallengeresp",
                "clientpairingsecret",
            ]
            .into_iter()
            .find(|k| q.contains_key(*k))
        })
        .unwrap_or("?");
    tracing::info!(uniqueid, step, "pair request");

    // Every phase is bound to it: phase 1 parks the PIN under it, 2-4 check it.
    let peer_ip = addr
        .as_ref()
        .map_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), |a| {
            (a.0).0.ip()
        });
    let result = if phrase == Some("getservercert") {
        match (q.get("salt"), q.get("clientcert")) {
            (Some(salt), Some(cc)) => {
                st.pairing
                    .getservercert(&st.identity, &uniqueid, salt, cc, peer_ip)
                    .await
            }
            _ => Ok(pair_error_xml()),
        }
    } else if phrase == Some("pairchallenge") {
        // Last step is over TLS with the just-pinned cert. Only a pinned caller is told they
        // are paired; HTTP or an unpinned cert get the unpaired answer.
        if peer_is_paired(&peer, &st) {
            Ok(paired_ok_xml())
        } else {
            tracing::warn!("pairchallenge rejected — client is not paired");
            Ok(pair_error_xml())
        }
    } else if let Some(v) = q.get("clientchallenge") {
        st.pairing
            .clientchallenge(&st.identity, &uniqueid, v, peer_ip)
    } else if let Some(v) = q.get("serverchallengeresp") {
        st.pairing
            .serverchallengeresp(&st.identity, &uniqueid, v, peer_ip)
    } else if let Some(v) = q.get("clientpairingsecret") {
        let r = st
            .pairing
            .clientpairingsecret(&uniqueid, v, &st.paired, peer_ip);
        // First pairing: bring ENet control up now (idempotent). Moonlight connects control
        // before video; waiting would abort the session.
        if let Err(e) = super::sync_control(&st) {
            tracing::warn!(error = %format!("{e:#}"), "control port sync after pairing failed");
        }
        r
    } else {
        Ok(pair_error_xml())
    };

    let body = result.unwrap_or_else(|e| {
        tracing::warn!(error = %format!("{e:#}"), uniqueid, "pair handler error");
        pair_error_xml()
    });
    xml(body)
}

fn paired_ok_xml() -> String {
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><paired>1</paired></root>\n"
        .to_string()
}

fn pair_error_xml() -> String {
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"200\"><paired>0</paired></root>\n"
        .to_string()
}

fn error_xml() -> String {
    "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<root status_code=\"400\"></root>\n".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> Arc<AppState> {
        let host = super::super::Host {
            hostname: "t".into(),
            uniqueid: "id".into(),
            http_port: HTTP_PORT,
            https_port: HTTPS_PORT,
            os_chain: "linux".into(),
            os_name: "Linux".into(),
        };
        let identity = super::super::cert::ServerIdentity::ephemeral().expect("ephemeral identity");
        let stats = crate::stats_recorder::StatsRecorder::new(
            std::env::temp_dir().join(format!("pf-nvhttp-stats-{}", std::process::id())),
        );
        Arc::new(AppState::new(host, identity, stats))
    }

    fn fp_of(der: &[u8]) -> String {
        hex::encode(punktfunk_core::quic::endpoint::cert_fingerprint(der))
    }

    /// One request through the router as the TLS layer would deliver it.
    async fn drive(
        st: &Arc<AppState>,
        path: &str,
        peer: Option<PeerCertFingerprint>,
    ) -> (StatusCode, String) {
        use tower::ServiceExt;
        let mut req = axum::http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap();
        if let Some(p) = peer {
            req.extensions_mut().insert(p);
        }
        let resp = router(st.clone(), true).oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// Every route has a gate row, and the layer enforces it: an unpaired peer gets each
    /// paired route's own rejection and every open route's answer, while a pinned cert
    /// passes the layer. A route added without a row cannot compile (`routes` is the router).
    #[tokio::test]
    async fn every_nvhttp_route_is_classified_and_the_gate_enforces_it() {
        let st = test_state();
        let der = b"a-client-cert-der".to_vec();
        st.paired.lock().unwrap().push(der.clone());
        let pinned = PeerCertFingerprint(Some(fp_of(&der)));
        let stranger = PeerCertFingerprint(Some(fp_of(b"someone-else")));

        let table = routes();
        let mut seen = std::collections::HashSet::new();
        for (path, _, _) in &table {
            assert!(seen.insert(*path), "{path} is routed twice");
        }
        for (path, gate, _) in &table {
            assert_eq!(gate_for(path), Some(*gate));
            let (status, body) = drive(&st, path, Some(stranger.clone())).await;
            let (certless, _) = drive(&st, path, None).await;
            match gate {
                Gate::Open => {
                    assert_eq!(status, StatusCode::OK, "{path} is open to a stranger");
                    assert_eq!(certless, StatusCode::OK, "{path} is open without a cert");
                }
                Gate::Paired => {
                    assert_eq!(status, StatusCode::OK, "{path}: the nvhttp error is a 200");
                    assert_eq!(
                        body,
                        error_xml(),
                        "{path} must answer a stranger with the error XML"
                    );
                }
                Gate::PairedAsset => {
                    assert_eq!(status, StatusCode::FORBIDDEN, "{path} must 403 a stranger");
                    assert_eq!(certless, StatusCode::FORBIDDEN);
                }
            }
        }
        // The pinned cert passes the layer: `/applist` answers, `/appasset` reaches its own
        // argument check (400, not the gate's 403).
        let (status, body) = drive(&st, "/applist", Some(pinned.clone())).await;
        assert_eq!(status, StatusCode::OK);
        assert_ne!(body, error_xml(), "a pinned cert must not be rejected");
        let (status, _) = drive(&st, "/appasset", Some(pinned)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn launch_gate_requires_a_pinned_client_cert() {
        let st = test_state();
        let der = b"a-client-cert-der".to_vec();
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_of(&der)))));

        assert!(!peer_is_paired(&peer, &st), "unknown cert must be rejected");
        assert!(
            !peer_is_paired(&None, &st),
            "no client cert must be rejected"
        );
        assert!(
            !peer_is_paired(&Some(Extension(PeerCertFingerprint(None))), &st),
            "certless HTTPS peer must be rejected"
        );

        st.paired.lock().unwrap().push(der);
        assert!(peer_is_paired(&peer, &st), "pinned cert must be accepted");
        let other = Some(Extension(PeerCertFingerprint(Some(fp_of(
            b"different-der",
        )))));
        assert!(
            !peer_is_paired(&other, &st),
            "a non-pinned cert stays rejected"
        );
    }

    #[tokio::test]
    async fn pairchallenge_answers_only_a_pinned_client() {
        async fn challenge(
            st: &Arc<AppState>,
            peer: Option<Extension<PeerCertFingerprint>>,
        ) -> String {
            let q = HashMap::from([("phrase".to_string(), "pairchallenge".to_string())]);
            let resp = h_pair(State(st.clone()), peer, None, Query(q))
                .await
                .into_response();
            let b = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap();
            String::from_utf8(b.to_vec()).unwrap()
        }

        let st = test_state();
        let der = b"pairchallenge-client-der".to_vec();
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_of(&der)))));

        let plain = challenge(&st, None).await;
        assert!(plain.contains("<paired>0</paired>"), "plain HTTP: {plain}");
        let unpinned = challenge(&st, peer.clone()).await;
        assert!(
            unpinned.contains("<paired>0</paired>"),
            "unpinned cert: {unpinned}"
        );

        st.paired.lock().unwrap().push(der);
        let pinned = challenge(&st, peer).await;
        assert!(pinned.contains("<paired>1</paired>"), "pinned: {pinned}");
    }

    #[test]
    fn gamestream_admission_policy_matrix() {
        use crate::vdisplay::policy::ModeConflict;
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let live = Some((Some(a), (2560, 1440, 120)));
        assert!(matches!(
            gamestream_admission(None, Some(b), ModeConflict::Reject),
            GsDecision::Serve
        ));
        assert!(matches!(
            gamestream_admission(live, Some(a), ModeConflict::Reject),
            GsDecision::Serve
        ));
        assert!(matches!(
            gamestream_admission(live, Some(b), ModeConflict::Reject),
            GsDecision::Reject(_)
        ));
        // Sharing is what `join` asks for and what this plane cannot do, so the client that
        // is streaming keeps its session rather than losing it to the newcomer.
        assert!(matches!(
            gamestream_admission(live, Some(b), ModeConflict::Join),
            GsDecision::Reject(_)
        ));
        assert!(matches!(
            gamestream_admission(live, Some(b), ModeConflict::Steal),
            GsDecision::Steal
        ));
        assert!(matches!(
            gamestream_admission(live, Some(b), ModeConflict::Separate),
            GsDecision::Steal
        ));
        // No cert: treat as a different client.
        assert!(matches!(
            gamestream_admission(live, None, ModeConflict::Reject),
            GsDecision::Reject(_)
        ));
    }

    fn test_registry(
        tag: &str,
    ) -> (
        Arc<crate::native_pairing::NativePairing>,
        std::path::PathBuf,
    ) {
        let p = std::env::temp_dir().join(format!(
            "pf-nvhttp-access-{tag}-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&p);
        let np = Arc::new(
            crate::native_pairing::NativePairing::load_with(Some(p.clone()), None, false).unwrap(),
        );
        (np, p)
    }

    #[test]
    fn peer_grants_resolution_rule() {
        use crate::native_pairing::Access;
        use punktfunk_core::quic::GRANT_GAMEPAD;
        let st = test_state();
        let der = b"grants-client-der".to_vec();
        let fp_hex = fp_of(&der);
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_hex.clone()))));

        assert_eq!(peer_grants(&peer, &st), Some(GRANT_ALL));

        let (np, store) = test_registry("rule");
        assert!(st.access.set(np.clone()).is_ok());
        assert_eq!(peer_grants(&peer, &st), Some(GRANT_ALL));
        np.add_with_access(
            "Guest",
            &fp_hex,
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: None,
                until_disconnect: false,
            }),
        )
        .unwrap();
        assert_eq!(peer_grants(&peer, &st), Some(GRANT_GAMEPAD));
        assert_eq!(peer_grants(&peer, &st).unwrap() & GRANT_LAUNCH, 0);
        np.set_access(
            &fp_hex,
            Access {
                grants: GRANT_ALL,
                expires_unix: Some(super::super::wall_unix_now() - 5),
                until_disconnect: false,
            },
        )
        .unwrap();
        assert_eq!(peer_grants(&peer, &st), None);
        assert_eq!(peer_grants(&None, &st), None);
        assert_eq!(
            peer_grants(&Some(Extension(PeerCertFingerprint(None))), &st),
            None
        );
        let _ = std::fs::remove_file(&store);
    }

    #[tokio::test]
    async fn resume_and_cancel_honor_grants_and_expiry() {
        use crate::native_pairing::Access;
        use punktfunk_core::quic::GRANT_GAMEPAD;

        async fn body_of(resp: Response) -> String {
            let b = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap();
            String::from_utf8(b.to_vec()).unwrap()
        }

        let st = test_state();
        let der = b"resume-grants-client".to_vec();
        let fp_hex = fp_of(&der);
        let owner_fp = punktfunk_core::quic::endpoint::cert_fingerprint(&der);
        st.paired.lock().unwrap().push(der);
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_hex.clone()))));
        let (np, store) = test_registry("resume");
        assert!(st.access.set(np.clone()).is_ok());
        let session = LaunchSession {
            gcm_key: [0; 16],
            rikeyid: 0,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            peer_ip: None,
            owner_fp: Some(owner_fp),
        };
        *st.launch.lock().unwrap() = Some(session);

        let ok = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(HashMap::new()))
                .await
                .into_response(),
        )
        .await;
        assert!(ok.contains("<resume>1</resume>"), "ungoverned resume: {ok}");

        let now = super::super::wall_unix_now();
        np.add_with_access(
            "Guest",
            &fp_hex,
            Some(Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 3600),
                until_disconnect: false,
            }),
        )
        .unwrap();
        let no = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(HashMap::new()))
                .await
                .into_response(),
        )
        .await;
        assert!(!no.contains("<resume>1</resume>"), "no-LAUNCH resume: {no}");

        np.set_access(
            &fp_hex,
            Access {
                grants: GRANT_ALL,
                expires_unix: Some(now - 5),
                until_disconnect: false,
            },
        )
        .unwrap();
        let no = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(HashMap::new()))
                .await
                .into_response(),
        )
        .await;
        assert!(!no.contains("<resume>1</resume>"), "expired resume: {no}");
        let no = body_of(
            h_cancel(State(st.clone()), peer.clone())
                .await
                .into_response(),
        )
        .await;
        assert!(!no.contains("<cancel>1</cancel>"), "expired cancel: {no}");
        assert!(
            st.launch.lock().unwrap().is_some(),
            "a refused cancel must not tear the session down"
        );

        // Owner cancel is not GRANT_LAUNCH-gated: a limited client can still quit.
        np.set_access(
            &fp_hex,
            Access {
                grants: GRANT_GAMEPAD,
                expires_unix: Some(now + 3600),
                until_disconnect: false,
            },
        )
        .unwrap();
        let ok = body_of(
            h_cancel(State(st.clone()), peer.clone())
                .await
                .into_response(),
        )
        .await;
        assert!(ok.contains("<cancel>1</cancel>"), "owner cancel: {ok}");
        assert!(st.launch.lock().unwrap().is_none(), "cancel tears down");
        let _ = std::fs::remove_file(&store);
    }

    /// Non-owner / unknown fp sees `currentgame=0`, so Moonlight stays on `/launch` (admission)
    /// instead of owner-only `/resume`.
    #[test]
    fn current_game_is_owner_scoped() {
        let owner = [7u8; 32];
        let other = [9u8; 32];
        let session = |owner_fp: Option<[u8; 32]>| {
            Some(LaunchSession {
                gcm_key: [0; 16],
                rikeyid: 0,
                width: 1920,
                height: 1080,
                fps: 60,
                appid: 4242,
                peer_ip: None,
                owner_fp,
            })
        };
        assert_eq!(owner_current_game(&session(Some(owner)), Some(owner)), 4242);
        assert_eq!(owner_current_game(&session(Some(owner)), Some(other)), 0);
        // Unknown fps fail closed here (unlike the control gate).
        assert_eq!(owner_current_game(&session(None), Some(owner)), 0);
        assert_eq!(owner_current_game(&session(Some(owner)), None), 0);
        assert_eq!(owner_current_game(&None, Some(owner)), 0);
    }

    #[tokio::test]
    async fn resume_rekeys_the_session() {
        async fn body_of(resp: Response) -> String {
            let b = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                .await
                .unwrap();
            String::from_utf8(b.to_vec()).unwrap()
        }
        let st = test_state();
        let der = b"resume-rekey-client".to_vec();
        let fp_hex = fp_of(&der);
        let owner_fp = punktfunk_core::quic::endpoint::cert_fingerprint(&der);
        st.paired.lock().unwrap().push(der);
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_hex))));
        *st.launch.lock().unwrap() = Some(LaunchSession {
            gcm_key: [0x11; 16],
            rikeyid: 1,
            width: 1920,
            height: 1080,
            fps: 60,
            appid: 1,
            peer_ip: None,
            owner_fp: Some(owner_fp),
        });

        let mut q = HashMap::new();
        q.insert("rikey".to_string(), "22".repeat(16));
        q.insert("rikeyid".to_string(), "-5".to_string());
        let ok = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(q))
                .await
                .into_response(),
        )
        .await;
        assert!(ok.contains("<resume>1</resume>"), "re-keyed resume: {ok}");
        {
            let launch = st.launch.lock().unwrap();
            let s = launch.as_ref().unwrap();
            assert_eq!(s.gcm_key, [0x22; 16]);
            assert_eq!(s.rikeyid, -5);
        }

        let mut bad = HashMap::new();
        bad.insert("rikey".to_string(), "zz".to_string());
        let no = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(bad))
                .await
                .into_response(),
        )
        .await;
        assert!(!no.contains("<resume>1</resume>"), "malformed rikey: {no}");
        assert_eq!(
            st.launch.lock().unwrap().as_ref().unwrap().gcm_key,
            [0x22; 16]
        );

        let ok = body_of(
            h_resume(State(st.clone()), peer.clone(), None, Query(HashMap::new()))
                .await
                .into_response(),
        )
        .await;
        assert!(ok.contains("<resume>1</resume>"), "keyless resume: {ok}");
        assert_eq!(st.launch.lock().unwrap().as_ref().unwrap().rikeyid, -5);
    }

    fn owned_session(der: &[u8]) -> LaunchSession {
        LaunchSession {
            gcm_key: [0x11; 16],
            rikeyid: 1,
            width: 2560,
            height: 1440,
            fps: 120,
            appid: 1,
            peer_ip: None,
            owner_fp: Some(punktfunk_core::quic::endpoint::cert_fingerprint(der)),
        }
    }

    async fn resumes(st: &Arc<AppState>, der: &[u8]) -> bool {
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_of(der)))));
        let resp = h_resume(State(st.clone()), peer, None, Query(HashMap::new()))
            .await
            .into_response();
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        String::from_utf8_lossy(&body).contains("<resume>1</resume>")
    }

    /// A `/launch` steal stops the owner's media and hands the session over as a drop, not a
    /// quit. Only the new owner may resume.
    #[tokio::test]
    async fn a_launch_steal_ends_the_live_session_and_hands_resume_over() {
        use crate::vdisplay::policy::ModeConflict;
        use std::sync::atomic::Ordering::SeqCst;
        let st = test_state();
        let (owner, thief) = (b"steal-owner".to_vec(), b"steal-thief".to_vec());
        *st.launch.lock().unwrap() = Some(owned_session(&owner));
        st.streaming.store(true, SeqCst);
        st.audio_streaming.store(true, SeqCst);
        // Stand-in for the two media threads: they exit once their flags drop.
        let threads = {
            let st = st.clone();
            tokio::spawn(async move {
                while st.streaming.load(SeqCst) || st.audio_streaming.load(SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                st.media_exited.fetch_add(2, SeqCst);
            })
        };

        let q = HashMap::from([
            ("rikey".to_string(), "22".repeat(16)),
            ("rikeyid".to_string(), "9".to_string()),
            ("mode".to_string(), "1920x1080x60".to_string()),
        ]);
        let peer = Some(Extension(PeerCertFingerprint(Some(fp_of(&thief)))));
        let resp = launch_under(st.clone(), peer, None, q, ModeConflict::Steal).await;
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("<gamesession>1</gamesession>"));
        assert!(
            !st.streaming.load(SeqCst) && !st.audio_streaming.load(SeqCst),
            "the owner's media must stop"
        );
        threads.await.unwrap();
        assert!(!st.quit.load(SeqCst), "a steal is a drop, not a quit");
        assert_eq!(
            st.launch.lock().unwrap().as_ref().unwrap().owner_fp,
            Some(punktfunk_core::quic::endpoint::cert_fingerprint(&thief))
        );
        assert!(!resumes(&st, &owner).await, "the victim cannot resume");
        assert!(resumes(&st, &thief).await);
    }

    /// A display stolen through admission ends the session on the control tick, as a drop,
    /// once. The victim then has nothing to resume.
    #[tokio::test]
    async fn a_stolen_display_ends_the_session_and_refuses_resume() {
        use std::sync::atomic::Ordering::SeqCst;
        let st = test_state();
        let owner = b"stolen-display-owner".to_vec();
        *st.launch.lock().unwrap() = Some(owned_session(&owner));
        st.streaming.store(true, SeqCst);
        st.audio_streaming.store(true, SeqCst);
        assert!(!st.end_if_preempted(), "nothing stole it");
        assert!(st.streaming.load(SeqCst));

        st.preempted.store(true, SeqCst); // what `Admission::Steal` does to each victim
        assert!(st.end_if_preempted());
        assert!(!st.streaming.load(SeqCst) && !st.audio_streaming.load(SeqCst));
        assert!(st.launch.lock().unwrap().is_none());
        assert!(!st.quit.load(SeqCst), "a steal is a drop, not a quit");
        assert!(!st.end_if_preempted(), "handled once");
        assert!(!resumes(&st, &owner).await);
    }
}
