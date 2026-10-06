//! Pairing-ceremony control messages: PairRequest / Challenge / Proof / Result.
//!
//! A client may open the control stream with [`PairRequest`] instead of Hello.
//! The host shows a short PIN out-of-band; the user types it on the client.
//! Trust is [`super::pake`] (SPAKE2), not a hash of the PIN: an active MITM learns
//! only whether one guess was right — no transcript for offline search.
//! Both fingerprints are SPAKE2 identities, so the confirmation MACs agree only
//! when both sides saw the same two. After mutual confirmation the host persists
//! the client's fingerprint and the client pins the host's.
//!
//! A client with a certificate uses its fingerprint and stops there — the transport re-proves
//! it on every later connection. A browser has no client certificate, so it sends
//! [`PairRequest::device_key`] instead and its fingerprint is the SHA-256 of that. Nothing on
//! the transport re-proves it afterwards, which is what [`AuthChallenge`] is for: once per
//! session, the client signs a host nonce bound to the channel.

use super::*;
use crate::error::{PunktfunkError, Result};

pub const MSG_PAIR_REQUEST: u8 = 0x10;
pub const MSG_PAIR_CHALLENGE: u8 = 0x11;
pub const MSG_PAIR_PROOF: u8 = 0x12;
pub const MSG_PAIR_RESULT: u8 = 0x13;
/// `host → client`, browser plane: why the host is about to close. The native plane says this
/// with the QUIC close code and reason; a browser cannot read those in every engine (WebKit
/// hands back a bare error), so the same code and text go on the control stream first.
pub const MSG_REFUSED: u8 = 0x16;

/// Domain separation for [`AuthResponse::signature`]. A device key may sign other things; a
/// signature over a session nonce must not be replayable as one over any of them.
pub const AUTH_SIG_CONTEXT: &[u8] = b"punktfunk-device-auth-v1:";

/// The exact bytes an [`AuthResponse`] signs. Both sides call this, so neither can hold a
/// different opinion of what was signed.
///
/// `binding` names the channel — for the browser plane, the raw SHA-256 of the transport
/// certificate both ends already agree on. Without it a signature captured on one connection
/// replays on another; with it, it is worthless anywhere else.
pub fn auth_signed_message(binding: &[u8; 32], nonce: &[u8; 32]) -> Vec<u8> {
    let mut m = Vec::with_capacity(AUTH_SIG_CONTEXT.len() + 64);
    m.extend_from_slice(AUTH_SIG_CONTEXT);
    m.extend_from_slice(binding);
    m.extend_from_slice(nonce);
    m
}

/// `client → host`: begin pairing. `name` is the host-stored label (≤64 bytes UTF-8);
/// `spake_a` is the client's SPAKE2 message (see [`super::pake::start`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequest {
    pub name: String,
    pub spake_a: Vec<u8>,
    /// SPKI DER of the client's device key, or empty when the client has a certificate instead.
    /// The host's identity for the ceremony is then the SHA-256 of these bytes, which is also
    /// what it stores — so this must be the key the client can later sign with, not a copy.
    ///
    /// Trailing and optional: a host that predates it rejects a message carrying one, and only
    /// carriers without mTLS send it. Those carriers are newer than the field.
    pub device_key: Vec<u8>,
}

/// `host → client`: prove you still hold the device key you paired with.
///
/// Only sent on a carrier with no client certificate. The nonce is fresh per connection, so a
/// captured response does not open a second one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthChallenge {
    pub nonce: [u8; 32],
}

/// `client → host`: the device key and its signature over [`auth_signed_message`].
///
/// The host recomputes SHA-256 of `device_key` and looks that up in its pairing store, so a
/// valid signature by an unpaired key proves possession of nothing that matters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthResponse {
    /// SPKI DER, the same bytes sent in [`PairRequest::device_key`].
    pub device_key: Vec<u8>,
    /// ECDSA P-256 / SHA-256, ASN.1 DER — what WebCrypto's `ECDSA` produces once converted
    /// from its raw `r || s`.
    pub signature: Vec<u8>,
}

/// `host → client`: host SPAKE2 message + key-confirmation MAC. The client
/// finishes SPAKE2, verifies `confirm` (same key ⇒ same PIN and certs), then
/// sends its own confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairChallenge {
    pub spake_b: Vec<u8>,
    pub confirm: [u8; 32],
}

/// `client → host`: client's key-confirmation MAC (one proof attempt).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairProof {
    pub confirm: [u8; 32],
}

/// `host → client`: ceremony outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PairResult {
    pub ok: bool,
}

/// `host → client`: the close that follows, in words. `code` is one of
/// [`crate::reject`]'s close codes; `reason` is the sentence the native plane would have put in
/// the close frame, capped so a hostile host cannot make a client render pages of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub code: u32,
    pub reason: String,
}

/// Longest `reason` on the wire. Enough for one host sentence.
pub const REFUSED_REASON_MAX: usize = 256;

fn put_bytes(b: &mut Vec<u8>, x: &[u8]) {
    b.extend_from_slice(&(x.len() as u16).to_le_bytes());
    b.extend_from_slice(x);
}

fn get_bytes(b: &[u8], off: usize) -> Result<(&[u8], usize)> {
    if off + 2 > b.len() {
        return Err(PunktfunkError::InvalidArg("truncated field"));
    }
    let n = u16::from_le_bytes([b[off], b[off + 1]]) as usize;
    let start = off + 2;
    if start + n > b.len() {
        return Err(PunktfunkError::InvalidArg("field overruns message"));
    }
    Ok((&b[start..start + n], start + n))
}

impl PairRequest {
    pub fn encode_pkf1(&self) -> Vec<u8> {
        // Same cap as Hello: truncate on a char boundary. A mid-sequence cut
        // puts invalid UTF-8 on the wire and the host stores U+FFFD forever.
        let name = super::handshake::truncate_to(&self.name, HELLO_NAME_MAX).as_bytes();
        let n = name.len();
        let mut b = Vec::with_capacity(8 + n + self.spake_a.len());
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_REQUEST);
        b.push(n as u8);
        b.extend_from_slice(name);
        put_bytes(&mut b, &self.spake_a);
        // Omitted entirely when absent, so a certificate client's bytes are what they always were.
        if !self.device_key.is_empty() {
            put_bytes(&mut b, &self.device_key);
        }
        b
    }

    pub fn decode_pkf1(b: &[u8]) -> Result<PairRequest> {
        if b.len() < 6 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_REQUEST {
            return Err(PunktfunkError::InvalidArg("bad PairRequest"));
        }
        let n = b[5] as usize;
        if n > 64 || b.len() < 6 + n {
            return Err(PunktfunkError::InvalidArg("bad PairRequest name"));
        }
        let name = String::from_utf8_lossy(&b[6..6 + n]).into_owned();
        let (spake_a, end) = get_bytes(b, 6 + n)?;
        let (device_key, end) = if end == b.len() {
            (&b[..0], end)
        } else {
            get_bytes(b, end)?
        };
        if end != b.len() {
            return Err(PunktfunkError::InvalidArg("trailing bytes"));
        }
        Ok(PairRequest {
            name,
            spake_a: spake_a.to_vec(),
            device_key: device_key.to_vec(),
        })
    }
}

impl PairChallenge {
    pub fn encode_pkf1(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(7 + self.spake_b.len() + 32);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_CHALLENGE);
        put_bytes(&mut b, &self.spake_b);
        b.extend_from_slice(&self.confirm);
        b
    }

    pub fn decode_pkf1(b: &[u8]) -> Result<PairChallenge> {
        if b.len() < 5 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_CHALLENGE {
            return Err(PunktfunkError::InvalidArg("bad PairChallenge"));
        }
        let (spake_b, end) = get_bytes(b, 5)?;
        if end + 32 != b.len() {
            return Err(PunktfunkError::InvalidArg("bad PairChallenge confirm"));
        }
        let mut confirm = [0u8; 32];
        confirm.copy_from_slice(&b[end..end + 32]);
        Ok(PairChallenge {
            spake_b: spake_b.to_vec(),
            confirm,
        })
    }
}

impl PairProof {
    pub fn encode_pkf1(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(37);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_PROOF);
        b.extend_from_slice(&self.confirm);
        b
    }

    pub fn decode_pkf1(b: &[u8]) -> Result<PairProof> {
        if b.len() != 37 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_PROOF {
            return Err(PunktfunkError::InvalidArg("bad PairProof"));
        }
        let mut confirm = [0u8; 32];
        confirm.copy_from_slice(&b[5..37]);
        Ok(PairProof { confirm })
    }
}

impl PairResult {
    pub fn encode_pkf1(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(6);
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_PAIR_RESULT);
        b.push(self.ok as u8);
        b
    }

    pub fn decode_pkf1(b: &[u8]) -> Result<PairResult> {
        if b.len() != 6 || &b[0..4] != CTL_MAGIC || b[4] != MSG_PAIR_RESULT {
            return Err(PunktfunkError::InvalidArg("bad PairResult"));
        }
        Ok(PairResult { ok: b[5] != 0 })
    }
}

impl Refused {
    pub fn encode_pkf1(&self) -> Vec<u8> {
        let reason = super::handshake::truncate_to(&self.reason, REFUSED_REASON_MAX).as_bytes();
        let mut b = Vec::with_capacity(11 + reason.len());
        b.extend_from_slice(CTL_MAGIC);
        b.push(MSG_REFUSED);
        b.extend_from_slice(&self.code.to_le_bytes());
        put_bytes(&mut b, reason);
        b
    }

    pub fn decode_pkf1(b: &[u8]) -> Result<Refused> {
        if b.len() < 9 || &b[0..4] != CTL_MAGIC || b[4] != MSG_REFUSED {
            return Err(PunktfunkError::InvalidArg("bad Refused"));
        }
        let code = u32::from_le_bytes([b[5], b[6], b[7], b[8]]);
        let (reason, end) = get_bytes(b, 9)?;
        if end != b.len() {
            return Err(PunktfunkError::InvalidArg("trailing bytes"));
        }
        let reason = core::str::from_utf8(reason)
            .map_err(|_| PunktfunkError::InvalidArg("Refused reason is not UTF-8"))?;
        Ok(Refused {
            code,
            reason: reason.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::quic::*;

    #[test]
    fn refused_roundtrips_and_caps_the_reason() {
        let r = Refused {
            code: crate::reject::SETUP_FAILED_CLOSE_CODE,
            reason: "no usable compositor".into(),
        };
        assert_eq!(Refused::decode_pkf1(&r.encode_pkf1()).unwrap(), r);
        let long = Refused {
            code: 1,
            reason: "é".repeat(400),
        };
        let back = Refused::decode_pkf1(&long.encode_pkf1()).unwrap();
        assert!(back.reason.len() <= REFUSED_REASON_MAX, "capped");
        assert!(
            back.reason.chars().all(|c| c == 'é'),
            "cut on a char boundary"
        );
        assert!(Refused::decode_pkf1(&PairResult { ok: true }.encode_pkf1()).is_err());
    }

    #[test]
    fn pair_messages_roundtrip() {
        let pr = PairRequest {
            name: "Enrico's Mac".into(),
            spake_a: vec![1, 2, 3, 4, 5],
            device_key: Vec::new(),
        };
        assert_eq!(PairRequest::decode_pkf1(&pr.encode_pkf1()).unwrap(), pr);
        let keyed = PairRequest {
            device_key: vec![0x30, 0x59, 0x30, 0x13],
            ..pr.clone()
        };
        assert_eq!(
            PairRequest::decode_pkf1(&keyed.encode_pkf1()).unwrap(),
            keyed
        );
        let pc = PairChallenge {
            spake_b: vec![9; 33],
            confirm: [7u8; 32],
        };
        assert_eq!(PairChallenge::decode_pkf1(&pc.encode_pkf1()).unwrap(), pc);
        let pp = PairProof { confirm: [3u8; 32] };
        assert_eq!(PairProof::decode_pkf1(&pp.encode_pkf1()).unwrap(), pp);
        for ok in [true, false] {
            assert_eq!(
                PairResult::decode_pkf1(&PairResult { ok }.encode_pkf1())
                    .unwrap()
                    .ok,
                ok
            );
        }
        let mut bad = pp.encode_pkf1();
        bad.push(0);
        assert!(PairProof::decode_pkf1(&bad).is_err());
    }

    #[test]
    fn pair_request_name_cap_respects_char_boundaries() {
        // Drop a straddling multi-byte char whole (Hello's rule), never split
        // into invalid UTF-8 that the host would store as U+FFFD.
        let pr = PairRequest {
            name: format!("{}\u{00fc}", "x".repeat(HELLO_NAME_MAX - 1)),
            spake_a: vec![1, 2, 3],
            device_key: Vec::new(),
        };
        let dec = PairRequest::decode_pkf1(&pr.encode_pkf1()).unwrap();
        assert!(dec.name.len() <= HELLO_NAME_MAX && dec.name.starts_with('x'));
        assert!(
            !dec.name.contains('\u{FFFD}'),
            "name must never be split mid-char on the wire"
        );
    }

    /// A certificate client must put the same bytes on the wire it always did, or every host
    /// older than the field rejects it.
    #[test]
    fn a_device_key_free_request_is_byte_identical_to_the_old_shape() {
        let pr = PairRequest {
            name: "old client".into(),
            spake_a: vec![7; 33],
            device_key: Vec::new(),
        };
        let bytes = pr.encode_pkf1();
        let mut expected = Vec::new();
        expected.extend_from_slice(CTL_MAGIC);
        expected.push(MSG_PAIR_REQUEST);
        expected.push(10);
        expected.extend_from_slice(b"old client");
        expected.extend_from_slice(&33u16.to_le_bytes());
        expected.extend_from_slice(&[7; 33]);
        assert_eq!(bytes, expected, "the field is absent, not empty-encoded");
    }

    /// The signature has to name the connection it was made on, or one captured response opens
    /// every later session.
    #[test]
    fn the_signed_message_binds_the_channel_and_the_nonce() {
        let m = auth_signed_message(&[1u8; 32], &[2u8; 32]);
        assert!(m.starts_with(AUTH_SIG_CONTEXT), "domain separated");
        assert_ne!(m, auth_signed_message(&[9u8; 32], &[2u8; 32]));
        assert_ne!(m, auth_signed_message(&[1u8; 32], &[9u8; 32]));
        // Same length either way, so the two halves cannot be slid past each other.
        assert_eq!(m.len(), AUTH_SIG_CONTEXT.len() + 64);
    }
}
