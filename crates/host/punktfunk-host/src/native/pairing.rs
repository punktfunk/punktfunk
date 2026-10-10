//! Host SPAKE2 pairing.
//!
//! `serve_session` dispatches a first-message `PairRequest` here after resolving
//! the armed PIN. This is SPAKE2 role B: the PIN is consumed before the host
//! challenge goes out so an attacker gets one online guess, then the client
//! fingerprint is persisted on success.
//!
//! **The caller supplies both identities.** The ceremony binds the SPAKE2 key to them and does
//! not care where they came from — which is what lets a carrier without mTLS use it. On the
//! native plane they are the two certificate fingerprints. A browser presents no certificate, so
//! `client_fp` has to come from a key it holds and `host_fp` from the certificate hash it pinned
//! to connect (`design/web-client-implementation-plan.md`, Phase 3). Resolving them here, from a
//! connection, is what would have forced a second ceremony.
//!
//! A lapsed or re-armed window mints nothing (`consume_window`; tests at the
//! foot). Protocol: `punktfunk_core::quic::pake`. Access stored via
//! `crate::native_pairing`.

use super::*;
use crate::native_pairing::sanitize_device_name;
use punktfunk_core::quic::v2::io::{self as v2io, FrameReader};
use punktfunk_core::quic::{pkf1, PairChallenge, PairProof, PairResult};

/// 60 s: a person reads the PIN off the host and types it. Session handshake is machine-speed.
const PAIRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A pairing connection's stream and how it frames the ceremony: `punktfunk/2` frames, or the
/// `pkf1` framing a client from before `punktfunk/2` still pairs in.
pub(crate) enum PairWire<W, R> {
    V2 { send: W, recv: FrameReader<R> },
    Pkf1 { send: W, recv: R },
}

impl<W, R> PairWire<W, R>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    async fn send_challenge(&mut self, c: &PairChallenge) -> std::io::Result<()> {
        match self {
            PairWire::V2 { send, .. } => v2io::send(send, c).await,
            PairWire::Pkf1 { send, .. } => pkf1::write(send, &c.encode_pkf1()).await,
        }
    }

    async fn recv_proof(&mut self) -> Result<PairProof> {
        let bad = |e| anyhow!("PairProof decode: {e:?}");
        match self {
            PairWire::V2 { recv, .. } => {
                let (ty, body) = recv.read_frame().await?;
                punktfunk_core::quic::v2::msg::decode::<PairProof>(ty, &body).map_err(bad)
            }
            PairWire::Pkf1 { recv, .. } => {
                PairProof::decode_pkf1(&pkf1::read(recv).await?).map_err(bad)
            }
        }
    }

    async fn send_result(&mut self, r: &PairResult) -> std::io::Result<()> {
        match self {
            PairWire::V2 { send, .. } => v2io::send(send, r).await,
            PairWire::Pkf1 { send, .. } => pkf1::write(send, &r.encode_pkf1()).await,
        }
    }

    /// The generic write side's "no more data": quinn's `finish` under another name.
    async fn shutdown(&mut self) {
        let (PairWire::V2 { send, .. } | PairWire::Pkf1 { send, .. }) = self;
        let _ = tokio::io::AsyncWriteExt::shutdown(send).await;
    }
}

/// Host SPAKE2 (role B). Consumes the armed PIN before the challenge write, so a
/// client that stalls or resets that write still spends its one online guess.
/// Both stream writes time out.
///
/// `client_fp` and `host_fp` are the SPAKE2 identities: whatever the caller binds this pairing
/// to. Getting them wrong does not fail loudly — it yields a different key and a MAC mismatch,
/// which is indistinguishable from a wrong PIN, so the caller owns that decision deliberately.
// Eight orthogonal inputs, not a struct waiting to happen: the two identities are the whole
// point of the signature, and grouping them would hide the decision the caller has to make.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pair_ceremony<W, R>(
    conn: &super::link::SessionLink,
    mut wire: PairWire<W, R>,
    req: PairRequest,
    client_fp: &[u8; 32],
    host_fp: &[u8; 32],
    np: &NativePairing,
    pin: &str,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    R: tokio::io::AsyncRead + Unpin,
{
    use punktfunk_core::quic::pake;
    let client_fp = *client_fp;
    let client_fp_hex = hex::encode(client_fp);
    // Unpaired wire name: scrub once here and log only that value.
    // ANSI/C0 and bidi otherwise reach the operator terminal and the journal.
    let name = sanitize_device_name(&req.name, &client_fp_hex);

    tracing::info!(
        name = %name,
        client = %client_fp_hex,
        "PAIRING REQUEST — verifying against the armed PIN"
    );

    // `false` = SPAKE2 role B. Identities are the cert we presented and the cert we received.
    let (pake, spake_b) = pake::start(false, pin, &client_fp, host_fp);
    let confirms = pake.finish(&req.spake_a)?; // Err only on a malformed peer message

    // Burn the PIN before the challenge leaves: a client can read most of it and then stall
    // or reset the write, and the guess is spent either way. Garbage `finish` never got here.
    let access = consume_window(np, pin)?;

    // Timeout: this write waits on the client's stream window.
    let challenge = PairChallenge {
        spake_b,
        confirm: confirms.host,
    };
    tokio::time::timeout(PAIRING_TIMEOUT, wire.send_challenge(&challenge))
        .await
        .map_err(|_| anyhow!("pairing timed out sending the challenge"))??;

    let proof = tokio::time::timeout(PAIRING_TIMEOUT, wire.recv_proof())
        .await
        .map_err(|_| anyhow!("pairing timed out waiting for the client's confirmation"))??;

    // Wrong PIN or split certs: different SPAKE2 key; MAC mismatch. No offline search.
    let verified = pake::verify(&confirms.client, &proof.confirm);
    // A pin the store did not keep is no pairing: the client would knock as unpaired next time.
    let ok = verified
        && match np.add_with_access(&req.name, &hex::encode(client_fp), access) {
            Ok(()) => {
                tracing::info!(name = %name, "pairing complete — client trusted");
                true
            }
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "paired clients not saved");
                false
            }
        };
    if !verified {
        tracing::warn!(name = %name, "pairing rejected (wrong PIN) — fingerprint not stored");
    }
    // Same flow-control trap as the challenge write.
    tokio::time::timeout(PAIRING_TIMEOUT, wire.send_result(&PairResult { ok }))
        .await
        .map_err(|_| anyhow!("pairing timed out sending the result"))??;
    wire.shutdown().await;
    // 5 s: wait for the client to ACK PairResult before we close. A vanished
    // peer must not occupy the sequential host.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), conn.closed()).await;
    conn.close(0, b"pairing done");
    anyhow::ensure!(verified, "pairing rejected (wrong PIN)");
    anyhow::ensure!(ok, "pairing not saved");
    Ok(())
}

/// Snapshot access, then prove this is still the PIN we started with, then disarm.
/// Reverse order fails open: [`NativePairing::add_with_access`] treats a missing
/// choice as full/permanent. `Err` if the window lapsed or was re-armed — leave
/// the live window untouched so a stalling client cannot wipe the next one.
fn consume_window(np: &NativePairing, pin: &str) -> Result<Option<crate::native_pairing::Access>> {
    let access = np.armed_access();
    anyhow::ensure!(
        np.current_pin().as_deref() == Some(pin),
        "the pairing window lapsed while the ceremony was running"
    );
    np.disarm();
    Ok(access)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_pairing::Access;
    use std::time::Duration;

    fn temp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pf-native-ceremony-{tag}-{}.json",
            std::process::id()
        ))
    }

    fn controller_4h() -> Access {
        Access {
            grants: punktfunk_core::quic::GRANT_PRESET_CONTROLLER_ONLY,
            expires_unix: Some(4 * 3600),
            until_disconnect: false,
        }
    }

    /// Lapsed window mints nothing. Without the PIN re-check, expiry reads as
    /// no choice and `add_with_access(.., None)` is full/permanent.
    #[test]
    fn expired_window_mints_no_grant() {
        let p = temp("expired");
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), None, false).unwrap();

        let pin = np.arm_for(Duration::from_secs(60), None, Some(controller_4h()));
        assert_eq!(consume_window(&np, &pin).unwrap(), Some(controller_4h()));
        assert!(np.current_pin().is_none(), "single-use: window consumed");

        // Duration::ZERO is already past when read: refuse, do not mint full/permanent.
        let pin = np.arm_for(Duration::ZERO, None, Some(controller_4h()));
        assert!(consume_window(&np, &pin).is_err());

        // Stale PIN after re-arm: refuse and do not disarm the live window.
        let stale = np.arm_for(Duration::ZERO, None, Some(controller_4h()));
        // 4-digit PIN: 1 in 10_000 collision; re-arm until the values differ.
        let mut fresh = np.arm_for(Duration::from_secs(60), None, Some(controller_4h()));
        while fresh == stale {
            fresh = np.arm_for(Duration::from_secs(60), None, Some(controller_4h()));
        }
        assert!(consume_window(&np, &stale).is_err());
        assert_eq!(np.current_pin().as_deref(), Some(fresh.as_str()));

        let _ = std::fs::remove_file(&p);
    }

    /// CLI `--allow-pairing` has no choice and no expiry: `None` is the legitimate
    /// full/permanent default, not a lapse, and must still pair.
    #[test]
    fn choiceless_window_still_pairs() {
        let p = temp("choiceless");
        let _ = std::fs::remove_file(&p);
        let np = NativePairing::load_with(Some(p.clone()), Some("4321".into()), true).unwrap();
        assert_eq!(consume_window(&np, "4321").unwrap(), None);
        let _ = std::fs::remove_file(&p);
    }
}
