//! The client-side PIN pairing ceremony (SPAKE2): `NativeClient::pair`.

use super::worker::reject_from_close;
use super::{dial_addr, NativeClient};
use crate::error::{PunktfunkError, Result};
use crate::quic::endpoint;
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
}
