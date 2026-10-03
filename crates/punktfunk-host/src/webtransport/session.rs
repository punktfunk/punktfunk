//! One browser session: device-key admission on a WebTransport stream, then the native session.
//!
//! **Identity, with no client certificate.** The browser speaks first — a stream it opens does
//! not reach the host until it writes on it, so a host that greeted first would wait for a client
//! that was waiting for it. A first-time browser sends a `PairRequest` carrying its WebCrypto
//! device key; any other opens with `Hello`, and a host that requires pairing answers that with
//! an [`AuthChallenge`] rather than a `Welcome`. The signature that comes back is looked up in
//! the same store native clients live in. What mTLS does per packet, this does once.
//!
//! Admitted, the browser is a native client: the same `Hello` bytes, the same control stream
//! (as a [`crate::native::link::CtlSend`] pair) and the same pipeline run through
//! [`crate::native::run_admitted`], with video on a [`WebTransportPlane`] instead of a UDP
//! socket. Nothing below admission is browser-specific, and a second copy of the session is the
//! thing this file must never grow back into.

use super::{Serving, WebTransportPlane};
use crate::native::link::{CtlRecv, CtlSend, SessionLink, V2Session};
use crate::native::{DataPlane, Served};
use anyhow::{Context, Result};
use punktfunk_core::quic::io::{read_msg, write_msg};
use punktfunk_core::quic::{auth_signed_message, AuthChallenge, AuthResponse, Hello, PairRequest};
use punktfunk_core::reject::RejectReason;
use rand::RngCore;
use std::sync::Arc;
use wtransport::Connection;

/// A refusal with the close code the native plane would use for it. Anything that is not one
/// of these closes as [`punktfunk_core::reject::SETUP_FAILED_CLOSE_CODE`].
#[derive(Debug)]
pub(crate) struct Refusal {
    pub(crate) code: u32,
    pub(crate) what: String,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.what)
    }
}

impl std::error::Error for Refusal {}

fn refused(code: u32, what: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Refusal {
        code,
        what: what.into(),
    })
}

fn rejected(reason: RejectReason) -> anyhow::Error {
    refused(reason.close_code(), reason.to_string())
}

/// Admit the browser, then run the native session on its connection.
///
/// `sem` is the pool the native plane draws from: a browser holds a session slot, not a
/// browser slot. The slot is taken once admission returns, so a peer that stalls before its
/// device signature holds none. An unpaired device takes one only on approval. A full host
/// waits here with the session accepted; the plane's keep-alive holds the path meanwhile.
pub(crate) async fn run(
    conn: Connection,
    serving: Arc<Serving>,
    sem: Arc<tokio::sync::Semaphore>,
    v2: bool,
) -> Result<Served> {
    let Some(Admitted {
        link,
        tx,
        rx,
        first,
        knock,
    }) = admit_session(&conn, &serving, v2).await?
    else {
        return Ok(Served::Session);
    };
    let permit = match knock {
        None => sem
            .acquire_owned()
            .await
            .expect("session semaphore is never closed"),
        Some(label) => {
            let fp = link
                .peer_fingerprint()
                .context("a knock is keyed by its device")?;
            let pairing = &serving.plane.pairing;
            crate::native::park_knock(&link, None, pairing, &label, &hex::encode(fp), &sem)
                .await?
                .map_err(rejected)?
        }
    };
    let plane = WebTransportPlane::new(conn, link.v2_session().cloned());
    crate::native::run_admitted(
        link,
        tx,
        rx,
        first,
        &serving.host,
        DataPlane::Web(plane),
        permit,
    )
    .await
}

/// A browser past admission: its link, and the control stream with the first message read.
struct Admitted {
    link: SessionLink,
    tx: CtlSend,
    rx: CtlRecv,
    first: Vec<u8>,
    /// The name it asks for access under, when the host does not admit this device yet.
    knock: Option<String>,
}

/// The control stream, its first message, and the device behind it.
///
/// The link comes back keyed by the fingerprint the device signature proved, so the session's
/// per-device decisions — mode cap, game re-adopt, its own stale session — see a browser as they
/// see a native client. A device the host does not admit, never paired or expired, comes back as
/// a knock. `None` once a `PairRequest` has run: pairing is its own connection, as on the native
/// plane, so a browser reconnects to stream.
async fn admit_session(conn: &Connection, serving: &Serving, v2: bool) -> Result<Option<Admitted>> {
    const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let (tx, mut rx) = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
        .await
        .context("control stream: handshake timeout")?
        .context("accept control stream")?;
    // `/pf2`: the stream says it is control, then every message crosses the translation
    // edges, so admission below reads and writes `punktfunk/1` as it always has.
    let session = if v2 {
        use punktfunk_core::quic::v2::{io as v2io, registry};
        let ty = tokio::time::timeout(HANDSHAKE_TIMEOUT, v2io::read_stream_type(&mut rx))
            .await
            .context("stream type: handshake timeout")?
            .context("read the stream type")?;
        anyhow::ensure!(
            ty == registry::STREAM_CONTROL,
            "first stream is type {ty}, not control"
        );
        Some(Arc::new(V2Session::new()))
    } else {
        None
    };
    let (mut tx, mut rx) = match &session {
        Some(s) => {
            use punktfunk_core::quic::v2::io::{V2Reader, V2Writer};
            (
                CtlSend::WebV2(V2Writer::new(tx, s.tx.clone())),
                CtlRecv::WebV2(V2Reader::new(rx, s.rx.clone())),
            )
        }
        None => (CtlSend::Web(tx), CtlRecv::Web(rx)),
    };
    let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_msg(&mut rx))
        .await
        .context("first message: handshake timeout")?
        .context("read the first message")?;

    if let Ok(req) = PairRequest::decode(&first) {
        pair(conn, tx, rx, req, serving).await?;
        return Ok(None);
    }

    // Nothing is offered until the device answers. The nonce is fresh per connection, so a
    // captured response does not open a second one. Off (`serve --open`), the browser is as
    // anonymous as a native client would be, and the trust record enforces nothing.
    let (device_fp, knock) = if serving.plane.require_pairing {
        let mut nonce = [0u8; 32];
        rand::rng().fill_bytes(&mut nonce);
        // Bounded too: the write waits on the peer's stream credit.
        tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            write_msg(&mut tx, &AuthChallenge { nonce }.encode()),
        )
        .await
        .context("AuthChallenge: handshake timeout")?
        .context("write AuthChallenge")?;
        let answer = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_msg(&mut rx))
            .await
            .context("AuthResponse: handshake timeout")?
            .context("read AuthResponse")?;
        let auth = AuthResponse::decode(&answer).map_err(|_| {
            refused(
                punktfunk_core::reject::PAIR_NO_IDENTITY_CLOSE_CODE,
                "this host requires pairing — no device signature",
            )
        })?;
        let fp = verify(&auth, &nonce, serving)?;
        let fp_hex = hex::encode(fp);
        // `effective`, not `list`: an expired record knocks like an unpaired device, and
        // re-approval is the re-grant.
        let now = crate::clock::unix_secs();
        let knock = match serving.plane.pairing.effective(&fp_hex, now) {
            Some(_) => {
                tracing::info!(fingerprint = %fp_hex, "browser authenticated");
                None
            }
            None => Some(knock_label(&first, &fp_hex)?),
        };
        (Some(fp), knock)
    } else {
        (None, None)
    };
    Ok(Some(Admitted {
        link: SessionLink::Web(conn.clone(), device_fp, session),
        tx,
        rx,
        first,
        knock,
    }))
}

/// The console label for an unpaired browser, from its `Hello`. A client on another wire version
/// is refused here, before anyone is asked to approve a session it could not run.
fn knock_label(first: &[u8], fp_hex: &str) -> Result<String> {
    let hello = Hello::decode(first).map_err(|e| anyhow::anyhow!("Hello decode: {e:?}"))?;
    if hello.abi_version != punktfunk_core::WIRE_VERSION {
        return Err(rejected(RejectReason::WireVersionMismatch));
    }
    let name = hello.name.as_deref().unwrap_or("");
    Ok(crate::native_pairing::sanitize_device_name(name, fp_hex))
}

/// Does the device hold the key it names, signed over this nonce on this channel? Returns the
/// fingerprint the session is keyed by.
///
/// Checked before the pairing store is asked anything. A fingerprint is a public value anyone
/// can replay, so an unproven one may neither stream nor knock under a paired device's name.
fn verify(auth: &AuthResponse, nonce: &[u8; 32], serving: &Serving) -> Result<[u8; 32]> {
    let msg = auth_signed_message(&serving.cert_hash, nonce);
    aws_lc_rs::signature::UnparsedPublicKey::new(
        &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
        spki_p256_point(&auth.device_key).context("device key is not a P-256 SPKI")?,
    )
    .verify(&msg, &auth.signature)
    .map_err(|_| anyhow::anyhow!("the device signature does not verify"))?;
    Ok(sha256(&auth.device_key))
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

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(bytes).into()
}

/// Pair a browser, then end the connection.
///
/// The identities are what makes this the native ceremony rather than a second one: the client's
/// is the SHA-256 of the device key it just sent, the host's is the hash of the certificate this
/// endpoint presented — which the browser had to pin to connect at all, so a man in the middle
/// cannot hold it without the host's private key.
async fn pair(
    conn: &Connection,
    tx: CtlSend,
    rx: CtlRecv,
    req: PairRequest,
    serving: &Serving,
) -> Result<()> {
    anyhow::ensure!(
        spki_p256_point(&req.device_key).is_some(),
        "a browser must pair with a P-256 device key"
    );
    // Charged before arming is consulted, on every outcome: otherwise this plane answers "is
    // pairing armed?" for free, to anyone. Knocks can hold it against the real device, which is
    // the trade the native plane already makes.
    {
        let mut last = serving.last_pairing.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < crate::native::PAIRING_COOLDOWN) {
            return Err(refused(
                punktfunk_core::reject::PAIR_RATE_LIMITED_CLOSE_CODE,
                "pairing rate-limited — retry shortly",
            ));
        }
        *last = Some(std::time::Instant::now());
    }
    let client_fp = sha256(&req.device_key);
    let source = crate::native_pairing::classify_source(Some(conn.remote_address().ip()));
    let pin = match serving
        .plane
        .pairing
        .pin_for_attempt(&hex::encode(client_fp), source)
    {
        crate::native_pairing::PinAttempt::Pin(pin) => pin,
        crate::native_pairing::PinAttempt::Disarmed => {
            return Err(refused(
                punktfunk_core::reject::PAIR_NOT_ARMED_CLOSE_CODE,
                "pairing is not armed — arm it in the console, then retry",
            ))
        }
        crate::native_pairing::PinAttempt::BoundToOther => {
            return Err(refused(
                punktfunk_core::reject::PAIR_BOUND_OTHER_CLOSE_CODE,
                "pairing is armed for a different device",
            ))
        }
        // Same rule as the native plane: an open window is for the device in the operator's
        // hands, so a browser knocking from the internet reads as not armed.
        crate::native_pairing::PinAttempt::UnboundForWan => {
            return Err(refused(
                punktfunk_core::reject::PAIR_NOT_ARMED_CLOSE_CODE,
                "pairing is not armed for this device — arm a PIN bound to its fingerprint",
            ))
        }
    };
    crate::native::pair_ceremony(
        &crate::native::link::SessionLink::Web(conn.clone(), None, None),
        tx,
        rx,
        req,
        &client_fp,
        &serving.cert_hash,
        &serving.plane.pairing,
        &pin,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::auth_signed_message;
    use rcgen::{
        KeyPair, PublicKeyData as _, SigningKey as _, PKCS_ECDSA_P256_SHA256,
        PKCS_ECDSA_P384_SHA384,
    };

    fn store(tag: &str) -> Arc<crate::native_pairing::NativePairing> {
        let path =
            std::env::temp_dir().join(format!("pf-wt-auth-{tag}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Arc::new(crate::native_pairing::NativePairing::load_with(Some(path), None, false).unwrap())
    }

    fn serving(pairing: Arc<crate::native_pairing::NativePairing>) -> Serving {
        Serving {
            plane: crate::webtransport::Plane {
                bind: "127.0.0.1:9778".parse().unwrap(),
                sans: Vec::new(),
                origins: Vec::new(),
                identity: crate::identity::ephemeral().unwrap(),
                pairing: pairing.clone(),
                require_pairing: true,
            },
            cert_hash: [0x11; 32],
            last_pairing: std::sync::Mutex::new(None),
            host: crate::native::SessionHost::for_tests(pairing),
        }
    }

    fn respond(key: &KeyPair, binding: &[u8; 32], nonce: &[u8; 32]) -> AuthResponse {
        let device_key = key.subject_public_key_info();
        let signature = key.sign(&auth_signed_message(binding, nonce)).unwrap();
        AuthResponse {
            device_key,
            signature,
        }
    }

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

    /// A signature proves the key only over *this* nonce on *this* connection, and yields that
    /// key's own fingerprint: a second key cannot claim someone else's.
    #[test]
    fn a_device_proves_its_key_over_this_nonce_on_this_channel() {
        let s = serving(store("verify"));
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let nonce = [0x77u8; 32];
        let auth = respond(&key, &s.cert_hash, &nonce);
        assert_eq!(
            verify(&auth, &nonce, &s).unwrap(),
            sha256(&key.subject_public_key_info()),
            "the session is keyed by the device fingerprint"
        );

        assert!(verify(&auth, &[0x78; 32], &s).is_err(), "replayed nonce");
        let mut elsewhere = serving(store("verify-elsewhere"));
        elsewhere.cert_hash = [0x22; 32];
        assert!(
            verify(&auth, &nonce, &elsewhere).is_err(),
            "a response captured on one connection must not open another"
        );

        // Another key's signature proves that key, never this one's fingerprint.
        let other = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut claimed = respond(&other, &s.cert_hash, &nonce);
        claimed.device_key = auth.device_key.clone();
        assert!(verify(&claimed, &nonce, &s).is_err());

        let mut bent = auth.clone();
        let last = bent.signature.len() - 1;
        bent.signature[last] ^= 0xff;
        assert!(verify(&bent, &nonce, &s).is_err());
    }

    /// The `Hello` a browser opens a session with.
    fn hello(name: &str, launch: Option<&str>) -> Hello {
        use punktfunk_core::config::{CompositorPref, GamepadPref};
        Hello {
            abi_version: punktfunk_core::WIRE_VERSION,
            mode: punktfunk_core::Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            compositor: CompositorPref::Auto,
            gamepad: GamepadPref::Auto,
            bitrate_kbps: 0,
            name: Some(name.into()),
            launch: launch.map(Into::into),
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: 0,
            audio_rate_hz: punktfunk_core::audio::SAMPLE_RATE_HZ,
            audio_bits: punktfunk_core::audio::pcm::BITS_16,
            audio_layout: 0,
            video_fit: 0,
        }
    }

    /// One browser dial over loopback WebTransport, up to admission. The browser's end is
    /// returned too: dropping it would close the session under the test.
    async fn admit_over_loopback(
        s: &Serving,
        key: &KeyPair,
        first: &[u8],
    ) -> (wtransport::Connection, Admitted) {
        admit_over_loopback_on(s, key, first, false).await
    }

    /// [`admit_over_loopback`]; on `/pf2` (`v2`) the browser's control stream crosses the
    /// client's translation edges, as the client pump's does.
    async fn admit_over_loopback_on(
        s: &Serving,
        key: &KeyPair,
        first: &[u8],
        v2: bool,
    ) -> (wtransport::Connection, Admitted) {
        let identity = wtransport::Identity::self_signed(["localhost"]).unwrap();
        let cert = identity.certificate_chain().as_slice()[0].hash();
        let loopback: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let server = wtransport::Endpoint::server(
            wtransport::ServerConfig::builder()
                .with_bind_address(loopback)
                .with_identity(identity)
                .build(),
        )
        .unwrap();
        let url = format!("https://127.0.0.1:{}/", server.local_addr().unwrap().port());
        let browser = async {
            let conn = wtransport::Endpoint::client(
                wtransport::ClientConfig::builder()
                    .with_bind_address(loopback)
                    .with_server_certificate_hashes([cert])
                    .build(),
            )
            .unwrap()
            .connect(url)
            .await
            .unwrap();
            let (tx, rx) = conn.open_bi().await.unwrap().await.unwrap();
            type W = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;
            type R = Box<dyn tokio::io::AsyncRead + Send + Unpin>;
            let (mut tx, mut rx): (W, R) = if v2 {
                use punktfunk_core::quic::v2::{io, registry, translate};
                let mut tx = tx;
                io::write_stream_type(&mut tx, registry::STREAM_CONTROL)
                    .await
                    .unwrap();
                let edge = translate::TxEdge::client(translate::ClientExtra::default());
                (
                    Box::new(io::V2Writer::new(tx, Arc::new(std::sync::Mutex::new(edge)))),
                    Box::new(io::V2Reader::new(rx, Default::default())),
                )
            } else {
                (Box::new(tx), Box::new(rx))
            };
            write_msg(&mut tx, first).await.unwrap();
            let nonce = AuthChallenge::decode(&read_msg(&mut rx).await.unwrap())
                .unwrap()
                .nonce;
            let answer = respond(key, &s.cert_hash, &nonce).encode();
            write_msg(&mut tx, &answer).await.unwrap();
            conn
        };
        let host = async {
            let conn = server.accept().await.await.unwrap().accept().await.unwrap();
            admit_session(&conn, s, v2)
                .await
                .unwrap()
                .expect("a session, not a pairing")
        };
        tokio::join!(browser, host)
    }

    /// An admitted browser is its device for the rest of the session, as a native client is its
    /// certificate. Over a real WebTransport connection, because the link is what carries it.
    #[tokio::test]
    async fn an_admitted_browser_is_keyed_by_its_device() {
        let np = store("keyed");
        let s = serving(np.clone());
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let fp = sha256(&key.subject_public_key_info());
        np.add("Enrico's browser", &hex::encode(fp)).unwrap();

        // A paired device's first message is not read here: the session decodes its `Hello`.
        let (_browser, admitted) = admit_over_loopback(&s, &key, b"hello").await;
        assert!(admitted.link.is_web());
        assert_eq!(admitted.link.peer_fingerprint(), Some(fp));
        assert!(admitted.knock.is_none(), "a paired device streams at once");
    }

    /// An unpaired browser asks for access under its `Hello` name, waits without a session slot,
    /// and the console's approval admits this same connection.
    #[tokio::test]
    async fn an_unpaired_browser_knocks_and_approval_admits_it() {
        let np = store("knock");
        let s = serving(np.clone());
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let fp_hex = hex::encode(sha256(&key.subject_public_key_info()));
        let first = hello("Safari on Mac", None).encode();
        let (_browser, admitted) = admit_over_loopback(&s, &key, &first).await;
        let label = admitted.knock.expect("an unpaired device knocks");
        assert_eq!(label, "Safari on Mac");

        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let park = crate::native::park_knock(&admitted.link, None, &np, &label, &fp_hex, &sem);
        let console = async {
            let pending = loop {
                if let Some(p) = np.pending().into_iter().find(|p| p.fingerprint == fp_hex) {
                    break p;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            };
            assert_eq!(pending.name, "Safari on Mac");
            assert_eq!(sem.available_permits(), 1, "a parked knock holds no slot");
            np.approve_pending(pending.id, None, None).unwrap();
        };
        let (parked, ()) = tokio::join!(park, console);
        let _slot = parked
            .unwrap()
            .expect("approval admits the parked connection");
        assert_eq!(
            sem.available_permits(),
            0,
            "the admitted session holds its slot again"
        );
    }

    /// A browser reads why the host refused it: WebKit shows a page nothing of a close reason, so
    /// the typed refusal rides a stream. Here the device may not launch titles, and asks to.
    #[tokio::test]
    async fn a_browser_reads_why_its_launch_was_refused() {
        use punktfunk_core::quic::{Refused, GRANT_ALL, GRANT_LAUNCH};
        let np = store("refused");
        let s = serving(np.clone());
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let no_launch = crate::native_pairing::Access {
            grants: GRANT_ALL & !GRANT_LAUNCH,
            expires_unix: None,
            until_disconnect: false,
        };
        let fp_hex = hex::encode(sha256(&key.subject_public_key_info()));
        np.add_with_access("Enrico's browser", &fp_hex, Some(no_launch))
            .unwrap();

        let first = hello("Safari on Mac", Some("steam:570")).encode();
        let (browser, admitted) = admit_over_loopback(&s, &key, &first).await;
        let Admitted {
            link,
            tx,
            rx,
            first,
            ..
        } = admitted;
        let SessionLink::Web(host, ..) = &link else {
            unreachable!("a browser's link")
        };
        let plane = DataPlane::Web(WebTransportPlane::new(host.clone(), None));
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = sem.try_acquire_owned().unwrap();
        let session =
            crate::native::run_admitted(link.clone(), tx, rx, first, &s.host, plane, permit);
        let read = async {
            let mut uni = browser.accept_uni().await.unwrap();
            Refused::decode(&read_msg(&mut uni).await.unwrap()).unwrap()
        };
        let (ended, said) = tokio::join!(session, read);
        assert!(ended.is_err(), "the session is refused");
        let why = RejectReason::LaunchNotPermitted;
        assert_eq!(said.code, why.close_code());
        assert_eq!(said.reason, why.to_string());
    }

    /// A browser's close code reaches the session: `QUIT_CLOSE_CODE` is how a player ends the
    /// title rather than leaving it running.
    #[tokio::test]
    async fn a_browser_link_reports_the_close_code() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let s = serving(store("closed"));
        let fp_hex = hex::encode(sha256(&key.subject_public_key_info()));
        s.plane.pairing.add("Enrico's browser", &fp_hex).unwrap();
        let (browser, admitted) = admit_over_loopback(&s, &key, b"hello").await;
        let quit = punktfunk_core::quic::QUIT_CLOSE_CODE;
        browser.close(wtransport::VarInt::from_u32(quit), b"");
        let closed =
            tokio::time::timeout(std::time::Duration::from_secs(5), admitted.link.closed())
                .await
                .expect("the close reaches the host");
        assert!(closed.closed_with(quit), "{closed}");
    }

    /// A browser on `/pf2` is admitted through the translation edges: the host reads the same
    /// `Hello` it would on `punktfunk/1`, and the link speaks `punktfunk/2`.
    #[tokio::test]
    async fn a_browser_on_pf2_is_admitted_through_the_edges() {
        let np = store("pf2-keyed");
        let s = serving(np.clone());
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let fp_hex = hex::encode(sha256(&key.subject_public_key_info()));
        np.add("Enrico's browser", &fp_hex).unwrap();
        let first = hello("Safari on Mac", None).encode();
        let (_browser, admitted) = admit_over_loopback_on(&s, &key, &first, true).await;
        assert_eq!(admitted.link.wire(), 2);
        assert!(admitted.link.v2_session().is_some());
        assert_eq!(
            Hello::decode(&admitted.first).unwrap().name.as_deref(),
            Some("Safari on Mac")
        );
        assert!(admitted.knock.is_none(), "a paired device streams at once");
    }

    /// On `/pf2` a refusal still reaches the page on its own stream, as the `Refused` frame.
    #[tokio::test]
    async fn a_browser_on_pf2_reads_why_its_launch_was_refused() {
        use punktfunk_core::quic::v2::msg::V2Message;
        use punktfunk_core::quic::{Refused, GRANT_ALL, GRANT_LAUNCH};
        let np = store("pf2-refused");
        let s = serving(np.clone());
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let no_launch = crate::native_pairing::Access {
            grants: GRANT_ALL & !GRANT_LAUNCH,
            expires_unix: None,
            until_disconnect: false,
        };
        let fp_hex = hex::encode(sha256(&key.subject_public_key_info()));
        np.add_with_access("Enrico's browser", &fp_hex, Some(no_launch))
            .unwrap();
        let first = hello("Safari on Mac", Some("steam:570")).encode();
        let (browser, admitted) = admit_over_loopback_on(&s, &key, &first, true).await;
        let Admitted {
            link,
            tx,
            rx,
            first,
            ..
        } = admitted;
        let SessionLink::Web(host, ..) = &link else {
            unreachable!("a browser's link")
        };
        let plane = DataPlane::Web(WebTransportPlane::new(
            host.clone(),
            link.v2_session().cloned(),
        ));
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = sem.try_acquire_owned().unwrap();
        let session =
            crate::native::run_admitted(link.clone(), tx, rx, first, &s.host, plane, permit);
        let read = async {
            let mut uni = browser.accept_uni().await.unwrap();
            let (ty, body) = punktfunk_core::quic::v2::io::read_one_frame(&mut uni)
                .await
                .unwrap();
            assert_eq!(ty, Refused::TYPE);
            Refused::from_body(&body).unwrap()
        };
        let (ended, said) = tokio::join!(session, read);
        assert!(ended.is_err(), "the session is refused");
        let why = RejectReason::LaunchNotPermitted;
        assert_eq!(said.code, why.close_code());
        assert_eq!(said.reason, why.to_string());
    }
}
