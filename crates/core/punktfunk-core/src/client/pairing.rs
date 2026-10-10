//! The two ways a client gets paired: the PIN ceremony (SPAKE2, `NativeClient::pair`) and a
//! request the host's operator approves (`NativeClient::request_access`). Neither streams.

use super::worker::reject_from_close;
use super::{dial_addr, NativeClient};
use crate::error::{PunktfunkError, Result};
use crate::quic::endpoint;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

impl NativeClient {
    /// Pair over TOFU QUIC: the PIN, not the handshake, authenticates the certs.
    /// Returns the host fingerprint to pin. Pass the same PEM identity later to
    /// [`NativeClient::connect`]. The host stores `name` against this client.
    pub fn pair(
        host: &str,
        port: u16,
        identity: (&str, &str),
        pin: &str,
        name: &str,
        timeout: Duration,
    ) -> Result<[u8; 32]> {
        use crate::quic::{pake, PairChallenge, PairProof, PairRequest, PairResult};

        let client_fp = endpoint::fingerprint_of_pem(identity.0)
            .map_err(|_| PunktfunkError::InvalidArg("client cert pem"))?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(PunktfunkError::Io)?;
        let pin = pin.to_string();
        let name = name.to_string();

        rt.block_on(async move {
            let remote = dial_addr(host, port).await?;
            // quinn's driver is spawned on the current runtime.
            let (ep, observed) = endpoint::client_pinned_with_identity(None, Some(identity));
            let ep = ep.map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;

            // Never close here; the caller does, then flushes, so an early
            // return still lets the host see CONNECTION_CLOSE.
            let exchange = |conn: quinn::Connection, host_fp: [u8; 32]| async move {
                use crate::quic::v2::{io as v2io, msg::decode, registry};
                let (mut send, recv) = conn
                    .open_bi()
                    .await
                    .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
                v2io::write_stream_type(&mut send, registry::STREAM_CONTROL).await?;
                let mut recv = v2io::FrameReader::new(recv);
                // SPAKE2 as A; bind our fingerprint and the TOFU-observed host cert.
                let (pake, spake_a) = pake::start(true, &pin, &client_fp, &host_fp);
                // No `device_key`: this client's identity is its certificate, which the
                // transport re-proves on every connection.
                let req = PairRequest {
                    name,
                    spake_a,
                    device_key: Vec::new(),
                };
                v2io::send(&mut send, &req).await?;
                let (ty, body) = recv.read_frame().await?;
                let challenge = decode::<PairChallenge>(ty, &body)?;
                let confirms = pake.finish(&challenge.spake_b)?;
                // Host confirm = same key (PIN + certs). Pin only after this.
                if !pake::verify(&confirms.host, &challenge.confirm) {
                    return Err(PunktfunkError::Crypto); // wrong PIN or MITM
                }
                let proof = PairProof {
                    confirm: confirms.client,
                };
                v2io::send(&mut send, &proof).await?;
                let (ty, body) = recv.read_frame().await?;
                let result = decode::<PairResult>(ty, &body)?;
                if result.ok {
                    Ok(host_fp)
                } else {
                    Err(PunktfunkError::Crypto) // host rejected post-confirm
                }
            };

            let ceremony = async {
                let conn = ep
                    .connect(remote, "punktfunk")
                    .map_err(|_| PunktfunkError::InvalidArg("connect"))?
                    .await
                    .map_err(|e| match endpoint::refused_alpn(&e) {
                        true => PunktfunkError::Rejected(
                            crate::reject::RejectReason::WireVersionMismatch,
                        ),
                        false => PunktfunkError::Io(std::io::Error::other(e.to_string())),
                    })?;
                let host_fp = observed.lock().unwrap().ok_or(PunktfunkError::Crypto)?;
                let outcome = match exchange(conn.clone(), host_fp).await {
                    // Prefer a typed host close (not armed / wrong device / rate-limit)
                    // over the transport error from the aborted stream. Same race as
                    // connect: 300 ms for CONNECTION_CLOSE to land.
                    Err(e) => {
                        if conn.close_reason().is_none() {
                            let _ = tokio::time::timeout(
                                std::time::Duration::from_millis(300),
                                conn.closed(),
                            )
                            .await;
                        }
                        Err(match reject_from_close(&conn) {
                            Some((r, _)) => PunktfunkError::Rejected(r),
                            None => e,
                        })
                    }
                    ok => ok,
                };
                // Close so the host unblocks its read: 0 = ok, 1 = refused/aborted.
                let code: u32 = if outcome.is_ok() { 0 } else { 1 };
                conn.close(code.into(), b"pair done");
                outcome
            };
            let outcome = tokio::time::timeout(timeout, ceremony)
                .await
                .map_err(|_| PunktfunkError::Timeout)?;
            // Drain CONNECTION_CLOSE before dropping the runtime; else the host
            // waits the full pairing timeout. 2 s is enough for a local flush.
            let _ = tokio::time::timeout(Duration::from_secs(2), ep.wait_idle()).await;
            outcome
        })
    }

    /// Ask for access without a PIN: the host parks the request until its operator approves
    /// this device, then answers. Returns the host fingerprint to pin as paired, and never
    /// starts a stream. An older host admits a session instead; that admission is the
    /// approval, and the session is closed before it streams.
    ///
    /// `pin` is the advertised fingerprint; `None` trusts on first use. Setting `cancel`
    /// withdraws the request.
    pub fn request_access(
        host: &str,
        port: u16,
        identity: (&str, &str),
        pin: Option<[u8; 32]>,
        name: &str,
        timeout: Duration,
        cancel: Option<Arc<AtomicBool>>,
    ) -> Result<[u8; 32]> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(PunktfunkError::Io)?;
        rt.block_on(async move {
            let remote = dial_addr(host, port).await?;
            let (ep, observed) = endpoint::client_pinned_with_identity(pin, Some(identity));
            let ep = ep.map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
            let knock = async {
                let conn = ep
                    .connect(remote, "punktfunk")
                    .map_err(|_| PunktfunkError::InvalidArg("connect"))?
                    .await
                    .map_err(|e| {
                        let seen = *observed.lock().unwrap();
                        if pin.is_some() && seen.is_some() && seen != pin {
                            PunktfunkError::Crypto
                        } else if endpoint::refused_alpn(&e) {
                            PunktfunkError::Rejected(
                                crate::reject::RejectReason::WireVersionMismatch,
                            )
                        } else {
                            PunktfunkError::Io(std::io::Error::other(e.to_string()))
                        }
                    })?;
                let host_fp = observed.lock().unwrap().ok_or(PunktfunkError::Crypto)?;
                match knock_once(&conn, name).await {
                    Ok(()) => {
                        conn.close(crate::quic::QUIT_CLOSE_CODE.into(), b"access only");
                        Ok(host_fp)
                    }
                    Err(e) => {
                        if conn.close_reason().is_none() {
                            let _ = tokio::time::timeout(
                                Duration::from_millis(300),
                                conn.closed(),
                            )
                            .await;
                        }
                        if access_granted(&conn) {
                            return Ok(host_fp);
                        }
                        Err(match reject_from_close(&conn) {
                            Some((r, _)) => PunktfunkError::Rejected(r),
                            None => e,
                        })
                    }
                }
            };
            let cancelled = async {
                while !cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            };
            let outcome = tokio::select! {
                r = tokio::time::timeout(timeout, knock) => r.unwrap_or(Err(PunktfunkError::Timeout)),
                () = cancelled => Err(PunktfunkError::Timeout),
            };
            // The host withdraws a parked request only when its connection closes.
            ep.close(crate::quic::QUIT_CLOSE_CODE.into(), b"access request done");
            let _ = tokio::time::timeout(Duration::from_secs(2), ep.wait_idle()).await;
            outcome
        })
    }
}

/// Send an access-only hello and wait. `Ok` is an older host's `ServerHello`; a host that
/// knows the field answers by closing, which surfaces here as the read error.
async fn knock_once(conn: &quinn::Connection, name: &str) -> Result<()> {
    use crate::quic::v2::hello::{ClientHello, ServerHello};
    use crate::quic::v2::{io as v2io, msg::V2Message, registry};
    let (mut send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| PunktfunkError::Io(std::io::Error::other(e.to_string())))?;
    v2io::write_stream_type(&mut send, registry::STREAM_CONTROL).await?;
    let mut recv = v2io::FrameReader::new(recv);
    // A mode, codec and suite an older host can admit, so it reaches its `ServerHello`.
    let hello = ClientHello {
        hello: crate::quic::Hello {
            mode: crate::config::Mode {
                width: 1280,
                height: 720,
                refresh_hz: 60,
            },
            compositor: crate::config::CompositorPref::Auto,
            gamepad: crate::config::GamepadPref::Auto,
            bitrate_kbps: 0,
            name: Some(name.to_string()),
            launch: None,
            video_caps: 0,
            audio_channels: 2,
            video_codecs: 0,
            preferred_codec: 0,
            display_hdr: None,
            client_caps: 0,
            max_shard_payload: crate::config::max_shard_payload() as u16,
            audio_rate_hz: 0,
            audio_bits: 0,
            audio_layout: 0,
            video_fit: 0,
        },
        client_label: Some(super::client_label()),
        abr_features: 0,
        preset: None,
        link: Default::default(),
        probe_only: false,
        pyrowave_bpp_x100: 0,
        resume: None,
        suites: vec![crate::crypto::MediaSuite::Aes128Gcm],
        features: Default::default(),
        profile: None,
        access_only: true,
    };
    v2io::send(&mut send, &hello).await?;
    let mut waiting = false;
    loop {
        let (ty, _) = recv.read_frame().await?;
        match ty {
            ServerHello::TYPE => return Ok(()),
            registry::MSG_PENDING if !std::mem::replace(&mut waiting, true) => {
                tracing::info!("the host is waiting for this device to be approved");
            }
            _ => {}
        }
    }
}

fn access_granted(conn: &quinn::Connection) -> bool {
    matches!(
        conn.close_reason(),
        Some(quinn::ConnectionError::ApplicationClosed(ac))
            if u64::from(ac.error_code) == u64::from(crate::reject::ACCESS_GRANTED_CLOSE_CODE)
    )
}
