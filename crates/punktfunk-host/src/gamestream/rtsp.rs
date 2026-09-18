//! GameStream RTSP handshake on TCP 48010. Hand-rolled: GameStream uses `streamid=`
//! targets, the literal `DEADBEEFCAFE` session, and X-SS-* headers that stock RTSP
//! crates do not speak. Moonlight sequence: OPTIONS → DESCRIBE → SETUP(audio/video/
//! control) → ANNOUNCE → PLAY. ANNOUNCE carries the negotiated stream config; PLAY
//! starts the media stages.
//!
//! One native thread per connection, not the per-frame hot path. DESCRIBE offers
//! `SS_ENC_VIDEO` (per-shard AES-128-GCM, on by default, never REQUIRED;
//! `PUNKTFUNK_GS_ENCRYPT=0` opts out) and `SS_ENC_CONTROL_V2` (per-direction control
//! nonces; also lets the client seal RTSP; `PUNKTFUNK_GS_ENCRYPT=video` drops just
//! this). Audio is AES-CBC regardless; `SS_ENC_AUDIO` is not offered (layout not in
//! the wire reference). See [`EncOffer`].
//!
//! A sealed connection is recognised, not negotiated: [`ENCRYPTED_MESSAGE_TYPE_BIT`].

use super::audio;
use super::stream::{self, StreamConfig};
use super::{AppState, LaunchSession, AUDIO_PORT, CONTROL_PORT, RTSP_PORT, VIDEO_PORT};
use crate::encode::Codec;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// Unauthenticated, one thread per connection. Cap concurrent slots, per-read
// timeout, whole-request deadline, and header/body size so a pre-auth peer cannot
// pin a thread or grow memory. Real GameStream RTSP is a few hundred bytes.
const MAX_RTSP_CONNS: usize = 8;
const RTSP_READ_TIMEOUT: Duration = Duration::from_secs(15);
// Per-read timeout bounds one read, not the request: a byte just inside it
// resets forever and pins a slot. This bounds the whole message instead.
const RTSP_REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const MAX_RTSP_HEADER: usize = 16 * 1024;
const MAX_RTSP_BODY: usize = 64 * 1024;
const MAX_RTSP_MSG: usize = 128 * 1024;

/// Live connection count. Cap is [`MAX_RTSP_CONNS`]; [`ConnGuard`] decrements on panic too.
static RTSP_ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Releases the [`RTSP_ACTIVE`] slot on exit or panic.
struct ConnGuard;
impl Drop for ConnGuard {
    fn drop(&mut self) {
        RTSP_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

pub fn spawn(state: Arc<AppState>) -> Result<()> {
    let listener = super::tls::bind_exclusive((std::net::Ipv4Addr::UNSPECIFIED, RTSP_PORT).into())
        .with_context(|| format!("bind RTSP {RTSP_PORT}"))?;
    tracing::info!(port = RTSP_PORT, "RTSP listening");
    std::thread::Builder::new()
        .name("punktfunk-rtsp".into())
        .spawn(move || {
            for conn in listener.incoming() {
                match conn {
                    Ok(stream) => {
                        if RTSP_ACTIVE.fetch_add(1, Ordering::Relaxed) >= MAX_RTSP_CONNS {
                            RTSP_ACTIVE.fetch_sub(1, Ordering::Relaxed);
                            tracing::warn!("RTSP: too many concurrent connections — dropping");
                            continue; // `stream` drops, connection closed
                        }
                        // Guard before spawn: if `thread::spawn` panics (OS thread limit), Drop
                        // still releases the slot.
                        let guard = ConnGuard;
                        let st = state.clone();
                        std::thread::spawn(move || {
                            let _guard = guard;
                            if let Err(e) = handle_conn(stream, st) {
                                tracing::warn!(error = %format!("{e:#}"), "RTSP connection ended");
                            }
                        });
                    }
                    Err(e) => tracing::warn!(error = %e, "RTSP accept failed"),
                }
            }
        })
        .context("spawn RTSP thread")?;
    Ok(())
}

struct Request {
    method: String,
    uri: String,
    cseq: String,
    head: String,
    body: String,
}

fn handle_conn(mut stream: TcpStream, state: Arc<AppState>) -> Result<()> {
    let peer = stream.peer_addr().ok();
    let _ = stream.set_read_timeout(Some(RTSP_READ_TIMEOUT));
    let deadline = Instant::now() + RTSP_REQUEST_DEADLINE;
    let mut buf: Vec<u8> = Vec::new();
    // First byte picks framing: MSB set = sealed (`typeAndLength`); ASCII method
    // = plaintext. Answer in kind — no negotiation state.
    if !fill_at_least(&mut stream, &mut buf, 1, deadline)? {
        return Ok(());
    }
    let sealed = buf[0] & 0x80 != 0;
    // Sealed RTSP uses the `/launch` rikey, so it cannot precede a launch.
    let key = state.launch.lock().unwrap().map(|s| s.gcm_key);
    let key = match (sealed, key) {
        (false, _) => None,
        (true, Some(k)) => Some(k),
        (true, None) => {
            anyhow::bail!("sealed RTSP message arrived with no launch session to key it")
        }
    };
    // One request per TCP connection: moonlight-common-c reads until EOF, so
    // we answer once and close. Session state lives in `AppState`.
    let req = match key {
        Some(k) => read_sealed_message(&mut stream, &mut buf, deadline, &k)?,
        None => read_message(&mut stream, &mut buf, deadline)?,
    };
    if let Some(req) = req {
        tracing::debug!(
            method = %req.method,
            cseq = %req.cseq,
            sealed,
            headers = %req.head.replace("\r\n", " | "),
            body = %req.body.replace("\r\n", " | "),
            "RTSP request"
        );
        let resp = handle_request(&req, &state, peer);
        let out = match key {
            Some(k) => seal_response(&k, resp.as_bytes()),
            None => resp.into_bytes(),
        };
        stream.write_all(&out).context("RTSP write")?;
        stream.flush().ok();
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    Ok(())
}

/// One RTSP message (headers + Content-Length body). Remainder stays in `buf`.
/// `deadline` is the whole-request budget; the per-read timeout does not bound that.
fn read_message(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
) -> Result<Option<Request>> {
    loop {
        if Instant::now() >= deadline {
            anyhow::bail!("RTSP request deadline exceeded");
        }
        if let Some(end) = find_subslice(buf, b"\r\n\r\n") {
            // Cap even when the terminator is present: an oversized block with `\r\n\r\n`
            // would otherwise skip the no-terminator cap below.
            if end > MAX_RTSP_HEADER {
                anyhow::bail!("RTSP headers exceed limit");
            }
            let head = std::str::from_utf8(&buf[..end]).context("RTSP header utf8")?;
            let content_len = header_value(head, "content-length")
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            // Bound Content-Length before buffering it (allocation amplification).
            if content_len > MAX_RTSP_BODY {
                anyhow::bail!("RTSP Content-Length {content_len} exceeds limit");
            }
            let total = end + 4 + content_len;
            if buf.len() < total {
                // Headers complete; body still arriving.
            } else {
                let head = head.to_string();
                let body = String::from_utf8_lossy(&buf[end + 4..total]).into_owned();
                buf.drain(..total);
                return Ok(Some(parse_request(&head, body)));
            }
        } else if buf.len() > MAX_RTSP_HEADER {
            // No terminator within the cap — dribble would grow forever.
            anyhow::bail!("RTSP headers exceed limit");
        }
        let mut tmp = [0u8; 8192];
        let n = stream.read(&mut tmp).context("RTSP read")?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_RTSP_MSG {
            anyhow::bail!("RTSP message exceeds limit");
        }
    }
}

fn parse_request(head: &str, body: String) -> Request {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let uri = parts.next().unwrap_or("").to_string();
    let cseq = header_value(head, "cseq").unwrap_or("0").trim().to_string();
    Request {
        method,
        uri,
        cseq,
        head: head.to_string(),
        body,
    }
}

/// Gate a state-changing RTSP verb on the pairing-gated `/launch` session.
/// The RTSP/UDP media plane is unauthenticated, so honor `ANNOUNCE`/`PLAY`/
/// `TEARDOWN` only for the client that completed `/launch`, and — when the
/// launching IP is known — only from that source IP. `nvhttp` pins `/launch`
/// to a client cert. Returns the session; `PLAY` needs keys/appid, others use
/// it as a bool.
fn authorized_launch(state: &AppState, peer: Option<SocketAddr>) -> Option<LaunchSession> {
    let ls = (*state.launch.lock().unwrap())?;
    match (ls.peer_ip, peer.map(|p| p.ip())) {
        (Some(want), Some(got)) if want != got => None,
        // Match, or either side missing the address: launch-present only.
        _ => Some(ls),
    }
}

// `&Arc<AppState>` not `&AppState`: PLAY's `'static` session-lost callback
// needs an owned clone.
fn handle_request(req: &Request, state: &Arc<AppState>, peer: Option<SocketAddr>) -> String {
    match req.method.as_str() {
        "OPTIONS" => response(
            &req.cseq,
            &[("Public", "OPTIONS DESCRIBE SETUP ANNOUNCE PLAY TEARDOWN")],
            None,
        ),
        "DESCRIBE" => response(
            &req.cseq,
            &[("Content-Type", "application/sdp")],
            Some(&describe_sdp()),
        ),
        "SETUP" => {
            // SETUP hands out the ping payload the media planes verify. Ungated, any
            // peer on 48010 can ask for it and win the endpoint race.
            if authorized_launch(state, peer).is_none() {
                tracing::warn!(
                    ?peer,
                    "RTSP SETUP — refused: not the paired `/launch` owner"
                );
                return response_status("401 Unauthorized", &req.cseq, &[], None);
            }
            let (port, extra_key) = match stream_type(&req.uri) {
                Some("audio") => (AUDIO_PORT, "X-SS-Ping-Payload"),
                Some("video") => (VIDEO_PORT, "X-SS-Ping-Payload"),
                Some("control") => (CONTROL_PORT, "X-SS-Connect-Data"),
                _ => return response_status("404 Not Found", &req.cseq, &[], None),
            };
            let transport = format!("server_port={port}");
            // Session ping payload from `/launch`. The client echoes it as its first
            // datagram on each media port.
            let payload = hex::encode(state.av_ping_payload());
            response(
                &req.cseq,
                &[
                    ("Session", "DEADBEEFCAFE;timeout = 90"),
                    ("Transport", &transport),
                    (extra_key, &payload),
                ],
                None,
            )
        }
        "ANNOUNCE" => {
            // ANNOUNCE overwrites stream/audio config; gate to the launch owner.
            if authorized_launch(state, peer).is_none() {
                tracing::warn!(
                    ?peer,
                    "RTSP ANNOUNCE — refused: not the paired `/launch` owner"
                );
                return response_status("401 Unauthorized", &req.cseq, &[], None);
            }
            let map = parse_announce(&req.body);
            match stream_config(&map) {
                Some(cfg) => {
                    tracing::info!(?cfg, "RTSP ANNOUNCE — negotiated stream config");
                    *state.stream.lock().unwrap() = Some(cfg);
                }
                None => tracing::warn!("RTSP ANNOUNCE — missing required video config keys"),
            }
            let ap = audio_params(&map);
            tracing::info!(?ap, "RTSP ANNOUNCE — negotiated audio params");
            *state.audio_params.lock().unwrap() = ap;
            response(&req.cseq, &[], None)
        }
        "PLAY" => {
            let Some(ls) = authorized_launch(state, peer) else {
                tracing::warn!(?peer, "RTSP PLAY — refused: not the paired `/launch` owner");
                return response_status("401 Unauthorized", &req.cseq, &[], None);
            };
            let cfg = *state.stream.lock().unwrap();
            // Ends the whole session (both planes + launch). One plane detecting a
            // dead client must not leave the other streaming or a stale launch.
            let on_lost: super::OnSessionLost = {
                let st = state.clone();
                Arc::new(move || {
                    st.end_session("client unreachable");
                })
            };
            match cfg {
                Some(cfg) if !state.streaming.swap(true, Ordering::SeqCst) => {
                    let app = super::apps::by_id(ls.appid);
                    tracing::info!(app = ?app.as_ref().map(|a| &a.title), "RTSP PLAY — starting video stream");
                    stream::start(
                        cfg,
                        app,
                        state.streaming.clone(),
                        state.force_idr.clone(),
                        state.rfi_range.clone(),
                        state.loss_stats.clone(),
                        state.video_hdr.clone(),
                        // Rikey reaches the video plane only when `SS_ENC_VIDEO` was negotiated.
                        cfg.encrypt_video.then_some(ls.gcm_key),
                        state.video_cap.clone(),
                        state.stats.clone(),
                        on_lost.clone(),
                        state.media_exited.clone(),
                        state.counters.clone(),
                        // Game exit is a deliberate end (player finished), not a drop. Same
                        // distinction as the native close code; teardown policy keys off it.
                        stream::GameLifetime {
                            quit: state.quit.clone(),
                            preempted: state.preempted.clone(),
                            fingerprint: ls.owner_fp.map(hex::encode),
                            owner_ip: ls.peer_ip,
                            av_ping: state.av_ping_payload(),
                            on_game_exit: {
                                let st = state.clone();
                                Arc::new(move || {
                                    st.quit_session("game exited");
                                })
                            },
                        },
                    );
                }
                Some(_) => tracing::info!("RTSP PLAY — stream already running"),
                None => tracing::warn!("RTSP PLAY — no negotiated config (ANNOUNCE missing)"),
            }
            // Audio is independent (Opus UDP 48000). Needs the launch key for the
            // AES-CBC payload the client expects.
            if !state.audio_streaming.swap(true, Ordering::SeqCst) {
                tracing::info!("RTSP PLAY — starting audio stream");
                audio::start(
                    state.audio_streaming.clone(),
                    ls.gcm_key,
                    ls.rikeyid,
                    *state.audio_params.lock().unwrap(),
                    state.audio_cap.clone(),
                    on_lost,
                    // Same owner-IP bind as video: only the launching peer's pings count.
                    ls.peer_ip,
                    // Same ping payload: first datagram from that address is this client.
                    state.av_ping_payload(),
                    state.media_exited.clone(),
                );
            }
            response(&req.cseq, &[("Session", "DEADBEEFCAFE;timeout = 90")], None)
        }
        "TEARDOWN" => {
            // Gate TEARDOWN: an unpaired peer must not stop a paired client's media.
            if authorized_launch(state, peer).is_none() {
                tracing::warn!(
                    ?peer,
                    "RTSP TEARDOWN — refused: not the paired `/launch` owner"
                );
                return response_status("401 Unauthorized", &req.cseq, &[], None);
            }
            state.streaming.store(false, Ordering::SeqCst);
            state.audio_streaming.store(false, Ordering::SeqCst);
            response(&req.cseq, &[], None)
        }
        other => {
            tracing::warn!(method = other, "RTSP unsupported method");
            response_status("501 Not Implemented", &req.cseq, &[], None)
        }
    }
}

/// moonlight-common-c `LI_FF_PEN_TOUCH_EVENTS`. Set: clients send native
/// `SS_PEN`/`SS_TOUCH` instead of synthesizing mouse input.
const SS_FF_PEN_TOUCH_EVENTS: u32 = 0x01;

/// Per-shard AES-128-GCM video (`Limelight-internal.h`). `SS_ENC_AUDIO` 0x04
/// is not offered: audio-GCM layout is not in the wire reference.
const SS_ENC_VIDEO: u32 = 0x02;

/// Direction byte in the GCM nonce: `[10..12]` = `b"CC"` client→host, `b"HC"`
/// host→client. Legacy nonce is just the sender's `seq`, so host and client
/// share one (key, nonce) space and collide when counters cross — the AES-GCM
/// catastrophic failure. Also lets the client seal RTSP; [`read_sealed_message`].
const SS_ENC_CONTROL_V2: u32 = 0x01;

/// Video-encryption offer from `PUNKTFUNK_GS_ENCRYPT`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum EncOffer {
    /// `0` — advertise nothing. Escape hatch for a client that mis-negotiates.
    Off,
    /// Default. Advertise `SS_ENC_VIDEO` and `SS_ENC_CONTROL_V2` as SUPPORTED
    /// and let the client decide.
    Supported,
    /// `video` — video encryption only; control stays on the legacy nonce.
    /// `Off` would drop video encryption too.
    VideoOnly,
    /// `require` — also list offered bits as REQUESTED. Test lever: a LAN
    /// client may decline under `Supported`. Not a shipping mode.
    Required,
}

/// Encryption offer from the `gamestream_encrypt` setting. Default is [`EncOffer::Supported`].
/// `0` is plaintext; `video` drops the control offer; `require` also REQUESTS both.
fn gs_video_encryption_offer() -> EncOffer {
    static ON: std::sync::OnceLock<EncOffer> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        match pf_host_config::knob("PUNKTFUNK_GAMESTREAM_ENCRYPT")
            .as_deref()
            .map(str::trim)
        {
            Some("0") | Some("off") | Some("false") | Some("no") => EncOffer::Off,
            Some("video") | Some("video-only") => EncOffer::VideoOnly,
            Some("require") | Some("required") => EncOffer::Required,
            // Unset, `1`, `supported`, or unknown: default.
            _ => EncOffer::Supported,
        }
    })
}

/// `(encryptionSupported, encryptionRequested)` for an offer. Pure so tests
/// need not touch the process env.
fn enc_flags(offer: EncOffer) -> (u32, u32) {
    // REQUESTED is non-zero only for `require`: requiring encryption refuses
    // every client that cannot do it.
    match offer {
        EncOffer::Off => (0, 0),
        EncOffer::VideoOnly => (SS_ENC_VIDEO, 0),
        EncOffer::Supported => (SS_ENC_VIDEO | SS_ENC_CONTROL_V2, 0),
        EncOffer::Required => (
            SS_ENC_VIDEO | SS_ENC_CONTROL_V2,
            SS_ENC_VIDEO | SS_ENC_CONTROL_V2,
        ),
    }
}

/// MSB of `typeAndLength`, set on every sealed RTSP message. Plaintext opens
/// with an ASCII method (first byte < 0x80), so the framings are
/// self-distinguishing: the client picks per connection, we answer in kind.
/// No `corever` threshold (the wire reference names that field, not its value).
const ENCRYPTED_MESSAGE_TYPE_BIT: u32 = 0x8000_0000;

/// `encrypted_rtsp_header_t`: `u32 typeAndLength | u32 sequenceNumber | u8 tag[16]`,
/// big-endian, then `length` bytes of ciphertext.
const ENC_RTSP_HEADER: usize = 24;

/// Host→client RTSP sequence. Process-global and monotonic, never reset.
/// One message per TCP connection: a per-connection counter would restart
/// at 0 for each of a session's seven messages and reuse (key, nonce).
static RTSP_HOST_SEQ: AtomicU32 = AtomicU32::new(0);

/// 12-byte GCM nonce (NIST SP800-38D 8.2.1): `sequenceNumber` BE in `[0..4]`,
/// `[10]` direction (`b'C'` client, `b'H'` host), `[11]` channel `b'R'`.
/// The direction byte keeps each side's counter in its own nonce space.
fn rtsp_nonce(seq: u32, direction: u8) -> [u8; 12] {
    let mut iv = [0u8; 12];
    iv[0..4].copy_from_slice(&seq.to_be_bytes());
    iv[10] = direction;
    iv[11] = b'R';
    iv
}

/// One sealed frame. Pure and direction-parameterised so tests can build
/// the client side of the wire too.
fn seal_frame(key: &[u8; 16], seq: u32, direction: u8, pt: &[u8]) -> Vec<u8> {
    let ct_tag = super::control::gcm_seal(key, &rtsp_nonce(seq, direction), pt, &[]);
    let (ct, tag) = ct_tag.split_at(ct_tag.len() - 16);
    let mut wire = Vec::with_capacity(ENC_RTSP_HEADER + ct.len());
    wire.extend_from_slice(&(ENCRYPTED_MESSAGE_TYPE_BIT | ct.len() as u32).to_be_bytes());
    wire.extend_from_slice(&seq.to_be_bytes());
    wire.extend_from_slice(tag);
    wire.extend_from_slice(ct);
    wire
}

fn seal_response(key: &[u8; 16], pt: &[u8]) -> Vec<u8> {
    seal_frame(key, RTSP_HOST_SEQ.fetch_add(1, Ordering::Relaxed), b'H', pt)
}

/// Open one complete sealed frame and parse the RTSP inside. Split from
/// [`read_sealed_message`] so the wire half is testable without a socket.
/// `frame` must be exactly the header plus its declared payload.
fn open_sealed_frame(key: &[u8; 16], frame: &[u8]) -> Result<Request> {
    anyhow::ensure!(
        frame.len() >= ENC_RTSP_HEADER,
        "sealed RTSP frame too short"
    );
    let seq = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]);
    // Wire order is tag-first; `aes-gcm` wants `ciphertext || tag`.
    let mut ct_tag = frame[ENC_RTSP_HEADER..].to_vec();
    ct_tag.extend_from_slice(&frame[8..ENC_RTSP_HEADER]);
    let pt = super::control::gcm_open(key, &rtsp_nonce(seq, b'C'), &ct_tag, &[])
        .context("sealed RTSP message did not authenticate")?;
    // Ordinary RTSP inside. Frame length already bounds it; do not trust
    // Content-Length the way the plaintext path does.
    let Some(end) = find_subslice(&pt, b"\r\n\r\n") else {
        anyhow::bail!("sealed RTSP message has no header terminator");
    };
    if end > MAX_RTSP_HEADER {
        anyhow::bail!("RTSP headers exceed limit");
    }
    let head = std::str::from_utf8(&pt[..end]).context("RTSP header utf8")?;
    let body = String::from_utf8_lossy(&pt[end + 4..]).into_owned();
    Ok(parse_request(head, body))
}

/// Read until `buf` holds at least `want` bytes. `Ok(false)`: peer closed first.
fn fill_at_least(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    want: usize,
    deadline: Instant,
) -> Result<bool> {
    while buf.len() < want {
        if Instant::now() >= deadline {
            anyhow::bail!("RTSP request deadline exceeded");
        }
        let mut tmp = [0u8; 8192];
        let n = stream.read(&mut tmp).context("RTSP read")?;
        if n == 0 {
            return Ok(false);
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_RTSP_MSG {
            anyhow::bail!("RTSP message exceeds limit");
        }
    }
    Ok(true)
}

/// One sealed RTSP message. Wire order is `tag || ciphertext`; `aes-gcm`
/// wants `ciphertext || tag`.
fn read_sealed_message(
    stream: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
    key: &[u8; 16],
) -> Result<Option<Request>> {
    if !fill_at_least(stream, buf, ENC_RTSP_HEADER, deadline)? {
        return Ok(None);
    }
    let type_and_length = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let len = (type_and_length & !ENCRYPTED_MESSAGE_TYPE_BIT) as usize;
    // Attacker-controlled length, unauthenticated. Bound it before reading;
    // `fill_at_least` also caps, but refusing here skips the read.
    if len > MAX_RTSP_MSG {
        anyhow::bail!("sealed RTSP payload of {len} bytes exceeds limit");
    }
    if !fill_at_least(stream, buf, ENC_RTSP_HEADER + len, deadline)? {
        anyhow::bail!("sealed RTSP message truncated");
    }
    let req = open_sealed_frame(key, &buf[..ENC_RTSP_HEADER + len])?;
    buf.drain(..ENC_RTSP_HEADER + len);
    Ok(Some(req))
}

/// DESCRIBE SDP: HEVC + AV1, surround configs, and the encryption offer.
/// Shipping modes advertise encryption as SUPPORTED, never REQUESTED.
fn describe_sdp() -> String {
    // Advertise pen/touch only where we can inject (Linux uinput; same gate
    // as HOST_CAP_PEN). Else 0 so Moonlight keeps client-side mouse emulation.
    // `PUNKTFUNK_PEN=0` is the kill-switch inside `pen_supported`.
    let feature_flags: u32 = if crate::inject::pen_supported() {
        SS_FF_PEN_TOUCH_EVENTS
    } else {
        0
    };
    let (supported, requested) = enc_flags(gs_video_encryption_offer());
    let mut lines: Vec<String> = vec![
        format!("a=x-ss-general.featureFlags:{feature_flags}"),
        format!("a=x-ss-general.encryptionSupported:{supported}"),
        format!("a=x-ss-general.encryptionRequested:{requested}"),
        "sprop-parameter-sets=AAAAAU".into(), // HEVC capability
        "a=rtpmap:98 AV1/90000".into(),       // AV1 capability
    ];
    // Client takes the first `surround-params=<channelCount>` as normal and
    // a second as HQ, so normal must precede HQ. Stereo lines are Sunshine
    // parity; 2-channel clients hardcode 21101. See `audio::surround_params`.
    for (layout, hq) in [
        (&audio::LAYOUT_STEREO, false),
        (&audio::LAYOUT_STEREO, true),
        (&audio::LAYOUT_51, false),
        (&audio::LAYOUT_51_HQ, true),
        (&audio::LAYOUT_71, false),
        (&audio::LAYOUT_71_HQ, true),
    ] {
        lines.push(format!(
            "a=fmtp:97 surround-params={}",
            audio::surround_params(layout, hq)
        ));
    }
    lines.push(String::new());
    lines.join("\r\n")
}

fn parse_announce(body: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("a=") {
            if let Some((k, v)) = rest.split_once(':') {
                map.insert(k.to_string(), v.to_string());
            }
        }
    }
    map
}

/// ANNOUNCE keys → [`StreamConfig`]. Resolution and packetSize are required.
fn stream_config(map: &HashMap<String, String>) -> Option<StreamConfig> {
    let parse_u = |k: &str| map.get(k).and_then(|s| s.trim().parse::<u32>().ok());
    let width = parse_u("x-nv-video[0].clientViewportWd")?;
    let height = parse_u("x-nv-video[0].clientViewportHt")?;
    // packetSize is pre-auth. It sets per-shard payload (`packet_size - 16`):
    // tiny underflows/div-by-zeros the video thread; huge amplifies allocation.
    // Real Moonlight uses ~1024.
    const PACKET_SIZE_MIN: usize = 64;
    const PACKET_SIZE_MAX: usize = 2048;
    let packet_size = parse_u("x-nv-video[0].packetSize")? as usize;
    if !(PACKET_SIZE_MIN..=PACKET_SIZE_MAX).contains(&packet_size) {
        tracing::warn!(
            packet_size,
            "RTSP ANNOUNCE: out-of-range packetSize — rejecting"
        );
        return None;
    }
    let fps = parse_u("x-nv-video[0].maxFPS")
        .filter(|&f| f > 0)
        .unwrap_or(60);
    // Moonlight floors legacy `x-nv-vqos[0].bw.*` at 100 Mbps (old GFE) and
    // puts the real bitrate in `x-ml-video.configuredBitrateKbps`. Prefer
    // that; fall back to legacy, then 20 Mbps; clamp — ANNOUNCE is pre-auth.
    const MAX_BITRATE_KBPS: u32 = 1_000_000; // 1 Gbps; Moonlight's slider tops at 500
    let bitrate_kbps = parse_u("x-ml-video.configuredBitrateKbps")
        .filter(|&b| b > 0)
        .or_else(|| parse_u("x-nv-vqos[0].bw.maximumBitrateKbps").filter(|&b| b > 0))
        .unwrap_or(20_000)
        .min(MAX_BITRATE_KBPS);
    // moonlight-common-c SdpGenerator.c: 0=H264, 1=HEVC, 2=AV1.
    let codec = match map.get("x-nv-vqos[0].bitStreamFormat").map(|s| s.trim()) {
        Some("1") => Codec::H265,
        Some("2") => Codec::Av1,
        _ => Codec::H264,
    };
    // Moonlight sets `dynamicRangeMode != 0` when it saw Main10 and the user
    // enabled HDR. Honor only if `host_hdr_capable`; otherwise 8-bit SDR.
    let hdr_requested = parse_u("x-nv-video[0].dynamicRangeMode").unwrap_or(0) != 0;
    // `mut` is load-bearing on Linux (probe below clears it). Allow only
    // off-Linux so `unused_mut` still fires if that probe goes away.
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut hdr = hdr_requested && crate::gamestream::host_hdr_capable();
    if hdr_requested && !hdr {
        tracing::warn!(
            "client requested HDR (dynamicRangeMode != 0) but host is not HDR-capable — streaming 8-bit SDR"
        );
    }
    // `host_hdr_capable` is host-wide; this session's codec must also be
    // 10-bit. Else a PQ label over an 8-bit stream. H.264 always degrades.
    if hdr && !crate::encode::can_encode_10bit(codec) {
        tracing::warn!(
            ?codec,
            "client requested HDR but this host cannot encode 10-bit with the codec it \
             negotiated — streaming 8-bit SDR (pick the other codec client-side for HDR)"
        );
        hdr = false;
    }
    // Colour-mode probe is portal-monitor only: HDR needs a monitor in
    // BT.2100 now. Gamescope has no monitor; `host_hdr_capable` already
    // settled it. Probing would refuse every headless gamescope HDR session.
    #[cfg(target_os = "linux")]
    let portal_source = pf_host_config::config().video_source.as_deref() == Some("portal");
    #[cfg(target_os = "linux")]
    if hdr && portal_source && !pf_capture::gnome_hdr_monitor_active() {
        tracing::warn!(
            "client requested HDR but no monitor is in BT.2100 (HDR) colour mode — enable HDR in \
             GNOME Settings → Displays (GNOME 50+) to stream it; streaming 8-bit SDR"
        );
        hdr = false;
    }
    // HDR-capture latch, per source: after a failed `want_hdr` negotiation the
    // capturer offers SDR (portal: until restart, gamescope: until its display
    // is torn down). Without this we'd label PQ while encoding SDR.
    #[cfg(target_os = "linux")]
    let hdr_source = if portal_source {
        pf_capture::HdrSource::PortalMonitor
    } else {
        pf_capture::HdrSource::VirtualOutput
    };
    #[cfg(target_os = "linux")]
    if hdr && pf_capture::hdr_capture_failed(hdr_source) {
        tracing::warn!(
            ?hdr_source,
            "client requested HDR and the host is HDR-capable, but an earlier HDR capture \
             negotiation on this source failed — streaming 8-bit SDR (the portal retries after a \
             host restart, gamescope once its failed display is torn down)"
        );
        hdr = false;
    }
    // `encoderCscMode = (colorspace << 1) | fullRange` (0=Rec601, 1=Rec709,
    // 2=Rec2020). We encode BT.709 limited (SDR) or BT.2020 PQ (HDR) and do
    // not honor this. Warn only when the request is honorable in principle
    // and unmet. HDR10 *is* BT.2020 PQ, so an HDR mismatch is not actionable.
    if let Some(csc) = parse_u("x-nv-video[0].encoderCscMode") {
        let (space, range) = (
            match csc >> 1 {
                0 => "Rec601",
                1 => "Rec709",
                2 => "Rec2020",
                _ => "unknown",
            },
            if csc & 1 != 0 { "full" } else { "limited" },
        );
        let ours = if hdr {
            "Rec2020 limited (PQ)"
        } else {
            "Rec709 limited"
        };
        let matches_ours = (hdr && csc >> 1 == 2 || !hdr && csc >> 1 == 1) && csc & 1 == 0;
        if matches_ours {
            tracing::debug!(
                csc,
                space,
                range,
                "GameStream client requested CSC — matches ours"
            );
        } else if hdr {
            // HDR10 settles colour space; the request is not actionable.
            tracing::debug!(
                csc,
                requested = format!("{space} {range}"),
                encoding = ours,
                "GameStream client requested a CSC that HDR overrides — HDR10 is BT.2020 PQ by \
                 definition, and the stream's VUI says so"
            );
        } else {
            tracing::warn!(
                csc,
                requested = format!("{space} {range}"),
                encoding = ours,
                "GameStream client requested an SDR CSC we don't encode — we signal what we \
                 encode in the VUI, so a VUI-driven client is still correct; a client that \
                 renders from its own request would see shifted colours (unverified — honoring \
                 the request needs the CSC + VUI threaded per session)"
            );
        }
    }
    // Client parity floor (small frames); clamp to a sane max.
    let min_fec = parse_u("x-nv-vqos[0].fec.minRequiredFecPackets")
        .unwrap_or(2)
        .min(16) as u8;
    // Slice count: 1 for hardware decoders, 4 for software. Honor as the
    // encoder ceiling — multi-slice AUs wedge TV SoCs that asked for 1.
    // Absent or out of range (pre-auth) → 1.
    let slices = parse_u("x-nv-video[0].videoEncoderSlicesPerFrame")
        .filter(|n| (1..=32).contains(n))
        .unwrap_or(1);
    // Client echo of DESCRIBE's offer. Mask by what we advertised; a client
    // cannot enable a mode the host never offered.
    let (offered, _) = enc_flags(gs_video_encryption_offer());
    let enabled = parse_u("x-ss-general.encryptionEnabled").unwrap_or(0) & offered;
    let encrypt_video = enabled & SS_ENC_VIDEO != 0;
    if encrypt_video {
        tracing::info!("RTSP ANNOUNCE: client enabled SS_ENC_VIDEO — sealing every video shard");
    }
    // Control bit is not stored: ENet detects V2 from the first authenticating
    // nonce, RTSP from the leading MSB.
    if enabled & SS_ENC_CONTROL_V2 != 0 {
        tracing::info!(
            "RTSP ANNOUNCE: client enabled SS_ENC_CONTROL_V2 — control nonces are per-direction"
        );
    }
    Some(StreamConfig {
        width,
        height,
        fps,
        packet_size,
        bitrate_kbps,
        codec,
        min_fec,
        hdr,
        slices,
        encrypt_video,
    })
}

/// ANNOUNCE → [`audio::AudioParams`]. moonlight-common-c `SdpGenerator.c`:
/// `numChannels`/`channelMask` and `packetDuration` always; `AudioQuality`
/// is 1 only when the client saw our second surround-params line. Unknown
/// channel counts fall back to stereo.
fn audio_params(map: &HashMap<String, String>) -> audio::AudioParams {
    let parse_u = |k: &str| map.get(k).and_then(|s| s.trim().parse::<u32>().ok());
    let requested = parse_u("x-nv-audio.surround.numChannels").unwrap_or(2);
    let channels = match requested {
        2 | 6 | 8 => requested as u8,
        other => {
            tracing::warn!(channels = other, "unsupported channel count — using stereo");
            2
        }
    };
    let high_quality = parse_u("x-nv-audio.surround.AudioQuality") == Some(1);
    // Moonlight sends 5 ms or 10 ms. Snap, don't clamp: 7 ms is not a legal
    // Opus frame size and would fail every encode.
    let packet_duration_ms = match parse_u("x-nv-aqos.packetDuration") {
        Some(d) if d >= 10 => 10,
        _ => 5,
    };
    audio::AudioParams {
        channels,
        high_quality,
        packet_duration_ms,
    }
}

/// SETUP URI `…/streamid=video/0/0` → `"video"` / `"audio"` / `"control"`.
fn stream_type(uri: &str) -> Option<&str> {
    let after = uri.split("streamid=").nth(1)?;
    let token = after.split('/').next()?;
    match token {
        "audio" | "video" | "control" => Some(token),
        _ => None,
    }
}

fn response(cseq: &str, headers: &[(&str, &str)], body: Option<&str>) -> String {
    response_status("200 OK", cseq, headers, body)
}

fn response_status(
    status: &str,
    cseq: &str,
    headers: &[(&str, &str)],
    body: Option<&str>,
) -> String {
    let body = body.unwrap_or("");
    let mut out = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\n");
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    out.push_str(body);
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn header_value<'a>(head: &'a str, key_lower: &str) -> Option<&'a str> {
    head.split("\r\n").find_map(|line| {
        let (k, v) = line.split_once(':')?;
        (k.trim().eq_ignore_ascii_case(key_lower)).then(|| v.trim_start())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn announce(extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut body = String::from(
            "v=0\r\n\
             a=x-nv-video[0].clientViewportWd:1920\r\n\
             a=x-nv-video[0].clientViewportHt:1080\r\n\
             a=x-nv-video[0].packetSize:1392\r\n\
             a=x-nv-video[0].maxFPS:120\r\n\
             a=x-nv-vqos[0].bw.maximumBitrateKbps:40000\r\n",
        );
        for (k, v) in extra {
            body.push_str(&format!("a={k}:{v}\r\n"));
        }
        parse_announce(&body)
    }

    /// Unauthenticated listener: a request that never finishes must not pin a
    /// slot. An already-passed deadline stands in for a dribbling peer.
    #[test]
    fn read_message_gives_up_at_the_request_deadline() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
        let addr = listener.local_addr().expect("local addr");
        let client = TcpStream::connect(addr).expect("connect"); // open, sends nothing
        let (mut server, _) = listener.accept().expect("accept");
        let mut buf: Vec<u8> = Vec::new();
        // `Request` isn't `Debug`, so match rather than `expect_err`.
        let err = match read_message(&mut server, &mut buf, Instant::now()) {
            Err(e) => e,
            Ok(_) => panic!("an elapsed deadline must end the request"),
        };
        assert!(
            format!("{err:#}").contains("deadline"),
            "unexpected error: {err:#}"
        );
        drop(client);
    }

    #[test]
    fn announce_codec_selection() {
        for (fmt, codec) in [
            (Some("0"), Codec::H264),
            (Some("1"), Codec::H265),
            (Some("2"), Codec::Av1),
            (None, Codec::H264),
        ] {
            let map = match fmt {
                Some(f) => announce(&[("x-nv-vqos[0].bitStreamFormat", f)]),
                None => announce(&[]),
            };
            let cfg = stream_config(&map).expect("required keys present");
            assert_eq!(cfg.codec, codec, "bitStreamFormat {fmt:?}");
            assert_eq!((cfg.width, cfg.height, cfg.fps), (1920, 1080, 120));
            assert_eq!(cfg.bitrate_kbps, 40_000);
        }
    }

    /// `configuredBitrateKbps` wins over the legacy max (Moonlight floors that
    /// at 100 Mbps for old GFE).
    #[test]
    fn announce_prefers_configured_bitrate() {
        let map = announce(&[
            ("x-nv-vqos[0].bw.maximumBitrateKbps", "100000"),
            ("x-ml-video.configuredBitrateKbps", "500000"),
        ]);
        assert_eq!(stream_config(&map).unwrap().bitrate_kbps, 500_000);
        assert_eq!(stream_config(&announce(&[])).unwrap().bitrate_kbps, 40_000);
        // Zero configured is ignored; absurd values clamp.
        let zero = announce(&[("x-ml-video.configuredBitrateKbps", "0")]);
        assert_eq!(stream_config(&zero).unwrap().bitrate_kbps, 40_000);
        let huge = announce(&[("x-ml-video.configuredBitrateKbps", "9000000")]);
        assert_eq!(stream_config(&huge).unwrap().bitrate_kbps, 1_000_000);
    }

    #[test]
    fn announce_missing_required_keys() {
        let mut map = announce(&[]);
        map.remove("x-nv-video[0].packetSize");
        assert!(stream_config(&map).is_none());
    }

    /// Out-of-range packetSize must reject here: ≤16 div-by-zeros the video
    /// thread; huge amplifies allocation.
    #[test]
    fn announce_rejects_out_of_range_packet_size() {
        for bad in ["0", "16", "63", "4096", "999999"] {
            let map = announce(&[("x-nv-video[0].packetSize", bad)]);
            assert!(
                stream_config(&map).is_none(),
                "out-of-range packetSize {bad} must be rejected"
            );
        }
        for ok in ["64", "1024", "1392", "2048"] {
            let map = announce(&[("x-nv-video[0].packetSize", ok)]);
            assert!(
                stream_config(&map).is_some(),
                "in-range packetSize {ok} must be accepted"
            );
        }
    }

    /// Slice count is a ceiling: 1 hardware, 4 software. Absent or out of
    /// range must fall back to 1, never reject the session.
    #[test]
    fn announce_slices_per_frame() {
        assert_eq!(stream_config(&announce(&[])).unwrap().slices, 1);
        for want in ["1", "4"] {
            let map = announce(&[("x-nv-video[0].videoEncoderSlicesPerFrame", want)]);
            assert_eq!(
                stream_config(&map).unwrap().slices,
                want.parse::<u32>().unwrap()
            );
        }
        for bad in ["0", "33", "999999", "-1", "x"] {
            let map = announce(&[("x-nv-video[0].videoEncoderSlicesPerFrame", bad)]);
            let cfg = stream_config(&map).expect("session must still negotiate");
            assert_eq!(cfg.slices, 1, "slicesPerFrame {bad} must degrade to 1");
        }
    }

    #[test]
    fn announce_audio_params() {
        assert_eq!(audio_params(&announce(&[])), audio::AudioParams::default());
        let ap = audio_params(&announce(&[
            ("x-nv-audio.surround.numChannels", "6"),
            ("x-nv-audio.surround.channelMask", "63"),
            ("x-nv-audio.surround.AudioQuality", "0"),
            ("x-nv-aqos.packetDuration", "5"),
        ]));
        assert_eq!(
            (ap.channels, ap.high_quality, ap.packet_duration_ms),
            (6, false, 5)
        );
        let ap = audio_params(&announce(&[
            ("x-nv-audio.surround.numChannels", "8"),
            ("x-nv-audio.surround.AudioQuality", "1"),
            ("x-nv-aqos.packetDuration", "10"),
        ]));
        assert_eq!(
            (ap.channels, ap.high_quality, ap.packet_duration_ms),
            (8, true, 10)
        );
        let ap = audio_params(&announce(&[("x-nv-audio.surround.numChannels", "4")]));
        assert_eq!(ap.channels, 2);
    }

    /// Offer ladder. Only `require` sets REQUESTED: a client allowed to
    /// encrypt may decline on a LAN.
    #[test]
    fn encryption_is_offered_but_never_required_in_shipping_modes() {
        assert_eq!(enc_flags(EncOffer::Off), (0, 0));
        assert_eq!(
            enc_flags(EncOffer::VideoOnly),
            (SS_ENC_VIDEO, 0),
            "the granular way out keeps video encryption"
        );
        assert_eq!(
            enc_flags(EncOffer::Supported),
            (SS_ENC_VIDEO | SS_ENC_CONTROL_V2, 0),
            "the default offers both, and requires neither"
        );
        assert_eq!(
            enc_flags(EncOffer::Required),
            (
                SS_ENC_VIDEO | SS_ENC_CONTROL_V2,
                SS_ENC_VIDEO | SS_ENC_CONTROL_V2
            ),
            "the test lever must also REQUEST it, or a client may decline"
        );
        // Never advertise a bit we cannot serve. `SS_ENC_AUDIO` (0x04) especially.
        let servable = SS_ENC_VIDEO | SS_ENC_CONTROL_V2;
        for offer in [
            EncOffer::Off,
            EncOffer::VideoOnly,
            EncOffer::Supported,
            EncOffer::Required,
        ] {
            let (sup, req) = enc_flags(offer);
            assert_eq!(sup & !servable, 0, "no unimplemented bits offered");
            assert_eq!(req & !sup, 0, "never request what isn't supported");
        }
        // Default must offer control-v2; losing the bit would be a silent
        // (key, nonce) regression.
        assert_ne!(
            enc_flags(EncOffer::Supported).0 & SS_ENC_CONTROL_V2,
            0,
            "the default must offer control-v2"
        );
    }

    #[test]
    fn sealed_rtsp_round_trips_and_is_self_distinguishing() {
        let key = [0x5Au8; 16];
        let msg = b"OPTIONS rtsp://x RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let wire = seal_response(&key, msg);
        assert!(wire.len() > ENC_RTSP_HEADER);
        assert!(wire[0] & 0x80 != 0, "sealed frames set the type bit");
        for method in [
            "OPTIONS", "DESCRIBE", "SETUP", "ANNOUNCE", "PLAY", "TEARDOWN",
        ] {
            assert!(
                method.as_bytes()[0] & 0x80 == 0,
                "{method} must not look sealed"
            );
        }
        let type_and_length = u32::from_be_bytes([wire[0], wire[1], wire[2], wire[3]]);
        let len = (type_and_length & !ENCRYPTED_MESSAGE_TYPE_BIT) as usize;
        assert_eq!(
            len,
            wire.len() - ENC_RTSP_HEADER,
            "length covers the payload"
        );
        let seq = u32::from_be_bytes([wire[4], wire[5], wire[6], wire[7]]);
        // Wire order is tag-first; `aes-gcm` wants tag last.
        let mut ct_tag = wire[ENC_RTSP_HEADER..].to_vec();
        ct_tag.extend_from_slice(&wire[8..ENC_RTSP_HEADER]);
        let pt = super::super::control::gcm_open(&key, &rtsp_nonce(seq, b'H'), &ct_tag, &[])
            .expect("host-sealed RTSP must open under the H/R nonce");
        assert_eq!(pt, msg);
        // Direction byte: the client nonce must not open a host message.
        assert!(
            super::super::control::gcm_open(&key, &rtsp_nonce(seq, b'C'), &ct_tag, &[]).is_none(),
            "a host message must not authenticate under the client direction"
        );
    }

    #[test]
    fn sealed_rtsp_request_is_opened_and_parsed() {
        let key = [0xC3u8; 16];
        let msg = b"ANNOUNCE rtsp://x RTSP/1.0\r\nCSeq: 6\r\nContent-length: 7\r\n\r\nv=0\r\na=b";
        let frame = seal_frame(&key, 42, b'C', msg);
        let req = open_sealed_frame(&key, &frame).expect("a client-sealed frame must open");
        assert_eq!(req.method, "ANNOUNCE");
        assert_eq!(req.cseq, "6");
        assert_eq!(req.body, "v=0\r\na=b");

        // Host-direction frame must not open as a client request.
        let host_framed = seal_frame(&key, 42, b'H', msg);
        assert!(
            open_sealed_frame(&key, &host_framed).is_err(),
            "host-direction frame must not authenticate as a client request"
        );
        // Tamper must fail: GCM, unlike the CBC audio path.
        let mut flipped = frame.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 0x01;
        assert!(
            open_sealed_frame(&key, &flipped).is_err(),
            "a flipped ciphertext byte must be rejected, not decrypted"
        );
        assert!(open_sealed_frame(&[0u8; 16], &frame).is_err(), "wrong key");
    }

    /// Host sequence is process-global. A per-connection counter would reuse
    /// (key, nonce) across a session's seven messages.
    #[test]
    fn host_rtsp_sequence_never_repeats() {
        let key = [1u8; 16];
        let seq_of = |w: &[u8]| u32::from_be_bytes([w[4], w[5], w[6], w[7]]);
        let a = seq_of(&seal_response(&key, b"a\r\n\r\n"));
        let b = seal_response(&key, b"b\r\n\r\n");
        let c = seal_response(&key, b"c\r\n\r\n");
        assert!(seq_of(&b) > a, "sequence must advance");
        assert!(seq_of(&c) > seq_of(&b), "sequence must advance");
    }

    /// DESCRIBE SDP: codec indicators and six Opus configs, normal before HQ
    /// per channel count.
    #[test]
    fn describe_advertises_codecs_and_surround() {
        let sdp = describe_sdp();
        // Assert against `enc_flags`, not a literal: `PUNKTFUNK_GS_ENCRYPT`
        // can still steer the offer.
        let (supported, requested) = enc_flags(gs_video_encryption_offer());
        assert!(sdp.contains(&format!("a=x-ss-general.encryptionSupported:{supported}")));
        assert!(sdp.contains(&format!("a=x-ss-general.encryptionRequested:{requested}")));
        assert!(
            sdp.contains("sprop-parameter-sets=AAAAAU"),
            "HEVC indicator"
        );
        assert!(sdp.contains("a=rtpmap:98 AV1/90000"), "AV1 indicator");
        for params in [
            "21101",       // stereo; 2-channel clients hardcode this
            "642012453",   // 5.1 normal, pre-rotated for GFE-order swap
            "660012345",   // 5.1 HQ, verbatim
            "85301245673", // 7.1 normal, pre-rotated over [3, 8)
            "88001234567", // 7.1 HQ, verbatim
        ] {
            assert!(
                sdp.contains(&format!("a=fmtp:97 surround-params={params}")),
                "missing surround-params={params} in:\n{sdp}"
            );
        }
        let n51 = sdp.find("surround-params=642").unwrap();
        let h51 = sdp.find("surround-params=660").unwrap();
        let n71 = sdp.find("surround-params=853").unwrap();
        let h71 = sdp.find("surround-params=880").unwrap();
        assert!(n51 < h51 && n71 < h71);
    }
}
