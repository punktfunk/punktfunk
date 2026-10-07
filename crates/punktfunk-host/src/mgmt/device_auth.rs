//! How a browser authenticates to the management API.
//!
//! Every other client presents a paired client certificate and mTLS proves it on every request.
//! A browser has no client certificate — it pairs with a WebCrypto device key instead
//! (`webtransport::session`) — so it proves the same thing here the same way it does on the
//! control stream: the host issues a nonce, the device signs it, and the signature is checked
//! against the key whose fingerprint the pairing store already holds.
//!
//! **This grants no new authority.** A device that authenticates here reaches exactly
//! [`super::auth::cert_may_access`], the paired-certificate set, and every expiry and grant
//! re-check downstream keys on the same fingerprint it always did. What changes is how the
//! fingerprint is established, not what it is worth.
//!
//! Two steps rather than a signature per request, because a browser makes many: the exchange
//! yields a short-lived bearer token, and the key is used once per session.

use super::shared::*;
use base64::Engine as _;
use rand::Rng;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long an unspent nonce lives. Long enough for a round trip and a signature, short enough
/// that a captured challenge is worthless by the time it could be replayed.
const NONCE_TTL: Duration = Duration::from_secs(60);

/// How long a token lasts. A browser re-runs the exchange rather than holding a long-lived
/// credential; the device key it re-signs with never leaves IndexedDB.
const TOKEN_TTL: Duration = Duration::from_secs(60 * 60);

/// Bound on outstanding challenges, so an unauthenticated peer cannot grow this map without
/// limit. Old entries are swept on every issue; this is the hard ceiling underneath that.
const MAX_NONCES: usize = 256;

/// Outstanding challenges one address may hold. A LAN peer flooding `challenge` evicts only
/// its own nonces, never a browser's exchange in flight from another address.
const MAX_NONCES_PER_IP: usize = 8;

/// Live tokens one device may hold. A device re-runs the exchange rather than keeping a
/// long-lived credential, so a few in flight is plenty and the map cannot grow for an hour.
const MAX_TOKENS_PER_DEVICE: usize = 4;

/// Live nonces and the tokens they turned into.
///
/// Both are in memory and both die with the process, which is the right lifetime: a restart
/// re-mints the host identity's binding anyway, and a browser re-runs an exchange it can do
/// unattended.
#[derive(Default)]
pub(crate) struct DeviceAuth {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// nonce (hex) → when and to whom it was issued. Removed when spent, so a signature is
    /// good once. `None` is a test without a peer address.
    nonces: HashMap<String, (Instant, Option<IpAddr>)>,
    /// token → the device that earned it, and when it lapses.
    tokens: HashMap<String, Session>,
}

struct Session {
    fingerprint: String,
    expires: Instant,
}

impl DeviceAuth {
    /// A fresh challenge. Also the sweep point for both maps — there is no timer, and this is
    /// the only call an unauthenticated peer can make.
    pub(crate) fn challenge(&self, peer: Option<IpAddr>) -> String {
        let mut guard = self.inner.lock().expect("device-auth mutex");
        let now = Instant::now();
        guard
            .nonces
            .retain(|_, (at, _)| now.duration_since(*at) < NONCE_TTL);
        guard.tokens.retain(|_, s| s.expires > now);
        // A flood of challenges must not evict a live token, so only the nonce map is capped.
        // Oldest one out per new one in, never the whole map: clearing it let any unauthenticated
        // caller cancel a real browser's exchange in flight, 256 requests at a time. The
        // per-address cap comes first, so a flood evicts its own nonces before anyone else's.
        let mine = guard.nonces.values().filter(|(_, ip)| *ip == peer).count();
        if mine >= MAX_NONCES_PER_IP {
            evict_oldest(&mut guard.nonces, |(_, ip)| *ip == peer);
        }
        while guard.nonces.len() >= MAX_NONCES {
            if !evict_oldest(&mut guard.nonces, |_| true) {
                break;
            }
        }
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        let nonce = hex::encode(raw);
        guard.nonces.insert(nonce.clone(), (now, peer));
        nonce
    }

    /// Spend a nonce. `false` if it was never issued, has lapsed, or has already been used —
    /// which is what makes a captured signature good exactly once.
    fn spend(&self, nonce: &str) -> bool {
        let mut guard = self.inner.lock().expect("device-auth mutex");
        match guard.nonces.remove(nonce) {
            Some((at, _)) => Instant::now().duration_since(at) < NONCE_TTL,
            None => false,
        }
    }

    /// Mint a token. A device past [`MAX_TOKENS_PER_DEVICE`] loses its oldest one, so a paired
    /// device replaying the exchange cannot grow the map for the life of a token.
    fn issue(&self, fingerprint: String) -> (String, i64) {
        let mut raw = [0u8; 32];
        rand::rng().fill_bytes(&mut raw);
        let token = hex::encode(raw);
        let now = Instant::now();
        let expires = now + TOKEN_TTL;
        let mut guard = self.inner.lock().expect("device-auth mutex");
        guard.tokens.retain(|_, s| s.expires > now);
        let mine = guard
            .tokens
            .values()
            .filter(|s| s.fingerprint == fingerprint)
            .count();
        if mine >= MAX_TOKENS_PER_DEVICE {
            let oldest = guard
                .tokens
                .iter()
                .filter(|(_, s)| s.fingerprint == fingerprint)
                .min_by_key(|(_, s)| s.expires)
                .map(|(t, _)| t.clone());
            if let Some(t) = oldest {
                guard.tokens.remove(&t);
            }
        }
        guard.tokens.insert(
            token.clone(),
            Session {
                fingerprint,
                expires,
            },
        );
        (
            token,
            crate::clock::unix_secs() + TOKEN_TTL.as_secs() as i64,
        )
    }

    /// The device behind a bearer token, if it is live. The caller still re-checks the
    /// fingerprint against the pairing store: a device unpaired after its token was issued must
    /// lose access immediately, not when the token lapses.
    pub(crate) fn device_for(&self, token: &str) -> Option<String> {
        let guard = self.inner.lock().expect("device-auth mutex");
        let session = guard.tokens.get(token)?;
        (session.expires > Instant::now()).then(|| session.fingerprint.clone())
    }
}

/// Drop the oldest nonce matching `pick`. `false` when none matched.
fn evict_oldest(
    nonces: &mut HashMap<String, (Instant, Option<IpAddr>)>,
    pick: impl Fn(&(Instant, Option<IpAddr>)) -> bool,
) -> bool {
    let oldest = nonces
        .iter()
        .filter(|(_, v)| pick(v))
        .min_by_key(|(_, (at, _))| *at)
        .map(|(nonce, _)| nonce.clone());
    match oldest {
        Some(n) => nonces.remove(&n).is_some(),
        None => false,
    }
}

/// `POST /auth/device/challenge` → the nonce to sign.
#[derive(Serialize, ToSchema)]
pub(crate) struct Challenge {
    /// 32 random bytes, lowercase hex. Single-use, and short-lived.
    nonce: String,
    /// Seconds this nonce remains signable.
    expires_in: u64,
}

/// `POST /auth/device/token` — what a paired browser presents.
#[derive(Deserialize, ToSchema)]
pub(crate) struct TokenRequest {
    /// Base64 SPKI of the device's P-256 public key — the same bytes it paired with, whose
    /// SHA-256 the host stored.
    device_key: String,
    /// The nonce from `challenge`.
    nonce: String,
    /// Base64 ECDSA-P256-SHA256 signature, ASN.1 DER, over the same message the control stream
    /// uses: context, the host's identity fingerprint, then the nonce.
    signature: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct TokenGrant {
    /// Present as `Authorization: Bearer <token>`.
    token: String,
    /// Unix seconds.
    expires_at: i64,
    /// The device's own fingerprint, so a client can show which identity it is using.
    fingerprint: String,
}

/// Start a device exchange
#[utoipa::path(
    post,
    path = "/auth/device/challenge",
    tag = "host",
    operation_id = "postDeviceChallenge",
    // Unauthenticated by necessity: this is what a client calls in order to authenticate. The
    // response is a random number and authorises nothing.
    security(()),
    responses((status = OK, description = "A single-use nonce to sign", body = Challenge)),
)]
pub(crate) async fn post_device_challenge(
    State(st): State<Arc<MgmtState>>,
    peer: Option<axum::Extension<crate::gamestream::tls::PeerAddr>>,
) -> Response {
    Json(Challenge {
        nonce: st.device_auth.challenge(peer.map(|p| p.0 .0.ip())),
        expires_in: NONCE_TTL.as_secs(),
    })
    .into_response()
}

/// Exchange a device signature for a token
#[utoipa::path(
    post,
    path = "/auth/device/token",
    tag = "host",
    operation_id = "postDeviceToken",
    security(()),
    request_body = TokenRequest,
    responses(
        (status = OK, description = "A bearer token for the paired-device lane", body = TokenGrant),
        (status = UNAUTHORIZED, description = "Unknown device, stale nonce, or a bad signature", body = ApiError),
    ),
)]
pub(crate) async fn post_device_token(
    State(st): State<Arc<MgmtState>>,
    Json(req): Json<TokenRequest>,
) -> Response {
    // One error for every failure, deliberately. Telling "this key is not paired" apart from
    // "that signature is wrong" would let an unauthenticated caller enumerate paired devices.
    let refuse = || {
        api_error(
            StatusCode::UNAUTHORIZED,
            "the device signature was not accepted",
        )
    };

    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(spki), Ok(sig)) = (b64.decode(&req.device_key), b64.decode(&req.signature)) else {
        return refuse();
    };
    // Spend the nonce before anything else can fail: a wrong signature must still burn its
    // challenge, or a captured nonce could be attacked offline at leisure.
    if !st.device_auth.spend(&req.nonce) {
        return refuse();
    }
    let Ok(nonce) = <[u8; 32] as hex::FromHex>::from_hex(&req.nonce) else {
        return refuse();
    };
    let Some(point) = crate::webtransport::spki_p256_point(&spki) else {
        return refuse();
    };

    // The channel binding is this host's own identity — the fingerprint a browser stored when it
    // paired. Signing it means a signature made for one host cannot be replayed at another.
    let Some(binding) = st.identity_fingerprint else {
        return refuse();
    };
    let msg = punktfunk_core::quic::auth_signed_message(&binding, &nonce);
    if aws_lc_rs::signature::UnparsedPublicKey::new(
        &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1,
        point,
    )
    .verify(&msg, &sig)
    .is_err()
    {
        return refuse();
    }

    let fingerprint = hex::encode(crate::webtransport::sha256(&spki));
    // Paired *and* unexpired, read now rather than trusted from the ceremony: the same
    // `effective` check the certificate lane makes.
    let paired = st.native.as_ref().is_some_and(|n| {
        n.effective(&fingerprint, crate::clock::unix_secs())
            .is_some()
    });
    if !paired {
        return refuse();
    }

    let (token, expires_at) = st.device_auth.issue(fingerprint.clone());
    tracing::info!(device = %fingerprint, "management API: device token issued");
    Json(TokenGrant {
        token,
        expires_at,
        fingerprint,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nonce_is_spendable_once() {
        let auth = DeviceAuth::default();
        let nonce = auth.challenge(None);
        assert!(auth.spend(&nonce), "the nonce it just issued");
        assert!(!auth.spend(&nonce), "and never again");
        assert!(!auth.spend("never issued"));
    }

    /// One address cannot push another's exchange out: its flood evicts its own nonces.
    #[test]
    fn a_flood_from_one_address_evicts_only_itself() {
        let auth = DeviceAuth::default();
        let a: IpAddr = "192.168.1.10".parse().unwrap();
        let b: IpAddr = "192.168.1.11".parse().unwrap();
        let theirs = auth.challenge(Some(b));
        for _ in 0..(MAX_NONCES_PER_IP * 4) {
            auth.challenge(Some(a));
        }
        let held_by_a = auth
            .inner
            .lock()
            .unwrap()
            .nonces
            .values()
            .filter(|(_, ip)| *ip == Some(a))
            .count();
        assert_eq!(held_by_a, MAX_NONCES_PER_IP);
        assert!(auth.spend(&theirs), "the other address's nonce survived");
    }

    #[test]
    fn a_device_holds_a_bounded_number_of_tokens() {
        let auth = DeviceAuth::default();
        let (first, _) = auth.issue("abc".into());
        for _ in 0..MAX_TOKENS_PER_DEVICE {
            auth.issue("abc".into());
        }
        assert_eq!(auth.device_for(&first), None, "the oldest went first");
        assert_eq!(
            auth.inner.lock().unwrap().tokens.len(),
            MAX_TOKENS_PER_DEVICE
        );
    }

    #[test]
    fn a_token_names_its_device_until_it_lapses() {
        let auth = DeviceAuth::default();
        let (token, _) = auth.issue("abc".into());
        assert_eq!(auth.device_for(&token).as_deref(), Some("abc"));
        assert_eq!(auth.device_for("not a token"), None);
    }

    /// An unauthenticated peer can only call `challenge`, so that is the call that must not grow
    /// the map without bound.
    #[test]
    fn challenges_cannot_grow_without_bound() {
        let auth = DeviceAuth::default();
        let (token, _) = auth.issue("abc".into());
        for i in 0..(MAX_NONCES * 2) {
            let ip = std::net::Ipv4Addr::new(10, 1, (i / 256) as u8, (i % 256) as u8);
            let _ = auth.challenge(Some(ip.into()));
        }
        assert!(
            auth.inner.lock().unwrap().nonces.len() <= MAX_NONCES,
            "capped"
        );
        assert!(
            auth.device_for(&token).is_some(),
            "a flood of challenges must not evict a live session"
        );
    }

    /// One in, one out. Wiping the map instead let an unauthenticated caller cancel whatever
    /// exchange a real browser had in flight, which is a denial nobody had to authenticate for.
    #[test]
    fn the_cap_evicts_one_nonce_not_the_whole_map() {
        let auth = DeviceAuth::default();
        // Distinct addresses: the per-address budget would otherwise stop one peer short.
        let peer = |i: usize| -> Option<IpAddr> {
            Some(std::net::Ipv4Addr::new(10, 0, (i / 256) as u8, (i % 256) as u8).into())
        };
        for i in 0..MAX_NONCES {
            let _ = auth.challenge(peer(i));
        }
        assert_eq!(
            auth.inner.lock().unwrap().nonces.len(),
            MAX_NONCES,
            "the cap is reached, not overshot"
        );
        let _ = auth.challenge(peer(MAX_NONCES));
        assert_eq!(
            auth.inner.lock().unwrap().nonces.len(),
            MAX_NONCES,
            "still full: a `clear()` here would have left 1"
        );
    }
}
