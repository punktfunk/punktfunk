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
use crate::https::{sha256, spki_p256_point};
use crate::native::link::{CtlReader, CtlRecv, CtlSend, SessionLink, V2Session};
use crate::native::{DataPlane, Served};
use anyhow::{Context, Result};
use punktfunk_core::quic::v2::hello::ClientHello;
use punktfunk_core::quic::v2::io as v2io;
use punktfunk_core::quic::v2::msg::decode;
use punktfunk_core::quic::{auth_signed_message, AuthChallenge, AuthResponse, PairRequest};
use punktfunk_core::reject::RejectReason;
use rand::Rng;
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
) -> Result<Served> {
    let Some(Admitted {
        link,
        mut tx,
        rx,
        first,
        knock,
    }) = admit_session(&conn, &serving).await?
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
            crate::native::park_knock(
                &link,
                Some(&mut tx),
                pairing,
                &label,
                &hex::encode(fp),
                first.profile.as_deref(),
                &sem,
            )
            .await?
            .map_err(rejected)?
        }
    };
    let plane = WebTransportPlane::new(conn, link.v2_session().clone());
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
    rx: CtlReader,
    first: ClientHello,
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
async fn admit_session(conn: &Connection, serving: &Serving) -> Result<Option<Admitted>> {
    const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let (tx, mut rx) = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
        .await
        .context("control stream: handshake timeout")?
        .context("accept control stream")?;
    // The stream says it is control, then carries frames as the native plane's does.
    let ty = tokio::time::timeout(HANDSHAKE_TIMEOUT, v2io::read_stream_type(&mut rx))
        .await
        .context("stream type: handshake timeout")?
        .context("read the stream type")?;
    anyhow::ensure!(
        ty == punktfunk_core::quic::v2::registry::STREAM_CONTROL,
        "first stream is type {ty}, not control"
    );
    let session = Arc::new(V2Session::new());
    let (mut tx, mut rx) = (CtlSend::Web(tx), CtlReader::new(CtlRecv::Web(rx)));
    let (ty, body) = tokio::time::timeout(HANDSHAKE_TIMEOUT, rx.read_frame())
        .await
        .context("first message: handshake timeout")?
        .context("read the first message")?;

    if let Ok(req) = decode::<PairRequest>(ty, &body) {
        pair(conn, tx, rx, req, serving).await?;
        return Ok(None);
    }
    let first = decode::<ClientHello>(ty, &body)
        .map_err(|e| anyhow::anyhow!("ClientHello decode: {e:?}"))?;

    // Nothing is offered until the device answers. The nonce is fresh per connection, so a
    // captured response does not open a second one. Off (`serve --open`), the browser is as
    // anonymous as a native client would be, and the trust record enforces nothing.
    let (device_fp, knock) = if serving.plane.require_pairing {
        let mut nonce = [0u8; 32];
        rand::rng().fill_bytes(&mut nonce);
        // Bounded too: the write waits on the peer's stream credit.
        tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            v2io::send(&mut tx, &AuthChallenge { nonce }),
        )
        .await
        .context("AuthChallenge: handshake timeout")?
        .context("write AuthChallenge")?;
        let (ty, body) = tokio::time::timeout(HANDSHAKE_TIMEOUT, rx.read_frame())
            .await
            .context("AuthResponse: handshake timeout")?
            .context("read AuthResponse")?;
        let auth = decode::<AuthResponse>(ty, &body).map_err(|_| {
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
            None => Some(knock_label(&first, &fp_hex)),
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

/// The console label for an unpaired browser, from its `ClientHello`.
fn knock_label(first: &ClientHello, fp_hex: &str) -> String {
    let name = first.hello.name.as_deref().unwrap_or("");
    crate::native_pairing::sanitize_device_name(name, fp_hex)
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

/// Pair a browser, then end the connection.
///
/// The identities are what makes this the native ceremony rather than a second one: the client's
/// is the SHA-256 of the device key it just sent, the host's is the hash of the certificate this
/// endpoint presented — which the browser had to pin to connect at all, so a man in the middle
/// cannot hold it without the host's private key.
async fn pair(
    conn: &Connection,
    tx: CtlSend,
    rx: CtlReader,
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
        &crate::native::link::SessionLink::Web(conn.clone(), None, Arc::new(V2Session::new())),
        crate::native::PairWire::V2 { send: tx, recv: rx },
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
    use punktfunk_core::quic::v2::hello::ClientHello;
    use punktfunk_core::quic::v2::msg::V2Message;
    use punktfunk_core::quic::Hello;
    use rcgen::{KeyPair, PublicKeyData as _, SigningKey as _, PKCS_ECDSA_P256_SHA256};

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
            host: crate::native::SessionHost::for_tests(pairing, crate::native::test_profiles()),
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

    /// The `ClientHello` frame a browser opens a session with.
    fn hello(name: &str, launch: Option<&str>) -> Vec<u8> {
        use punktfunk_core::config::{CompositorPref, GamepadPref};
        let hello = Hello {
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
        };
        ClientHello {
            hello,
            client_label: None,
            abr_features: 0,
            preset: None,
            link: Default::default(),
            probe_only: false,
            resume: None,
            suites: Vec::new(),
            features: Default::default(),
            profile: None,
        }
        .encode_v2()
    }

    /// One browser dial over loopback WebTransport, up to admission; `first` is the browser's
    /// first control frame. The browser's end is returned too: dropping it would close the
    /// session under the test.
    async fn admit_over_loopback(
        s: &Serving,
        key: &KeyPair,
        first: &[u8],
    ) -> (
        wtransport::Connection,
        punktfunk_core::quic::v2::io::FrameReader<wtransport::RecvStream>,
        Admitted,
    ) {
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
            let (mut tx, rx) = conn.open_bi().await.unwrap().await.unwrap();
            use punktfunk_core::quic::v2::{io, msg, registry};
            io::write_stream_type(&mut tx, registry::STREAM_CONTROL)
                .await
                .unwrap();
            let mut rx = io::FrameReader::new(rx);
            tx.write_all(first).await.unwrap();
            let (ty, body) = rx.read_frame().await.unwrap();
            let nonce = msg::decode::<AuthChallenge>(ty, &body).unwrap().nonce;
            io::send(&mut tx, &respond(key, &s.cert_hash, &nonce))
                .await
                .unwrap();
            (conn, rx)
        };
        let host = async {
            let conn = server.accept().await.await.unwrap().accept().await.unwrap();
            admit_session(&conn, s)
                .await
                .unwrap()
                .expect("a session, not a pairing")
        };
        let ((conn, rx), admitted) = tokio::join!(browser, host);
        (conn, rx, admitted)
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

        let first = hello("Safari on Mac", None);
        let (_browser, _rx, admitted) = admit_over_loopback(&s, &key, &first).await;
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
        let first = hello("Safari on Mac", None);
        let (_browser, _rx, admitted) = admit_over_loopback(&s, &key, &first).await;
        let label = admitted.knock.expect("an unpaired device knocks");
        assert_eq!(label, "Safari on Mac");

        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let park =
            crate::native::park_knock(&admitted.link, None, &np, &label, &fp_hex, None, &sem);
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

    /// A browser's close code reaches the session: `QUIT_CLOSE_CODE` is how a player ends the
    /// title rather than leaving it running.
    #[tokio::test]
    async fn a_browser_link_reports_the_close_code() {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let s = serving(store("closed"));
        let fp_hex = hex::encode(sha256(&key.subject_public_key_info()));
        s.plane.pairing.add("Enrico's browser", &fp_hex).unwrap();
        let first = hello("Safari on Mac", None);
        let (browser, _rx, admitted) = admit_over_loopback(&s, &key, &first).await;
        let quit = punktfunk_core::quic::QUIT_CLOSE_CODE;
        browser.close(wtransport::VarInt::from_u32(quit), b"");
        let closed =
            tokio::time::timeout(std::time::Duration::from_secs(5), admitted.link.closed())
                .await
                .expect("the close reaches the host");
        assert!(closed.closed_with(quit), "{closed}");
    }

    /// A browser reads why the host refused it: WebKit shows a page nothing of a close reason, so
    /// the typed refusal rides its own stream, as the `Refused` frame. Here the device may not
    /// launch titles, and asks to.
    #[tokio::test]
    async fn a_browser_reads_why_its_launch_was_refused() {
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
        let first = hello("Safari on Mac", Some("steam:570"));
        let (browser, _rx, admitted) = admit_over_loopback(&s, &key, &first).await;
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
            link.v2_session().clone(),
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

    /// A `Redirect` reaches the browser before any `ServerHello`, and the host then closes.
    #[tokio::test]
    async fn a_redirect_reaches_the_client_before_the_server_hello() {
        use punktfunk_core::quic::v2::msg::{Redirect, V2Message};
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let s = serving(store("redirect"));
        let fp_hex = hex::encode(sha256(&key.subject_public_key_info()));
        s.plane.pairing.add("Enrico's browser", &fp_hex).unwrap();
        let first = hello("Safari on Mac", None);
        let (browser, mut rx, admitted) = admit_over_loopback(&s, &key, &first).await;
        let Admitted { link, mut tx, .. } = admitted;
        let to = Redirect {
            addr: "192.168.1.20".into(),
            port: 9778,
            profile: "9a3f1c2b7e40".into(),
            seat_no: 1,
            seat_name: "Seat 1".into(),
            occupant: String::new(),
            pin: String::new(),
        };
        let host = crate::native::redirect(&link, &mut tx, &to);
        let read = async {
            let (ty, body) = rx.read_frame().await.unwrap();
            assert_eq!(ty, Redirect::TYPE);
            let got = Redirect::from_body(&body).unwrap();
            browser.close(wtransport::VarInt::from_u32(0), b"");
            got
        };
        let (sent, got) = tokio::join!(host, read);
        sent.unwrap();
        assert_eq!(got, to);
    }
}
